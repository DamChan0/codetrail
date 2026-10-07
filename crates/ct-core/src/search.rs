//! Content search (ripgrep engine, parallel walker) and fuzzy file-name index (nucleo).

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::overrides::OverrideBuilder;
use ignore::{WalkBuilder, WalkState};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32String};
use rayon::prelude::*;

use crate::{Error, Result, SearchHit, SearchQuery, SearchStats};

/// How long a full result channel may stay full before the search aborts (consumer stalled).
const STALL: Duration = Duration::from_secs(5);

struct Shared {
    cancel: Arc<AtomicBool>,
    /// Set when the search must stop for any reason (cancel, consumer gone/stalled).
    stop: AtomicBool,
    aborted: AtomicBool,
    files_searched: AtomicU64,
    files_matched: AtomicU64,
    matches: AtomicU64,
    bytes: AtomicU64,
    first_hit_us: AtomicU64,
    start: Instant,
}

impl Shared {
    fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.cancel.load(Ordering::Relaxed)
    }
}

/// Reader that fails fast when the cancel flag is set and counts bytes read.
struct CancelReader<'a> {
    inner: File,
    sh: &'a Shared,
}

impl Read for CancelReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.sh.should_stop() {
            return Err(io::Error::other("search cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.sh.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

struct HitSink<'a, M: grep_matcher::Matcher> {
    matcher: &'a M,
    path: &'a str,
    tx: &'a SyncSender<SearchHit>,
    sh: &'a Shared,
    hits: u64,
}

impl<M: grep_matcher::Matcher> Sink for HitSink<'_, M> {
    type Error = io::Error;

    fn matched(&mut self, _s: &Searcher, m: &SinkMatch<'_>) -> std::result::Result<bool, io::Error> {
        if self.sh.should_stop() {
            return Ok(false);
        }
        let mut bytes = m.bytes();
        if bytes.last() == Some(&b'\n') {
            bytes = &bytes[..bytes.len() - 1];
        }
        if bytes.last() == Some(&b'\r') {
            bytes = &bytes[..bytes.len() - 1];
        }
        let text = String::from_utf8_lossy(bytes).into_owned();
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        let _ = self.matcher.find_iter(text.as_bytes(), |mm| {
            ranges.push((mm.start() as u32, mm.end() as u32));
            true
        });
        let hit = SearchHit {
            path: self.path.to_string(),
            line: m.line_number().unwrap_or(0) as u32,
            col: ranges.first().map(|r| r.0 + 1).unwrap_or(1),
            text,
            ranges,
        };
        if !send(self.tx, hit, self.sh) {
            return Ok(false);
        }
        self.hits += 1;
        self.sh.matches.fetch_add(1, Ordering::Relaxed);
        let us = self.sh.start.elapsed().as_micros() as u64;
        let _ = self.sh.first_hit_us.compare_exchange(u64::MAX, us, Ordering::Relaxed, Ordering::Relaxed);
        Ok(true)
    }
}

/// Bounded send with backpressure: waits while the channel is full, gives up when the search is
/// cancelled, the receiver is gone, or the consumer stalls for [`STALL`].
fn send(tx: &SyncSender<SearchHit>, hit: SearchHit, sh: &Shared) -> bool {
    let mut hit = hit;
    let mut waited: Option<Instant> = None;
    loop {
        match tx.try_send(hit) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => {
                sh.aborted.store(true, Ordering::Relaxed);
                sh.stop.store(true, Ordering::Relaxed);
                return false;
            }
            Err(TrySendError::Full(h)) => {
                if sh.should_stop() {
                    return false;
                }
                let since = *waited.get_or_insert_with(Instant::now);
                if since.elapsed() > STALL {
                    sh.aborted.store(true, Ordering::Relaxed);
                    sh.stop.store(true, Ordering::Relaxed);
                    return false;
                }
                hit = h;
                std::thread::sleep(Duration::from_micros(300));
            }
        }
    }
}

/// Parallel content search. Hits are sent (unordered) through the bounded channel; the search
/// stops promptly when `cancel` is set, the receiver is dropped, or the receiver stalls.
pub fn search_content(
    root: &Path,
    q: &SearchQuery,
    cancel: Arc<AtomicBool>,
    tx: SyncSender<SearchHit>,
) -> Result<SearchStats> {
    if q.pattern.is_empty() {
        return Err(Error::Invalid("empty search pattern".into()));
    }
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(!q.case_sensitive)
        .fixed_strings(!q.regex)
        .line_terminator(Some(b'\n'))
        .build(&q.pattern)
        .map_err(|e| Error::Search(format!("invalid pattern: {e}")))?;
    let searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .build();

    let mut wb = WalkBuilder::new(root);
    wb.threads(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(12));
    if !q.globs.is_empty() {
        let mut ob = OverrideBuilder::new(root);
        for g in &q.globs {
            ob.add(g).map_err(|e| Error::Search(format!("invalid glob {g:?}: {e}")))?;
        }
        wb.overrides(ob.build().map_err(|e| Error::Search(format!("invalid globs: {e}")))?);
    }

    let sh = Shared {
        cancel,
        stop: AtomicBool::new(false),
        aborted: AtomicBool::new(false),
        files_searched: AtomicU64::new(0),
        files_matched: AtomicU64::new(0),
        matches: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        first_hit_us: AtomicU64::new(u64::MAX),
        start: Instant::now(),
    };
    let shr = &sh;
    let (matcher, searcher, txr) = (&matcher, &searcher, &tx);

    wb.build_parallel().run(|| {
        let mut searcher = searcher.clone();
        Box::new(move |entry| {
            if shr.should_stop() {
                return WalkState::Quit;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => return WalkState::Continue,
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
            let rel = rel.to_string_lossy().replace('\\', "/");
            let file = match File::open(entry.path()) {
                Ok(f) => f,
                Err(_) => return WalkState::Continue,
            };
            shr.files_searched.fetch_add(1, Ordering::Relaxed);
            let mut sink = HitSink { matcher, path: &rel, tx: txr, sh: shr, hits: 0 };
            let _ = searcher.search_reader(matcher, CancelReader { inner: file, sh: shr }, &mut sink);
            if sink.hits > 0 {
                shr.files_matched.fetch_add(1, Ordering::Relaxed);
            }
            if shr.should_stop() {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });

    let first = sh.first_hit_us.load(Ordering::Relaxed);
    Ok(SearchStats {
        files_searched: sh.files_searched.load(Ordering::Relaxed),
        files_matched: sh.files_matched.load(Ordering::Relaxed),
        matches: sh.matches.load(Ordering::Relaxed),
        bytes_searched: sh.bytes.load(Ordering::Relaxed),
        elapsed_ms: sh.start.elapsed().as_millis() as u64,
        first_hit_ms: (first != u64::MAX).then_some(first / 1000),
        cancelled: sh.cancel.load(Ordering::Relaxed),
        aborted: sh.aborted.load(Ordering::Relaxed),
    })
}

/// Fuzzy file-name index (fzf-style multi-atom patterns, smart case).
pub struct FileIndex {
    paths: Vec<String>,
    hay: Vec<Utf32String>,
}

impl FileIndex {
    pub fn build(paths: Vec<String>) -> Self {
        let hay = paths.par_iter().map(|p| Utf32String::from(p.as_str())).collect();
        FileIndex { paths, hay }
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Best matches first: `(path, score, matched char indices into path)`. An empty query
    /// returns the first `limit` paths unscored.
    pub fn query(&self, q: &str, limit: usize) -> Vec<(String, u32, Vec<u32>)> {
        if limit == 0 {
            return Vec::new();
        }
        if q.trim().is_empty() {
            return self.paths.iter().take(limit).map(|p| (p.clone(), 0, Vec::new())).collect();
        }
        let pattern = Pattern::parse(q, CaseMatching::Smart, Normalization::Smart);
        let cfg = Config::DEFAULT.match_paths();
        const CHUNK: usize = 2048;
        let mut scored: Vec<(u32, usize)> = self
            .hay
            .par_chunks(CHUNK)
            .enumerate()
            .map_init(
                || Matcher::new(cfg.clone()),
                |m, (ci, chunk)| {
                    let mut v = Vec::new();
                    for (j, h) in chunk.iter().enumerate() {
                        if let Some(s) = pattern.score(h.slice(..), m) {
                            v.push((s, ci * CHUNK + j));
                        }
                    }
                    v
                },
            )
            .flatten()
            .collect();
        let better = |a: &(u32, usize), b: &(u32, usize)| {
            b.0.cmp(&a.0)
                .then_with(|| self.paths[a.1].len().cmp(&self.paths[b.1].len()))
                .then_with(|| a.1.cmp(&b.1))
        };
        if scored.len() > limit {
            scored.select_nth_unstable_by(limit, better);
            scored.truncate(limit);
        }
        scored.sort_unstable_by(better);
        let mut m = Matcher::new(cfg);
        scored
            .into_iter()
            .map(|(s, i)| {
                let mut idx = Vec::new();
                pattern.indices(self.hay[i].slice(..), &mut m, &mut idx);
                idx.sort_unstable();
                idx.dedup();
                (self.paths[i].clone(), s, idx)
            })
            .collect()
    }
}
