mod common;

use common::T;
use ct_core::*;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

fn q(p: &str) -> SearchQuery {
    SearchQuery { pattern: p.into(), ..Default::default() }
}

fn run(root: &std::path::Path, q: &SearchQuery) -> (Vec<SearchHit>, SearchStats) {
    let (tx, rx) = mpsc::sync_channel(1024);
    let cancel = Arc::new(AtomicBool::new(false));
    let root = root.to_path_buf();
    let q = q.clone();
    let h = std::thread::spawn(move || search_content(&root, &q, cancel, tx));
    let hits: Vec<_> = rx.iter().collect();
    (hits, h.join().unwrap().unwrap())
}

fn set(h: &[SearchHit]) -> BTreeSet<(String, u32, String)> {
    h.iter().map(|x| (x.path.clone(), x.line, x.text.clone())).collect()
}

fn tree() -> T {
    let t = T::new();
    t.write(".gitignore", b"ignored/\n*.log\n");
    t.write("a.txt", b"Hello World\nhello again\nnothing\n");
    t.write("sub/b.rs", b"fn hello() {}\r\nlet x = HELLO;\n");
    t.write("sub/deep/c.md", b"# Title\nhello hello hello\n");
    t.write("ignored/x.txt", b"hello ignored\n");
    t.write("run.log", b"hello log\n");
    t.write(".hidden/h.txt", b"hello hidden\n");
    t.write("bin.dat", b"hello\0binary\n");
    t.write("한글.txt", "안녕 hello 세계\n두번째 줄 세계 세계\n".as_bytes());
    t.commit_all("tree");
    t
}

#[test]
fn literal_case_sensitivity_and_ignore_rules() {
    let t = tree();
    let (hits, st) = run(t.path(), &q("hello"));
    let paths: BTreeSet<_> = hits.iter().map(|h| h.path.as_str()).collect();
    // ignored dir, *.log, hidden dir and binary files are skipped (ripgrep defaults)
    assert_eq!(paths, BTreeSet::from(["a.txt", "sub/b.rs", "sub/deep/c.md", "한글.txt"]));
    assert_eq!(st.matches as usize, hits.len());
    assert_eq!(hits.len(), 4, "{hits:?}");
    assert!(st.files_matched == 4 && st.files_searched >= 4 && st.bytes_searched > 0);
    assert!(st.first_hit_ms.is_some() && !st.cancelled && !st.aborted);

    let ci = SearchQuery { case_sensitive: false, ..q("hello") };
    let (hits, _) = run(t.path(), &ci);
    assert_eq!(hits.len(), 6);
    // literal is not regex
    let (hits, _) = run(t.path(), &q("fn hello()"));
    assert_eq!(hits.len(), 1);
    let (hits, _) = run(t.path(), &q(".*"));
    assert!(hits.is_empty());
}

#[test]
fn regex_ranges_columns_crlf_and_korean() {
    let t = tree();
    let rx = SearchQuery { regex: true, ..q(r"hel+o") };
    let (hits, _) = run(t.path(), &rx);
    let c = hits.iter().find(|h| h.path == "sub/deep/c.md").unwrap();
    assert_eq!((c.line, c.col), (2, 1));
    assert_eq!(c.ranges, vec![(0, 5), (6, 11), (12, 17)]);
    let b = hits.iter().find(|h| h.path == "sub/b.rs").unwrap();
    assert_eq!(b.text, "fn hello() {}", "CR of CRLF stripped");
    assert_eq!(b.col, 4);
    // byte (not char) columns for non-ASCII text
    let k = hits.iter().find(|h| h.path == "한글.txt").unwrap();
    assert_eq!(k.text, "안녕 hello 세계");
    assert_eq!(k.col as usize, "안녕 ".len() + 1);
    assert_eq!(&k.text[k.ranges[0].0 as usize..k.ranges[0].1 as usize], "hello");
    let (hits, _) = run(t.path(), &q("세계"));
    let l2 = hits.iter().find(|h| h.line == 2).unwrap();
    assert_eq!(l2.ranges.len(), 2);
    assert!(l2.ranges.iter().all(|r| &l2.text[r.0 as usize..r.1 as usize] == "세계"));
    // errors
    assert!(matches!(search_content(t.path(), &SearchQuery { regex: true, ..q("(unclosed") }, Arc::new(AtomicBool::new(false)), mpsc::sync_channel(1).0), Err(Error::Search(_))));
    assert!(matches!(search_content(t.path(), &q(""), Arc::new(AtomicBool::new(false)), mpsc::sync_channel(1).0), Err(Error::Invalid(_))));
}

#[test]
fn globs_include_and_exclude() {
    let t = tree();
    let inc = SearchQuery { globs: vec!["*.rs".into()], ..q("hello") };
    let (hits, _) = run(t.path(), &inc);
    assert_eq!(hits.iter().map(|h| h.path.as_str()).collect::<BTreeSet<_>>(), BTreeSet::from(["sub/b.rs"]));
    let exc = SearchQuery { globs: vec!["!*.rs".into(), "!sub/deep/**".into()], ..q("hello") };
    let (hits, _) = run(t.path(), &exc);
    assert_eq!(hits.iter().map(|h| h.path.as_str()).collect::<BTreeSet<_>>(), BTreeSet::from(["a.txt", "한글.txt"]));
    let bad = SearchQuery { globs: vec!["[".into()], ..q("hello") };
    assert!(matches!(search_content(t.path(), &bad, Arc::new(AtomicBool::new(false)), mpsc::sync_channel(1).0), Err(Error::Search(_))));
}

fn big_tree(files: usize, lines: usize) -> T {
    let t = T::new();
    for f in 0..files {
        let mut s = String::new();
        for l in 0..lines {
            s.push_str(&format!("line {l} of file {f} needle_{}\n", l % 7));
        }
        t.write(&format!("d{}/f{f}.txt", f % 20), s.as_bytes());
    }
    t
}

#[test]
fn cancel_before_and_during() {
    let t = big_tree(400, 400);
    let (tx, rx) = mpsc::sync_channel(8);
    let cancel = Arc::new(AtomicBool::new(true));
    let st = search_content(t.path(), &q("needle"), cancel, tx).unwrap();
    assert!(st.cancelled);
    assert_eq!(rx.try_iter().count() as u64, st.matches);
    assert!(st.matches < 400 * 400);

    // cancel after first hit; with a tiny channel the search can't have run ahead
    let (tx, rx) = mpsc::sync_channel(4);
    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    let root = t.path().to_path_buf();
    let h = std::thread::spawn(move || search_content(&root, &q("needle"), c2, tx));
    let _first = rx.recv().unwrap();
    let t0 = std::time::Instant::now();
    cancel.store(true, Ordering::Relaxed);
    let st = h.join().unwrap().unwrap();
    assert!(t0.elapsed().as_millis() < 500, "search must stop promptly after cancel, took {:?}", t0.elapsed());
    assert!(st.cancelled);
    assert!(st.matches < 400 * 400 / 2, "stopped early, got {}", st.matches);
}

#[test]
fn dropped_receiver_aborts_search() {
    let t = big_tree(100, 100);
    let (tx, rx) = mpsc::sync_channel(2);
    drop(rx);
    let st = search_content(t.path(), &q("needle"), Arc::new(AtomicBool::new(false)), tx).unwrap();
    assert!(st.aborted);
    assert!(st.matches < 10);
}

#[test]
fn bounded_channel_applies_backpressure_without_losing_hits() {
    let t = big_tree(30, 50);
    let total = 30 * 50;
    let (tx, rx) = mpsc::sync_channel(3);
    let root = t.path().to_path_buf();
    let h = std::thread::spawn(move || search_content(&root, &q("needle"), Arc::new(AtomicBool::new(false)), tx));
    let mut n = 0;
    for _ in rx.iter() {
        n += 1;
        if n % 100 == 0 {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    let st = h.join().unwrap().unwrap();
    assert_eq!(n, total);
    assert_eq!(st.matches as usize, total);
    assert!(!st.aborted);
}

#[test]
fn search_set_matches_ripgrep_on_fixture() {
    let t = tree();
    for (pat, extra) in [("hello", vec![]), ("HELLO", vec!["-i"]), ("hel+o", vec!["-e"])] {
        let mut sq = q(pat);
        let mut args: Vec<String> = vec!["-n".into(), "--no-heading".into(), "--color=never".into()];
        match extra.first().copied() {
            Some("-i") => {
                sq.case_sensitive = false;
                args.push("-i".into());
                args.push("-F".into());
            }
            Some("-e") => {
                sq.regex = true;
            }
            _ => args.push("-F".into()),
        }
        args.push("-e".into());
        args.push(pat.into());
        let out = std::process::Command::new("rg").current_dir(t.path()).args(&args).output().unwrap();
        let want: BTreeSet<(String, u32, String)> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| {
                let mut p = l.splitn(3, ':');
                (p.next().unwrap().to_string(), p.next().unwrap().parse().unwrap(), p.next().unwrap().trim_end_matches('\r').to_string())
            })
            .collect();
        let (hits, _) = run(t.path(), &sq);
        assert_eq!(set(&hits), want, "pattern {pat}");
    }
}
