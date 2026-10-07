//! ct-store: append-only, self-contained AI change-reason log (`<git_dir>/codetrail/log.ct`).
//!
//! Layout (PLAN §9.3):
//! * file header: `"CTLG"` + version `u16` (LE)
//! * frame: `"CTF1"` | len `u32` (<= 1 MiB) | seq `u64` | crc32(payload) `u32` | postcard payload
//!   - `seq` is the byte offset of the frame (unique + monotonic under the append lock).
//!   - the whole frame is written with ONE `write_all` while holding `flock(LOCK_EX)`; no fsync
//!     (a crash may lose the newest frames; this is deliberate and documented).
//! * corrupt frames are skipped by resyncing on the next magic; the skip count is exposed in [`Stats`].
//! * `index.ct` caches the merged in-memory index keyed by (log length, log mtime, version) and is
//!   replaced atomically (tmp + rename). A log with a different version is opened read-only.
//!
//! The data is LOCAL ONLY: deleting `.git` deletes the log. Use [`Store::export_json`] for backups.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use parking_lot::Mutex;
use std::sync::Arc;
use xxhash_rust::xxh3::xxh3_64;

pub type Oid = String;

pub const FORMAT_VERSION: u16 = 1;
const LOG_MAGIC: &[u8; 4] = b"CTLG";
const FRAME_MAGIC: &[u8; 4] = b"CTF1";
const IDX_MAGIC: &[u8; 4] = b"CTIX";
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

struct Scan {
    frames: Vec<Frame>,
    skipped: u64,
}

fn find_magic(buf: &[u8], from: usize) -> Option<usize> {
    if from >= buf.len() {
        return None;
    }
    buf[from..].windows(4).position(|w| w == FRAME_MAGIC).map(|p| p + from)
}

fn scan_frames(buf: &[u8]) -> Scan {
    let mut frames = Vec::new();
    let mut skipped = 0u64;
    let mut pos = LOG_HEADER_LEN.min(buf.len());
    while pos < buf.len() {
        let ok = (|| {
            let h = buf.get(pos..pos + FRAME_HEADER_LEN)?;
            if &h[0..4] != FRAME_MAGIC {
                return None;
            }
            let len = u32::from_le_bytes(h[4..8].try_into().ok()?) as usize;
            if len > MAX_FRAME_LEN {
                return None;
            }
            let crc = u32::from_le_bytes(h[16..20].try_into().ok()?);
            let payload = buf.get(pos + FRAME_HEADER_LEN..pos + FRAME_HEADER_LEN + len)?;
            if crc32fast::hash(payload) != crc {
                return None;
            }
            let f: Frame = postcard::from_bytes(payload).ok()?;
            Some((f, FRAME_HEADER_LEN + len))
        })();
        match ok {
            Some((f, adv)) => {
                frames.push(f);
                pos += adv;
            }
            None => {
                skipped += 1;
                match find_magic(buf, pos + 1) {
                    Some(n) => pos = n,
                    None => break,
                }
            }
        }
    }
    Scan { frames, skipped }
}

// ---------------------------------------------------------------- index

#[derive(Serialize, Deserialize, Default)]
struct IndexBody {
    records: Vec<Record>,
    notes: Vec<NoteFrame>,
    frames: u64,
    orphan_notes: u64,
    skipped: u64,
}

struct Index {
    body: IndexBody,
    by_path: HashMap<String, Vec<usize>>,
    stats: Stats,
}

impl Index {
    fn from_body(body: IndexBody, log_len: u64, read_only: bool, from_cache: bool) -> Index {
        let mut by_path: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, r) in body.records.iter().enumerate() {
            by_path.entry(r.path.clone()).or_default().push(i);
        }
        let stats = Stats {
            log_len,
            frames: body.frames,
            records: body.records.len() as u64,
            notes: body.notes.len() as u64,
            orphan_notes: body.orphan_notes,
            skipped_frames: body.skipped,
            read_only,
            from_cache,
        };
        Index { body, by_path, stats }
    }

    fn build(buf: &[u8]) -> IndexBody {
        let scan = scan_frames(buf);
        let mut body = IndexBody { skipped: scan.skipped, frames: scan.frames.len() as u64, ..Default::default() };
        let mut pos: HashMap<u128, usize> = HashMap::new();
        for f in scan.frames {
            match f {
                Frame::Edit(r) | Frame::Unattributed(r) => {
                    pos.insert(r.id, body.records.len());
                    body.records.push(r);
                }
                Frame::Note(n) => {
                    match pos.get(&n.record_id) {
                        Some(&i) => body.records[i].reason = n.reason.clone(),
                        None => body.orphan_notes += 1,
                    }
                    body.notes.push(n);
                }
            }
        }
        body.records.sort_by_key(|r| (r.ts_ms, r.id));
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
        *self.cache.lock() = None;
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

    fn index(&self) -> Arc<Index> {
        if let Some(i) = self.cache.lock().as_ref() {
            return i.clone();
        }
        let idx = Arc::new(self.load_index());
        *self.cache.lock() = Some(idx.clone());
        idx
    }

    fn load_index(&self) -> Index {
        let read_only = self.read_only.is_some();
        let meta = match fs::metadata(&self.log) {
            Ok(m) => m,
            Err(_) => return Index::from_body(IndexBody::default(), 0, read_only, false),
        };
        if read_only {
            return Index::from_body(IndexBody::default(), meta.len(), true, false);
        }
        let len = meta.len();
        let mtime = mtime_ns(&meta);
        let idx_path = self.dir.join("index.ct");
        if let Some(body) = read_cache(&idx_path, len, mtime) {
            return Index::from_body(body, len, false, true);
        }
        let buf = fs::read(&self.log).unwrap_or_default();
        // Re-stat after reading: only cache when no append raced the scan.
        let body = Index::build(&buf);
        let len2 = fs::metadata(&self.log).map(|m| m.len()).unwrap_or(0);
        if len2 == buf.len() as u64 && len == len2 {
            let _ = write_cache(&idx_path, &body, len, mtime);
        }
        Index::from_body(body, buf.len() as u64, false, false)
    }

    pub fn stats(&self) -> Stats {
        self.index().stats.clone()
    }

    pub fn all(&self) -> Vec<Record> {
        self.index().body.records.clone()
    }

    pub fn get(&self, id: u128) -> Option<Record> {
        self.index().body.records.iter().find(|r| r.id == id).cloned()
    }

    /// Resolve a full id or a unique hex prefix (>= 6 chars).
    pub fn find_id(&self, s: &str) -> std::result::Result<Record, String> {
        let s = s.trim().to_ascii_lowercase();
        if s.len() < 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("invalid record id '{s}'"));
        }
        let idx = self.index();
        let hits: Vec<&Record> = idx.body.records.iter().filter(|r| fmt_id(r.id).starts_with(&s)).collect();
        match hits.len() {
            0 => Err(format!("no record with id '{s}'")),
            1 => Ok(hits[0].clone()),
            n => Err(format!("id prefix '{s}' is ambiguous ({n} records)")),
        }
    }

    pub fn for_path(&self, path: &str) -> Vec<Record> {
        let idx = self.index();
        idx.by_path
            .get(path)
            .map(|v| v.iter().map(|&i| idx.body.records[i].clone()).collect())
            .unwrap_or_default()
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

    /// Dump every record (+ note history) as JSON for backup / re-clone.
    pub fn export_json<W: Write>(&self, mut w: W) -> Result<()> {
        let idx = self.index();
        #[derive(Serialize)]
        struct ER<'a> {
            id: String,
            ts_ms: i64,
            agent: String,
            session: &'a str,
            kind: &'a str,
            tool: &'a str,
            tool_use_id: &'a str,
            head_at_edit: &'a str,
            path: &'a str,
            pre_blob: &'a Option<Oid>,
            post_blob: &'a Option<Oid>,
            spans: &'a [Span],
            reason: String,
            transcript: &'a Option<TranscriptPtr>,
        }
        #[derive(Serialize)]
        struct EN {
            record_id: String,
            ts_ms: i64,
            agent: String,
            session: String,
            reason: String,
        }
        #[derive(Serialize)]
        struct Doc<'a> {
            format: &'static str,
            version: u16,
            skipped_frames: u64,
            read_only: Option<String>,
            records: Vec<ER<'a>>,
            notes: Vec<EN>,
        }
        let doc = Doc {
            format: "codetrail-export",
            version: FORMAT_VERSION,
            skipped_frames: idx.stats.skipped_frames,
            read_only: self.read_only_reason(),
            records: idx
                .body
                .records
                .iter()
                .map(|r| ER {
                    id: fmt_id(r.id),
                    ts_ms: r.ts_ms,
                    agent: r.agent.name(),
                    session: &r.session,
                    kind: match r.kind {
                        Kind::Edit => "edit",
                        Kind::Unattributed => "unattributed",
                    },
                    tool: &r.tool,
                    tool_use_id: &r.tool_use_id,
                    head_at_edit: &r.head_at_edit,
                    path: &r.path,
                    pre_blob: &r.pre_blob,
                    post_blob: &r.post_blob,
                    spans: &r.spans,
                    reason: decode_reason(r),
                    transcript: &r.transcript,
                })
                .collect(),
            notes: idx
                .body
                .notes
                .iter()
                .map(|n| EN {
                    record_id: fmt_id(n.record_id),
                    ts_ms: n.ts_ms,
                    agent: n.agent.name(),
                    session: n.session.clone(),
                    reason: decode_reason_bytes(&n.reason),
                })
                .collect(),
        };
        serde_json::to_writer_pretty(&mut w, &doc).map_err(|e| StoreError::Encode(e.to_string()))?;
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

fn read_cache(path: &Path, len: u64, mtime: u64) -> Option<IndexBody> {
    let b = fs::read(path).ok()?;
    if b.len() < 4 + 2 + 8 + 8 || &b[0..4] != IDX_MAGIC {
        return None;
    }
    if u16::from_le_bytes([b[4], b[5]]) != FORMAT_VERSION {
        return None;
    }
    let l = u64::from_le_bytes(b[6..14].try_into().ok()?);
    let m = u64::from_le_bytes(b[14..22].try_into().ok()?);
    if l != len || m != mtime {
        return None;
    }
    postcard::from_bytes(&b[22..]).ok()
}

fn write_cache(path: &Path, body: &IndexBody, len: u64, mtime: u64) -> Result<()> {
    let mut out = Vec::new();
    out.extend_from_slice(IDX_MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&mtime.to_le_bytes());
    out.extend_from_slice(&postcard::to_stdvec(body).map_err(|e| StoreError::Encode(e.to_string()))?);
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, out)?;
    fs::rename(&tmp, path)?;
    Ok(())
}
