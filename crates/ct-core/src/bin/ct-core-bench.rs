//! PLAN §4 measurements for ct-core + synthetic large repo generator.
//!
//!   ct-core-bench gen <dir> [--commits 20000] [--files 10000]   generate F2 (fast-import, in /tmp)
//!   ct-core-bench run <repo> [--runs 20]                          measure scenarios (p50/p95, ms)
//!   ct-core-bench fuzzy [--paths 50000] [--runs 20]               FileIndex query timing (in-memory)

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::Instant;

use ct_core::*;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const WORDS: &[&str] = &[
    "buffer", "render", "commit", "parser", "stream", "result", "handle", "config", "state", "index", "token", "value",
    "frame", "queue", "cache", "event", "layout", "color", "theme", "style", "event", "widget", "select", "filter",
    "update", "format", "length", "offset", "reader", "writer", "device", "signal", "memory", "thread", "worker",
];

fn line(r: &mut Rng) -> String {
    let a = WORDS[r.below(WORDS.len())];
    let b = WORDS[r.below(WORDS.len())];
    let n = r.below(1000);
    match r.below(4) {
        0 => format!("    let {a}_{n} = {b}.{a}({n}, \"{b}\");"),
        1 => format!("fn {a}_{b}_{n}(x: u32) -> u32 {{ x + {n} }}"),
        2 => format!("    // {a} {b} {n} handles the {a} before {b}"),
        _ => format!("    self.{a}.push({b}_{n});"),
    }
}

fn file_body(r: &mut Rng, lines: usize, tag: Option<&str>) -> String {
    let mut s = String::with_capacity(lines * 40);
    for i in 0..lines {
        s.push_str(&line(r));
        if let Some(t) = tag {
            if i == lines / 2 {
                s.push_str(&format!("\n// {t}"));
            }
        }
        s.push('\n');
    }
    s
}

fn put_file(w: &mut impl Write, path: &str, body: &str) {
    write!(w, "M 100644 inline {path}\ndata {}\n{body}\n", body.len()).unwrap();
}

/// F2: N commits, M files (~20 KB avg), a few 10k-char-line files, one commit rewriting a 10k-line file.
fn gen(dir: &Path, commits: usize, files: usize) {
    std::fs::create_dir_all(dir).unwrap();
    assert!(Command::new("git").current_dir(dir).args(["init", "-q", "-b", "main"]).status().unwrap().success());
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["fast-import", "--quiet", "--force"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut w = std::io::BufWriter::with_capacity(1 << 20, child.stdin.take().unwrap());
    let mut r = Rng(0x9E3779B97F4A7C15);
    let t0 = 1_500_000_000u64;
    let mut mark = 0u64;
    let mut commit = |w: &mut std::io::BufWriter<_>, msg: &str, ts: u64, mut body: Box<dyn FnMut(&mut std::io::BufWriter<_>)>| {
        mark += 1;
        write!(w, "commit refs/heads/main\nmark :{mark}\ncommitter Gen <gen@example.com> {ts} +0000\ndata {}\n{msg}\n", msg.len()).unwrap();
        if mark > 1 {
            writeln!(w, "from :{}", mark - 1).unwrap();
        }
        body(w);
        w.write_all(b"\n").unwrap();
    };
    // initial commit: all files
    let mut seed = Rng(r.next());
    let paths: Vec<String> = (0..files)
        .map(|i| format!("src/mod{:03}/sub{:02}/file{:05}.rs", i % 200, (i / 200) % 50, i))
        .collect();
    {
        let paths = paths.clone();
        commit(&mut w, "initial import", t0, Box::new(move |w| {
            for (i, p) in paths.iter().enumerate() {
                let lines = 100 + seed.below(900); // 100..1000 lines ≈ 4..40 KB
                let tag = if i % 500 == 7 { Some("NEEDLE_RARE_TOKEN") } else { None };
                put_file(w, p, &file_body(&mut seed, lines, tag));
            }
            // long-line files (10k chars per line)
            for k in 0..40 {
                let mut body = String::new();
                for _ in 0..30 {
                    while body.len() % 10_001 != 10_000 {
                        body.push_str(WORDS[seed.below(WORDS.len())]);
                        body.push(' ');
                        if body.len() % 10_001 > 10_000 { break; }
                    }
                    body.push('\n');
                }
                put_file(w, &format!("data/longlines{k:02}.txt"), &body);
            }
            // the big file used for the "10k-line diff" scenario
            put_file(w, "big/huge.txt", &file_body(&mut seed, 10_000, None));
        }));
    }
    // history: each commit edits one file (replace a few lines)
    for c in 1..commits.saturating_sub(1) {
        let idx = r.below(files);
        let p = paths[idx].clone();
        let mut local = Rng(r.next() | 1);
        let lines = 100 + local.below(900);
        let msg = format!("change {c}: touch file{idx}");
        commit(&mut w, &msg, t0 + c as u64 * 60, Box::new(move |w| {
            put_file(w, &p, &file_body(&mut local, lines, None));
        }));
    }
    // last commit: rewrite the whole 10k-line file → 10k-line diff
    {
        let mut local = Rng(0xDEADBEEF);
        commit(&mut w, "rewrite big/huge.txt", t0 + commits as u64 * 60, Box::new(move |w| {
            put_file(w, "big/huge.txt", &file_body(&mut local, 10_000, None));
        }));
    }
    w.flush().unwrap();
    drop(w);
    assert!(child.wait().unwrap().success());
    assert!(Command::new("git").current_dir(dir).args(["checkout", "-q", "-f", "main"]).status().unwrap().success());
    println!("generated {} commits, {} files in {}", commits, files, dir.display());
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i]
}

fn report(name: &str, budget: &str, mut v: Vec<f64>, cold: f64) {
    let (p50, p95, max) = (pct(&mut v, 0.5), pct(&mut v, 0.95), *v.last().unwrap());
    println!("{name:<52} cold {cold:>8.2}  p50 {p50:>8.2}  p95 {p95:>8.2}  max {max:>8.2} ms   [budget {budget}]");
}

fn time<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64() * 1000.0)
}

fn measure(runs: usize, mut f: impl FnMut() -> f64) -> (f64, Vec<f64>) {
    let cold = f();
    (cold, (0..runs).map(|_| f()).collect())
}

fn run(repo_path: &Path, runs: usize) {
    let (r0, open_ms) = time(|| Repo::open(repo_path).unwrap());
    let r = r0;
    println!("repo {}  (open {:.1} ms)", r.root.display(), open_ms);
    let total = String::from_utf8_lossy(&Command::new("git").current_dir(&r.root).args(["rev-list", "--count", "HEAD"]).output().unwrap().stdout).trim().to_string();
    let nfiles = r.list_files().unwrap().len();
    println!("commits: {total}, files: {nfiles}, runs: {runs}\n");

    // 1. cold open → first 200 commits
    let (c, v) = measure(runs, || {
        time(|| {
            let r = Repo::open(repo_path).unwrap();
            let l = r.log(&LogQuery { limit: 200, ..Default::default() }).unwrap();
            assert!(!l.is_empty());
        })
        .1
    });
    report("open + log first page (200 commits)", "<150", v, c);

    // 2. commit click → file list (random commits)
    let all = r.log(&LogQuery { limit: 3000, ..Default::default() }).unwrap();
    let mut rng = Rng(42);
    let (c, v) = measure(runs, || {
        let sha = all[rng.below(all.len())].sha.clone();
        time(|| r.diff(&Comparison::parent_of(&sha).unwrap(), &DiffOpts::default()).unwrap()).1
    });
    report("commit click → file list (diff, stat only)", "<60", v, c);

    // 3. big diff: find the largest single-file diff among the latest 300 commits
    let mut best: Option<(String, String, u32)> = None;
    for cm in all.iter().take(300) {
        if let Ok(s) = r.diff(&Comparison::parent_of(&cm.sha).unwrap(), &DiffOpts::default()) {
            for f in s.files.iter().filter(|f| !f.binary) {
                let n = f.add + f.del;
                if best.as_ref().is_none_or(|b| n > b.2) {
                    best = Some((cm.sha.clone(), f.path.clone(), n));
                }
            }
        }
    }
    if let Some((sha, path, n)) = best {
        let cmp = Comparison::parent_of(&sha).unwrap();
        let (c, v) = measure(runs, || {
            time(|| {
                let f = r.file_diff(&cmp, &path, &DiffOpts::default()).unwrap();
                assert!(!f.hunks.is_empty());
            })
            .1
        });
        report(&format!("file_diff of largest diff ({n} changed lines)"), "<100", v, c);
    }

    // 4. blame of the largest tracked file
    let files = r.list_files().unwrap();
    if let Some(big) = files.iter().max_by_key(|p| std::fs::metadata(r.root.join(p)).map(|m| m.len()).unwrap_or(0)) {
        if big.ends_with(".rs") || big.ends_with(".txt") || big.ends_with(".md") || big.ends_with(".py") {
            let head = r.head().unwrap();
            let (c, v) = measure(runs.min(10), || {
                time(|| {
                    let _ = r.blame(&Treeish::Commit(head.clone()), big, None);
                })
                .1
            });
            report(&format!("blame {big}"), "-", v, c);
        }
    }

    // 5. content search (warm cache after first run)
    let search = |pattern: &str| -> (f64, f64, u64) {
        let (tx, rx) = mpsc::sync_channel(1024);
        let root = r.root.clone();
        let q = SearchQuery { pattern: pattern.into(), ..Default::default() };
        let t = Instant::now();
        let h = std::thread::spawn(move || search_content(&root, &q, Arc::new(AtomicBool::new(false)), tx));
        let mut first = None;
        let mut n = 0u64;
        for _ in rx.iter() {
            n += 1;
            if first.is_none() {
                first = Some(t.elapsed().as_secs_f64() * 1000.0);
            }
        }
        let _ = h.join().unwrap().unwrap();
        (first.unwrap_or(f64::NAN), t.elapsed().as_secs_f64() * 1000.0, n)
    };
    for (label, pat) in [("common literal (many hits)", "self"), ("rare literal (few hits)", "NEEDLE_RARE_TOKEN"), ("no match", "zzzz_no_such_token_qq")] {
        let (cf, ct, hits) = search(pat);
        let mut firsts = Vec::new();
        let mut totals = Vec::new();
        for _ in 0..runs {
            let (f, t, _) = search(pat);
            firsts.push(f);
            totals.push(t);
        }
        if firsts.iter().all(|x| x.is_nan()) {
            report(&format!("search {label}: complete [{hits} hits]"), "<400", totals, ct);
        } else {
            report(&format!("search {label}: first hit [{hits} hits]"), "<100", firsts, cf);
            report(&format!("search {label}: complete"), "<400", totals, ct);
        }
    }

    // 6. cancel latency: cancel after the first hit of a many-hit search
    {
        let mut v = Vec::new();
        for _ in 0..runs {
            let (tx, rx) = mpsc::sync_channel(16);
            let cancel = Arc::new(AtomicBool::new(false));
            let (root, c2) = (r.root.clone(), cancel.clone());
            let h = std::thread::spawn(move || search_content(&root, &SearchQuery { pattern: "e".into(), ..Default::default() }, c2, tx));
            let _ = rx.recv();
            let t = Instant::now();
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            drop(rx);
            let _ = h.join().unwrap();
            v.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        report("search cancel latency (flag set → function returned)", "immediate", v, 0.0);
    }

    // 7. fuzzy over this repo's real paths
    fuzzy_run(files, runs);
}

fn fuzzy_run(paths: Vec<String>, runs: usize) {
    let n = paths.len();
    let (idx, build_ms) = time(|| FileIndex::build(paths.clone()));
    println!("\nFileIndex: {n} paths, build {build_ms:.1} ms");
    let queries = ["m", "main", "src", "mod12", "file00042", "sub07 file1", "rs", "mod1 sub2 fi", "z", "zzzzqq", "Cargo", "readme"];
    let mut all = Vec::new();
    let mut cold = 0.0;
    for (qi, q) in queries.iter().enumerate() {
        let (_, t) = time(|| idx.query(q, 50));
        if qi == 0 {
            cold = t;
        }
        for _ in 0..runs {
            all.push(time(|| idx.query(q, 50)).1);
        }
    }
    report(&format!("fuzzy query over {n} paths ({} queries × {runs})", queries.len()), "<16", all, cold);
}

fn main() {
    let mut a = std::env::args().skip(1);
    match a.next().as_deref() {
        Some("gen") => {
            let dir = PathBuf::from(a.next().expect("gen <dir>"));
            let (mut commits, mut files) = (20_000usize, 10_000usize);
            while let Some(o) = a.next() {
                match o.as_str() {
                    "--commits" => commits = a.next().unwrap().parse().unwrap(),
                    "--files" => files = a.next().unwrap().parse().unwrap(),
                    x => panic!("unknown {x}"),
                }
            }
            gen(&dir, commits, files);
        }
        Some("run") => {
            let dir = PathBuf::from(a.next().expect("run <repo>"));
            let mut runs = 20;
            while let Some(o) = a.next() {
                if o == "--runs" {
                    runs = a.next().unwrap().parse().unwrap();
                }
            }
            run(&dir, runs);
        }
        Some("fuzzy") => {
            let (mut n, mut runs) = (50_000usize, 20usize);
            while let Some(o) = a.next() {
                match o.as_str() {
                    "--paths" => n = a.next().unwrap().parse().unwrap(),
                    "--runs" => runs = a.next().unwrap().parse().unwrap(),
                    x => panic!("unknown {x}"),
                }
            }
            let paths: Vec<String> = (0..n)
                .map(|i| format!("crates/pkg{:03}/src/mod{:02}/sub{:02}/file_{:05}.rs", i % 211, i % 37, (i / 7) % 53, i))
                .collect();
            fuzzy_run(paths, runs);
        }
        _ => eprintln!("usage: ct-core-bench gen <dir> | run <repo> [--runs N] | fuzzy [--paths N] [--runs N]"),
    }
}
