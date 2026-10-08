//! ct-store: append-only, self-contained AI change-reason log (`<git_dir>/codetrail/log.ct`).
//!
//! Layout (PLAN §9.3):
//! * file header: `"CTLG"` + version `u16` (LE)
//! * frame: `"CTF1"` | len `u32` (<= 1 MiB) | seq `u64` | crc32(payload) `u32` | postcard payload
//!   - `seq` is the byte offset of the frame (unique + monotonic under the append lock).
//!   - the whole frame is written with ONE `write_all` while holding `flock(LOCK_EX)`; no fsync
//!     (a crash may lose the newest frames; this is deliberate and documented).
//! * corrupt frames are skipped by resyncing on the next magic; the skip count is exposed in [`Stats`].
//! * `index.ct` caches the in-memory index (record offsets, not contents; records are read back on demand) keyed by (log length, log mtime, version) and is
//!   replaced atomically (tmp + rename). A log with a different version is opened read-only.
//!
//! The data is LOCAL ONLY: deleting `.git` deletes the log. Use [`Store::export_json`] for backups.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use parking_lot::Mutex;
use std::sync::Arc;
use xxhash_rust::xxh3::xxh3_64;

pub type Oid = String;

pub const FORMAT_VERSION: u16 = 1;
const LOG_MAGIC: &[u8; 4] = b"CTLG";
const FRAME_MAGIC: &[u8; 4] = b"CTF1";
const IDX_MAGIC: &[u8; 4] = b"CTI3";
const LOG_HEADER_LEN: usize = 6;
const FRAME_HEADER_LEN: usize = 20;
pub const MAX_FRAME_LEN: usize = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "codetrail log is format version {found} but this build supports {supported}; \
         opened read-only - upgrade codetrail"
    )]
    ReadOnlyVersion { found: u16, supported: u16 },
    #[error("log.ct has an unrecognised header; opened read-only (move or repair the file)")]
    ReadOnlyBadHeader,
    #[error("record too large ({0} bytes, max {MAX_FRAME_LEN})")]
    TooLarge(usize),
    #[error("encode: {0}")]
    Encode(String),
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Omp,
    Codex,
    Other(u16),
}

impl Agent {
    pub fn name(&self) -> String {
        match self {
            Agent::Claude => "claude".into(),
            Agent::Omp => "omp".into(),
            Agent::Codex => "codex".into(),
            Agent::Other(n) => format!("other-{n}"),
        }
    }
    pub fn parse(s: &str) -> Agent {
        match s.to_ascii_lowercase().as_str() {
            "claude" => Agent::Claude,
            "omp" => Agent::Omp,
            "codex" => Agent::Codex,
            o => Agent::Other((xxh3_64(o.as_bytes()) & 0xffff) as u16),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Edit,
    Unattributed,
}

/// One contiguous added block of an edit, as numbered in the post-edit file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub new_start: u32,
    pub new_len: u32,
    /// [`fingerprint_lines`] of the added lines.
    pub fingerprint: u64,
    /// Ordinal among spans with an identical fingerprint inside the same record (position order).
    pub occurrence: u16,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TranscriptPtr {
    pub path: String,
    pub offset: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub id: u128,
    pub ts_ms: i64,
    pub agent: Agent,
    pub session: String,
    pub kind: Kind,
    pub tool: String,
    pub tool_use_id: String,
    pub head_at_edit: Oid,
    /// Repo-relative, `/`-separated.
    pub path: String,
    pub pre_blob: Option<Oid>,
    /// `None` when the file was deleted.
    pub post_blob: Option<Oid>,
    pub spans: Vec<Span>,
    /// zstd(UTF-8) of the latest note; empty until a Note frame arrives (merged at load time).
    pub reason: Vec<u8>,
    pub transcript: Option<TranscriptPtr>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct NoteFrame {
    pub record_id: u128,
    pub ts_ms: i64,
    pub agent: Agent,
    pub session: String,
    pub reason: Vec<u8>,
}

/// Wire enum - never reorder variants (postcard encodes the index).
#[derive(Serialize, Deserialize, Clone, Debug)]
enum Frame {
    Edit(Record),
    Note(NoteFrame),
    Unattributed(Record),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    High,
    Medium,
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub log_len: u64,
    /// Valid frames read.
    pub frames: u64,
    pub records: u64,
    pub notes: u64,
    /// Notes whose record id is not in the log.
    pub orphan_notes: u64,
    /// Corrupt regions skipped while scanning (shown in the status bar).
    pub skipped_frames: u64,
    pub read_only: bool,
    pub from_cache: bool,
}

// ---------------------------------------------------------------- helpers

pub fn encode_reason(text: &str) -> Vec<u8> {
    if text.is_empty() {
        return Vec::new();
    }
    zstd::encode_all(text.as_bytes(), 3).unwrap_or_default()
}

pub fn decode_reason_bytes(b: &[u8]) -> String {
    if b.is_empty() {
        return String::new();
    }
    match zstd::decode_all(b) {
        Ok(v) => String::from_utf8_lossy(&v).into_owned(),
        Err(_) => String::new(),
    }
}

pub fn decode_reason(r: &Record) -> String {
    decode_reason_bytes(&r.reason)
}

const B: u64 = 0x9E37_79B9_7F4A_7C15;

fn line_hash(l: &str) -> u64 {
    xxh3_64(l.trim_end().as_bytes())
}

/// Order-dependent fingerprint of added lines (each normalised by trimming trailing whitespace).
/// Polynomial form so windows of a larger hunk can be hashed with a rolling prefix.
pub fn fingerprint_lines<S: AsRef<str>>(lines: &[S]) -> u64 {
    lines
        .iter()
        .fold(0u64, |a, l| a.wrapping_mul(B).wrapping_add(line_hash(l.as_ref())))
}

pub fn is_blank_span<S: AsRef<str>>(lines: &[S]) -> bool {
    lines.iter().all(|l| l.as_ref().trim().is_empty())
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Time-ordered id: `ts_ms << 64 | 64 random-ish bits`.
pub fn new_id(ts_ms: i64) -> u128 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut seed = Vec::with_capacity(24);
    seed.extend_from_slice(&nanos.to_le_bytes());
    seed.extend_from_slice(&u64::from(std::process::id()).to_le_bytes());
    seed.extend_from_slice(&CTR.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    ((ts_ms.max(0) as u128) << 64) | u128::from(xxh3_64(&seed))
}

pub fn fmt_id(id: u128) -> String {
    format!("{id:032x}")
}

pub fn parse_id(s: &str) -> Option<u128> {
    u128::from_str_radix(s, 16).ok()
}

// ---------------------------------------------------------------- scanning

/// Sliding window over a log reader: memory is bounded by one frame (<= `MAX_FRAME_LEN`) + one chunk,
/// never by the log size.
struct Window<R> {
    r: R,
    buf: Vec<u8>,
    start: usize,
    /// File offset of `buf[0]`.
    base: u64,
    eof: bool,
}

const CHUNK: usize = 64 * 1024;

impl<R: Read> Window<R> {
    fn new(r: R, base: u64) -> Self {
        Window { r, buf: Vec::new(), start: 0, base, eof: false }
    }

    fn offset(&self) -> u64 {
        self.base + self.start as u64
    }

    fn avail(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    /// Ensure at least `n` unread bytes unless EOF; returns whether they are there.
    fn have(&mut self, n: usize) -> bool {
        while self.buf.len() - self.start < n && !self.eof {
            if self.start > 0 {
                self.buf.drain(..self.start);
                self.base += self.start as u64;
                self.start = 0;
            }
            let old = self.buf.len();
            self.buf.resize(old + CHUNK, 0);
            match self.r.read(&mut self.buf[old..]) {
                Ok(0) | Err(_) => {
                    self.buf.truncate(old);
                    self.eof = true;
                }
                Ok(k) => self.buf.truncate(old + k),
            }
        }
        self.buf.len() - self.start >= n
    }

    /// Move to the next frame magic strictly after the cursor. `false` = none before EOF (cursor at end).
    fn seek_magic(&mut self) -> bool {
        let mut from = self.start + 1;
        loop {
            if let Some(p) = self.buf.get(from..).and_then(|s| s.windows(4).position(|w| w == FRAME_MAGIC)) {
                self.start = from + p;
                return true;
            }
            if self.eof {
                self.start = self.buf.len();
                return false;
            }
            // keep only a possible magic prefix (last 3 bytes), then read more
            self.start = self.buf.len().saturating_sub(3).max(self.start);
            let need = self.buf.len() - self.start + 1;
            self.have(need);
            from = self.start;
        }
    }
}

struct ScanEnd {
    skipped: u64,
    /// Scan consumed the stream exactly (no trailing garbage / torn frame).
    clean: bool,
    /// Offset after the last byte consumed.
    end: u64,
}

/// Stream frames from `r` (positioned at file offset `base`), calling `on_frame(frame_offset, frame)`.
/// Corrupt regions are skipped by resyncing on the next magic.
fn scan_stream<R: Read>(r: R, base: u64, mut on_frame: impl FnMut(u64, Frame)) -> ScanEnd {
    let mut w = Window::new(r, base);
    let mut skipped = 0u64;
    let mut clean = true;
    while w.have(1) {
        let off = w.offset();
        let parsed = (|| {
            if !w.have(FRAME_HEADER_LEN) {
                return None;
            }
            let h = w.avail();
            if &h[0..4] != FRAME_MAGIC {
                return None;
            }
            let len = u32::from_le_bytes(h[4..8].try_into().ok()?) as usize;
            if len > MAX_FRAME_LEN {
                return None;
            }
            let crc = u32::from_le_bytes(h[16..20].try_into().ok()?);
            if !w.have(FRAME_HEADER_LEN + len) {
                return None;
            }
            let payload = &w.avail()[FRAME_HEADER_LEN..FRAME_HEADER_LEN + len];
            if crc32fast::hash(payload) != crc {
                return None;
            }
            let f: Frame = postcard::from_bytes(payload).ok()?;
            Some((f, FRAME_HEADER_LEN + len))
        })();
        match parsed {
            Some((f, adv)) => {
                on_frame(off, f);
                w.start += adv;
            }
            None => {
                skipped += 1;
                clean = false;
                if !w.seek_magic() {
                    break;
                }
            }
        }
    }
    ScanEnd { skipped, clean: clean && !w.have(1), end: w.offset() }
}

// ---------------------------------------------------------------- index

/// One record in the in-memory index: where it lives in the log, not its content. Records are read
/// back from `off` on demand, so memory is O(records x 32 B) instead of O(log size).
#[derive(Clone, Copy)]
struct Entry {
    id: u128,
    path: u32,
    /// Offset of the record's frame.
    off: u64,
}

#[derive(Clone, Default)]
struct IndexBody {
    /// File order.
    entries: Vec<Entry>,
    paths: Vec<String>,
    /// (record id, offset of the note frame) in file order; the latest per record wins.
    notes: Vec<(u128, u64)>,
    frames: u64,
    skipped: u64,
    /// Byte offset the body is complete up to when the scan ended cleanly (0 = not clean: always rescan).
    clean_end: u64,
}

#[derive(Clone)]
struct Index {
    body: IndexBody,
    by_path: HashMap<u32, Vec<u32>>,
    path_ids: HashMap<String, u32>,
    note_of: HashMap<u128, u64>,
    stats: Stats,
    len: u64,
    mtime: u64,
}

impl Index {
    fn from_body(body: IndexBody, len: u64, mtime: u64, read_only: bool, from_cache: bool) -> Index {
        let mut by_path: HashMap<u32, Vec<u32>> = HashMap::new();
        for (i, e) in body.entries.iter().enumerate() {
            by_path.entry(e.path).or_default().push(i as u32);
        }
        let path_ids = body.paths.iter().enumerate().map(|(i, p)| (p.clone(), i as u32)).collect();
        let note_of: HashMap<u128, u64> = body.notes.iter().copied().collect();
        let orphan_notes = if body.notes.is_empty() {
            0
        } else {
            let mut missing: HashMap<u128, u64> = HashMap::new();
            for (id, _) in &body.notes {
                *missing.entry(*id).or_default() += 1;
            }
            for e in &body.entries {
                missing.remove(&e.id);
            }
            missing.values().sum()
        };
        let stats = Stats {
            log_len: len,
            frames: body.frames,
            records: body.entries.len() as u64,
            notes: body.notes.len() as u64,
            orphan_notes,
            skipped_frames: body.skipped,
            read_only,
            from_cache,
        };
        Index { body, by_path, path_ids, note_of, stats, len, mtime }
    }

    /// Scan the log from `start` (header length for a full scan, `clean_end` for an extension),
    /// streaming: nothing but the index entries is retained.
    fn scan_into<R: Read>(mut body: IndexBody, f: R, start: u64) -> IndexBody {
        let mut path_ids: HashMap<String, u32> =
            body.paths.iter().enumerate().map(|(i, p)| (p.clone(), i as u32)).collect();
        let mut frames = 0u64;
        let end = scan_stream(BufReader::with_capacity(CHUNK, f), start, |off, fr| {
            frames += 1;
            match fr {
                Frame::Edit(r) | Frame::Unattributed(r) => {
                    let next = body.paths.len() as u32;
                    let path = *path_ids.entry(r.path.clone()).or_insert_with(|| {
                        body.paths.push(r.path.clone());
                        next
                    });
                    body.entries.push(Entry { id: r.id, path, off });
                }
                Frame::Note(n) => body.notes.push((n.record_id, off)),
            }
        });
        body.frames += frames;
        body.skipped += end.skipped;
        body.clean_end = if end.clean { end.end } else { 0 };
        body
    }
}

// ---------------------------------------------------------------- store

pub struct Store {
    dir: PathBuf,
    log: PathBuf,
    read_only: Option<StoreError>,
    cache: Mutex<Option<Arc<Index>>>,
}

fn read_header(path: &Path) -> Result<Option<(u16, bool)>> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut h = [0u8; LOG_HEADER_LEN];
    let mut n = 0;
    while n < LOG_HEADER_LEN {
        let k = f.read(&mut h[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    if n < LOG_HEADER_LEN {
        return Ok(None); // treated as fresh
    }
    if &h[0..4] != LOG_MAGIC {
        return Ok(Some((0, false)));
    }
    Ok(Some((u16::from_le_bytes([h[4], h[5]]), true)))
}

impl Store {
    /// Opens (without creating) the store under `<git_dir>/codetrail`.
    pub fn open(git_dir: &Path) -> Result<Store> {
        let dir = git_dir.join("codetrail");
        let log = dir.join("log.ct");
        let read_only = match read_header(&log)? {
            None => None,
            Some((_, false)) => Some(StoreError::ReadOnlyBadHeader),
            Some((v, true)) if v != FORMAT_VERSION => {
                Some(StoreError::ReadOnlyVersion { found: v, supported: FORMAT_VERSION })
            }
            Some(_) => None,
        };
        Ok(Store { dir, log, read_only, cache: Mutex::new(None) })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn log_path(&self) -> &Path {
        &self.log
    }
    /// `Some(message)` when the log cannot be written/parsed by this build.
    pub fn read_only_reason(&self) -> Option<String> {
        self.read_only.as_ref().map(|e| e.to_string())
    }

    fn append_frame(&self, frame: &Frame) -> Result<()> {
        if let Some(e) = &self.read_only {
            return Err(match e {
                StoreError::ReadOnlyVersion { found, supported } => {
                    StoreError::ReadOnlyVersion { found: *found, supported: *supported }
                }
                _ => StoreError::ReadOnlyBadHeader,
            });
        }
        let payload = postcard::to_stdvec(frame).map_err(|e| StoreError::Encode(e.to_string()))?;
        if payload.len() > MAX_FRAME_LEN {
            return Err(StoreError::TooLarge(payload.len()));
        }
        fs::create_dir_all(&self.dir)?;
        let mut f = OpenOptions::new().read(true).append(true).create(true).open(&self.log)?;
        f.lock_exclusive()?;
        let res = (|| -> Result<()> {
            let len = f.metadata()?.len();
            let mut buf = Vec::with_capacity(LOG_HEADER_LEN + FRAME_HEADER_LEN + payload.len());
            let mut seq = len;
            if len < LOG_HEADER_LEN as u64 {
                if len > 0 {
                    f.set_len(0)?; // partial header from a crashed first write
                }
                buf.extend_from_slice(LOG_MAGIC);
                buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
                seq = LOG_HEADER_LEN as u64;
            } else {
                // Re-check header under the lock: another process may be a different version.
                let mut h = [0u8; LOG_HEADER_LEN];
                f.seek(SeekFrom::Start(0))?;
                f.read_exact(&mut h)?;
                if &h[0..4] != LOG_MAGIC {
                    return Err(StoreError::ReadOnlyBadHeader);
                }
                let v = u16::from_le_bytes([h[4], h[5]]);
                if v != FORMAT_VERSION {
                    return Err(StoreError::ReadOnlyVersion { found: v, supported: FORMAT_VERSION });
                }
            }
            buf.extend_from_slice(FRAME_MAGIC);
            buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            buf.extend_from_slice(&seq.to_le_bytes());
            buf.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
            buf.extend_from_slice(&payload);
            f.write_all(&buf)?; // ONE write under O_APPEND + flock
            Ok(())
        })();
        let _ = FileExt::unlock(&f);
        res?;
        Ok(())
    }

    pub fn append(&self, rec: &Record) -> Result<()> {
        let f = match rec.kind {
            Kind::Edit => Frame::Edit(rec.clone()),
            Kind::Unattributed => Frame::Unattributed(rec.clone()),
        };
        self.append_frame(&f)
    }

    /// Attach a reason to a record (latest note wins on load; history stays in the log).
    pub fn append_note(&self, record_id: u128, agent: Agent, session: &str, reason: &str) -> Result<()> {
        self.append_frame(&Frame::Note(NoteFrame {
            record_id,
            ts_ms: now_ms(),
            agent,
            session: session.to_string(),
            reason: encode_reason(reason),
        }))
    }

    /// Current index. Every call stats the log (cheap) and, when another process appended, scans only
    /// the new tail (streaming). The shared flock excludes in-flight appends so a half-written frame
    /// is never observed.
    fn index(&self) -> Arc<Index> {
        let mut cache = self.cache.lock();
        let fresh = self.refresh(cache.take());
        *cache = Some(fresh.clone());
        fresh
    }

    fn refresh(&self, cached: Option<Arc<Index>>) -> Arc<Index> {
        let read_only = self.read_only.is_some();
        let empty = |len| Arc::new(Index::from_body(IndexBody::default(), len, 0, read_only, false));
        let Ok(mut f) = File::open(&self.log) else { return empty(0) };
        let Ok(meta) = f.metadata() else { return empty(0) };
        if read_only {
            return empty(meta.len());
        }
        let _ = f.lock_shared();
        let had_cache = cached.is_some();
        let out = (|| {
            let meta = f.metadata().ok()?;
            let (len, mtime) = (meta.len(), mtime_ns(&meta));
            if let Some(c) = cached {
                if c.len == len && c.mtime == mtime {
                    return Some(c);
                }
                if len > c.len && c.body.clean_end == c.len && c.len >= LOG_HEADER_LEN as u64 {
                    f.seek(SeekFrom::Start(c.len)).ok()?;
                    let start = c.len;
                    // sole owner in the common case: extend in place instead of copying the index
                    let idx = Arc::try_unwrap(c).unwrap_or_else(|a| (*a).clone());
                    let body = Index::scan_into(idx.body, &mut f, start);
                    return Some(Arc::new(Index::from_body(body, len, mtime, false, false)));
                }
            }
            let idx_path = self.dir.join("index.ct");
            if !had_cache {
                if let Some(body) = read_cache(&idx_path, len, mtime) {
                    return Some(Arc::new(Index::from_body(body, len, mtime, false, true)));
                }
            }
            f.seek(SeekFrom::Start(LOG_HEADER_LEN as u64)).ok()?;
            let body = Index::scan_into(IndexBody::default(), &mut f, LOG_HEADER_LEN as u64);
            let _ = write_cache(&idx_path, &body, len, mtime); // consistent: read under the shared lock
            Some(Arc::new(Index::from_body(body, len, mtime, false, false)))
        })();
        let _ = FileExt::unlock(&f);
        out.unwrap_or_else(|| empty(meta.len()))
    }

    pub fn stats(&self) -> Stats {
        self.index().stats.clone()
    }

    /// Read one frame back from the log (header + crc verified).
    fn frame_at(f: &mut File, off: u64) -> Option<Frame> {
        f.seek(SeekFrom::Start(off)).ok()?;
        let mut h = [0u8; FRAME_HEADER_LEN];
        f.read_exact(&mut h).ok()?;
        if &h[0..4] != FRAME_MAGIC {
            return None;
        }
        let len = u32::from_le_bytes(h[4..8].try_into().ok()?) as usize;
        if len > MAX_FRAME_LEN {
            return None;
        }
        let crc = u32::from_le_bytes(h[16..20].try_into().ok()?);
        let mut payload = vec![0u8; len];
        f.read_exact(&mut payload).ok()?;
        (crc32fast::hash(&payload) == crc).then_some(())?;
        postcard::from_bytes(&payload).ok()
    }

    /// Materialise the record of `e` (latest note merged in).
    fn load(idx: &Index, f: &mut File, e: &Entry) -> Option<Record> {
        let (Frame::Edit(mut r) | Frame::Unattributed(mut r)) = Self::frame_at(f, e.off)? else { return None };
        if let Some(Frame::Note(n)) = idx.note_of.get(&e.id).and_then(|&o| Self::frame_at(f, o)) {
            r.reason = n.reason;
        }
        Some(r)
    }

    fn load_entries<'a>(&self, idx: &Index, entries: impl Iterator<Item = &'a Entry>) -> Vec<Record> {
        let Ok(mut f) = File::open(&self.log) else { return Vec::new() };
        let mut out: Vec<Record> = entries.filter_map(|e| Self::load(idx, &mut f, e)).collect();
        out.sort_by_key(|r| (r.ts_ms, r.id));
        out
    }

    pub fn all(&self) -> Vec<Record> {
        let idx = self.index();
        self.load_entries(&idx, idx.body.entries.iter())
    }

    pub fn get(&self, id: u128) -> Option<Record> {
        let idx = self.index();
        let e = idx.body.entries.iter().find(|e| e.id == id)?;
        self.load_entries(&idx, std::iter::once(e)).pop()
    }

    /// Resolve a full id or a unique hex prefix (>= 6 chars).
    pub fn find_id(&self, s: &str) -> std::result::Result<Record, String> {
        let s = s.trim().to_ascii_lowercase();
        if s.len() < 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("invalid record id '{s}'"));
        }
        let idx = self.index();
        let hits: Vec<&Entry> = idx.body.entries.iter().filter(|e| fmt_id(e.id).starts_with(&s)).collect();
        match hits.len() {
            0 => Err(format!("no record with id '{s}'")),
            1 => self
                .load_entries(&idx, hits.into_iter())
                .pop()
                .ok_or_else(|| format!("record '{s}' is unreadable")),
            n => Err(format!("id prefix '{s}' is ambiguous ({n} records)")),
        }
    }

    pub fn for_path(&self, path: &str) -> Vec<Record> {
        let idx = self.index();
        let Some(rows) = idx.path_ids.get(path).and_then(|p| idx.by_path.get(p)) else { return Vec::new() };
        self.load_entries(&idx, rows.iter().map(|&i| &idx.body.entries[i as usize]))
    }

    /// Records whose spans overlap `[start, end]` (1-based, inclusive) in the file as last edited.
    /// With `hash_hint`, records whose span fingerprint equals it are returned first.
    pub fn for_range(&self, path: &str, start: u32, end: u32, hash_hint: Option<u64>) -> Vec<Record> {
        let mut out: Vec<(bool, Record)> = self
            .for_path(path)
            .into_iter()
            .filter_map(|r| {
                let overlap = r.spans.iter().any(|s| {
                    s.new_len > 0 && s.new_start <= end && s.new_start + s.new_len - 1 >= start
                });
                let hint = hash_hint.is_some_and(|h| r.spans.iter().any(|s| s.fingerprint == h));
                (overlap || hint).then_some((hint, r))
            })
            .collect();
        out.sort_by_key(|(hint, r)| (!*hint, r.ts_ms));
        out.into_iter().map(|(_, r)| r).collect()
    }

    /// Deterministic commit linking (PLAN §9.3). `blob_oids` = the file's blob oid(s) in the commit.
    /// * `High`: `post_blob` equals a commit blob.
    /// * `Medium`: a record span's fingerprint equals a window of the hunk's added lines (a span with
    ///   `occurrence = k` needs at least k+1 equal windows).
    /// * `Unknown` is the *absence* of any match: the result is empty, see [`best_confidence`].
    ///
    /// Delete-only changes (`added_lines` empty) can only be linked by `High`.
    pub fn match_hunk(&self, path: &str, added_lines: &[String], blob_oids: &[Oid]) -> Vec<(Record, Confidence)> {
        let recs = self.for_path(path);
        let n = added_lines.len();
        // prefix[k] = polynomial hash of first k lines
        let mut prefix = Vec::with_capacity(n + 1);
        prefix.push(0u64);
        for l in added_lines {
            let p = prefix.last().copied().unwrap_or(0);
            prefix.push(p.wrapping_mul(B).wrapping_add(line_hash(l)));
        }
        let mut nonblank = Vec::with_capacity(n + 1);
        nonblank.push(0usize);
        for l in added_lines {
            let p = nonblank.last().copied().unwrap_or(0);
            nonblank.push(p + usize::from(!l.trim().is_empty()));
        }
        let mut pow = vec![1u64; n + 1];
        for k in 1..=n {
            pow[k] = pow[k - 1].wrapping_mul(B);
        }
        let mut windows: HashMap<usize, HashMap<u64, usize>> = HashMap::new();
        let mut out = Vec::new();
        for r in recs {
            if r.post_blob.as_ref().is_some_and(|b| blob_oids.iter().any(|o| o == b)) {
                out.push((r, Confidence::High));
                continue;
            }
            let mut hit = false;
            for s in &r.spans {
                let len = s.new_len as usize;
                if len == 0 || len > n {
                    continue;
                }
                let counts = windows.entry(len).or_insert_with(|| {
                    let mut m: HashMap<u64, usize> = HashMap::new();
                    for i in 0..=(n - len) {
                        if nonblank[i + len] == nonblank[i] {
                            continue; // blank-only windows carry no signal
                        }
                        let h = prefix[i + len].wrapping_sub(prefix[i].wrapping_mul(pow[len]));
                        *m.entry(h).or_default() += 1;
                    }
                    m
                });
                let c = counts.get(&s.fingerprint).copied().unwrap_or(0);
                if c > s.occurrence as usize {
                    hit = true;
                    break;
                }
            }
            if hit {
                out.push((r, Confidence::Medium));
            }
        }
        out
    }

    /// Dump every record (+ note history) as JSON for backup / re-clone. Streams: records and notes are
    /// read from the log one at a time while being written, never all in memory.
    pub fn export_json<W: Write>(&self, mut w: W) -> Result<()> {
        use std::cell::RefCell;
        let idx = self.index();
        #[derive(Serialize)]
        struct ER {
            id: String,
            ts_ms: i64,
            agent: String,
            session: String,
            kind: &'static str,
            tool: String,
            tool_use_id: String,
            head_at_edit: String,
            path: String,
            pre_blob: Option<Oid>,
            post_blob: Option<Oid>,
            spans: Vec<Span>,
            reason: String,
            transcript: Option<TranscriptPtr>,
        }
        #[derive(Serialize)]
        struct EN {
            record_id: String,
            ts_ms: i64,
            agent: String,
            session: String,
            reason: String,
        }
        /// A sequence serialised straight from an iterator.
        struct Seq<I>(RefCell<Option<I>>);
        impl<T: Serialize, I: Iterator<Item = T>> Serialize for Seq<I> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                s.collect_seq(self.0.borrow_mut().take().into_iter().flatten())
            }
        }
        #[derive(Serialize)]
        struct Doc<R: Serialize, N: Serialize> {
            format: &'static str,
            version: u16,
            skipped_frames: u64,
            read_only: Option<String>,
            records: R,
            notes: N,
        }
        let mut order: Vec<&Entry> = idx.body.entries.iter().collect();
        order.sort_by_key(|e| (e.id, e.off)); // ids are time-ordered
        let mut rf = File::open(&self.log).ok();
        let mut nf = File::open(&self.log).ok();
        let records = order.into_iter().filter_map(|e| {
            let r = Self::load(&idx, rf.as_mut()?, e)?;
            Some(ER {
                id: fmt_id(r.id),
                ts_ms: r.ts_ms,
                agent: r.agent.name(),
                kind: match r.kind {
                    Kind::Edit => "edit",
                    Kind::Unattributed => "unattributed",
                },
                reason: decode_reason(&r),
                session: r.session,
                tool: r.tool,
                tool_use_id: r.tool_use_id,
                head_at_edit: r.head_at_edit,
                path: r.path,
                pre_blob: r.pre_blob,
                post_blob: r.post_blob,
                spans: r.spans,
                transcript: r.transcript,
            })
        });
        let notes = idx.body.notes.iter().filter_map(|&(_, off)| {
            let Some(Frame::Note(n)) = Self::frame_at(nf.as_mut()?, off) else { return None };
            Some(EN {
                record_id: fmt_id(n.record_id),
                ts_ms: n.ts_ms,
                agent: n.agent.name(),
                session: n.session,
                reason: decode_reason_bytes(&n.reason),
            })
        });
        let doc = Doc {
            format: "codetrail-export",
            version: FORMAT_VERSION,
            skipped_frames: idx.stats.skipped_frames,
            read_only: self.read_only_reason(),
            records: Seq(RefCell::new(Some(records))),
            notes: Seq(RefCell::new(Some(notes))),
        };
        serde_json::to_writer_pretty(std::io::BufWriter::new(&mut w), &doc)
            .map_err(|e| StoreError::Encode(e.to_string()))?;
        w.write_all(b"\n")?;
        Ok(())
    }
}

/// `Unknown` when nothing linked (UI: "not linked"), else the best (lowest) confidence value.
pub fn best_confidence(m: &[(Record, Confidence)]) -> Confidence {
    m.iter().map(|(_, c)| *c).min().unwrap_or(Confidence::Unknown)
}

// ---------------------------------------------------------------- index cache file

fn mtime_ns(m: &fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// `index.ct`: derived cache of the in-memory index, fixed-width and streamed (never one big buffer).
fn read_cache(path: &Path, len: u64, mtime: u64) -> Option<IndexBody> {
    let mut r = BufReader::with_capacity(CHUNK, File::open(path).ok()?);
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic).ok()?;
    if &magic != IDX_MAGIC {
        return None;
    }
    let mut u16b = [0u8; 2];
    r.read_exact(&mut u16b).ok()?;
    if u16::from_le_bytes(u16b) != FORMAT_VERSION {
        return None;
    }
    let mut u64s = || -> Option<u64> {
        let mut b = [0u8; 8];
        r.read_exact(&mut b).ok()?;
        Some(u64::from_le_bytes(b))
    };
    if u64s()? != len || u64s()? != mtime {
        return None;
    }
    let (frames, skipped, clean_end) = (u64s()?, u64s()?, u64s()?);
    let n_paths = u64s()?;
    let n_entries = u64s()?;
    let n_notes = u64s()?;
    // sanity bound against a corrupt header: an entry cannot be smaller than a frame header
    let max = len / FRAME_HEADER_LEN as u64;
    if n_entries > max || n_notes > max || n_paths > n_entries {
        return None;
    }
    let mut body = IndexBody { frames, skipped, clean_end, ..Default::default() };
    for _ in 0..n_paths {
        let mut l = [0u8; 4];
        r.read_exact(&mut l).ok()?;
        let l = u32::from_le_bytes(l) as usize;
        if l > MAX_FRAME_LEN {
            return None;
        }
        let mut p = vec![0u8; l];
        r.read_exact(&mut p).ok()?;
        body.paths.push(String::from_utf8(p).ok()?);
    }
    body.entries.reserve_exact(n_entries as usize);
    for _ in 0..n_entries {
        let mut b = [0u8; 28];
        r.read_exact(&mut b).ok()?;
        let path = u32::from_le_bytes(b[16..20].try_into().ok()?);
        if path as u64 >= n_paths {
            return None;
        }
        body.entries.push(Entry {
            id: u128::from_le_bytes(b[0..16].try_into().ok()?),
            path,
            off: u64::from_le_bytes(b[20..28].try_into().ok()?),
        });
    }
    for _ in 0..n_notes {
        let mut b = [0u8; 24];
        r.read_exact(&mut b).ok()?;
        body.notes.push((u128::from_le_bytes(b[0..16].try_into().ok()?), u64::from_le_bytes(b[16..24].try_into().ok()?)));
    }
    let mut extra = [0u8; 1];
    (r.read(&mut extra).ok()? == 0).then_some(body)
}

fn write_cache(path: &Path, body: &IndexBody, len: u64, mtime: u64) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut w = std::io::BufWriter::with_capacity(CHUNK, File::create(&tmp)?);
        w.write_all(IDX_MAGIC)?;
        w.write_all(&FORMAT_VERSION.to_le_bytes())?;
        for v in [
            len,
            mtime,
            body.frames,
            body.skipped,
            body.clean_end,
            body.paths.len() as u64,
            body.entries.len() as u64,
            body.notes.len() as u64,
        ] {
            w.write_all(&v.to_le_bytes())?;
        }
        for p in &body.paths {
            w.write_all(&(p.len() as u32).to_le_bytes())?;
            w.write_all(p.as_bytes())?;
        }
        for e in &body.entries {
            w.write_all(&e.id.to_le_bytes())?;
            w.write_all(&e.path.to_le_bytes())?;
            w.write_all(&e.off.to_le_bytes())?;
        }
        for (id, off) in &body.notes {
            w.write_all(&id.to_le_bytes())?;
            w.write_all(&off.to_le_bytes())?;
        }
        w.flush()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}
