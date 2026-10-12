//! Remote repositories through a fake `ssh` (tests/support/fake-ssh.sh, via CT_SSH_BIN) that runs the
//! command string locally with `sh -c`, so the whole ssh path (quoting, limiter, kill, caps) is
//! exercised against a real temp repo. The real-ssh test is `#[ignore]`d (CT_LIVE_SSH=1).
mod common;

use common::{canon, git_in, T};
use ct_core::remote::{self, RemoteSpec, Transport};
use ct_core::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

struct Env {
    log: PathBuf,
    active: PathBuf,
    _dir: tempfile::TempDir,
}

fn env() -> &'static Env {
    static E: OnceLock<Env> = OnceLock::new();
    E.get_or_init(|| {
        let dir = tempfile::Builder::new().prefix("ct-r").tempdir().unwrap();
        let (log, active) = (dir.path().join("ssh.log"), dir.path().join("active"));
        std::fs::create_dir(&active).unwrap();
        std::fs::write(&log, "").unwrap();
        let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake-ssh.sh");
        std::env::set_var("CT_SSH_BIN", fake);
        std::env::set_var("CT_FAKE_SSH_LOG", &log);
        std::env::set_var("CT_FAKE_SSH_ACTIVE", &active);
        std::env::set_var("XDG_RUNTIME_DIR", dir.path());
        std::env::set_var("XDG_CACHE_HOME", dir.path().join("cache"));
        Env { log, active, _dir: dir }
    })
}

fn spec(host: &str) -> RemoteSpec {
    env();
    RemoteSpec { name: format!("t-{host}"), host: host.into(), user: Some("tester".into()), port: None, identity_file: None, transport: Transport::Ssh, trust_new_host: false }
}

/// A repo whose directory name contains a space and a single quote.
fn nasty_repo() -> T {
    let dir = tempfile::Builder::new().prefix("my repo's ").tempdir().unwrap();
    git_in(dir.path(), &["init", "-q", "-b", "main"]);
    T { dir }
}

fn path_str(t: &T) -> String {
    canon(t.path()).to_str().unwrap().to_string()
}

fn log_lines_for(host: &str) -> Vec<String> {
    std::fs::read_to_string(&env().log).unwrap().lines().filter(|l| l.contains(&format!("target=tester@{host} ")) && l.contains("ctl= |")).map(String::from).collect()
}

fn pgrep(pat: &str) -> bool {
    std::process::Command::new("pgrep").args(["-f", pat]).output().is_ok_and(|o| !o.stdout.is_empty())
}

fn fixture() -> T {
    let t = nasty_repo();
    t.write("a.txt", b"one\ntwo\nthree\n");
    t.write("dir/sp ace.txt", b"hello world\nit's here\n");
    t.write("dir/q'uote.rs", b"fn main() { println!(\"$(id)\"); }\n");
    t.commit_all("first");
    t.write("a.txt", b"one\nTWO\nthree\nfour\n");
    t.git(&["mv", "dir/sp ace.txt", "dir/moved.txt"]);
    t.commit_all("second: change a, rename sp ace");
    t.git(&["tag", "v1"]);
    t
}

#[test]
fn remote_matches_local_for_the_whole_api() {
    let t = fixture();
    let local = t.repo();
    let sp = spec("repo-api");
    let r = Repo::open_remote(&sp, &path_str(&t)).unwrap();
    assert!(r.is_remote() && !local.is_remote());
    assert_eq!(r.remote().unwrap(), &sp);
    assert_eq!(r.root, local.root);
    assert_eq!(r.git_dir, local.git_dir);
    assert_eq!(r.head().unwrap(), local.head().unwrap());
    assert_eq!(r.resolve("v1").unwrap(), local.resolve("v1").unwrap());
    assert_eq!(r.refs().unwrap(), local.refs().unwrap());

    let (lc, rc) = (local.log(&LogQuery::default()).unwrap(), r.log(&LogQuery::default()).unwrap());
    assert_eq!(lc.len(), 2);
    assert_eq!(lc, rc);
    let q = LogQuery { message: Some("RENAME".into()), ..Default::default() };
    assert_eq!(r.log(&q).unwrap(), local.log(&q).unwrap());
    let q = LogQuery { path: Some("dir/q'uote.rs".into()), ..Default::default() };
    assert_eq!(r.log(&q).unwrap().len(), 1, "path with a quote must survive the shell");
    assert_eq!(r.file_history("a.txt", None, 10).unwrap(), local.file_history("a.txt", None, 10).unwrap());

    let head = rc[0].sha.clone();
    let cmp = Comparison::parent_of(&head).unwrap();
    let (ld, rd) = (local.diff(&cmp, &DiffOpts::default()).unwrap(), r.diff(&cmp, &DiffOpts::default()).unwrap());
    assert_eq!(ld, rd);
    assert!(rd.files.iter().any(|f| f.status == Status::Renamed));
    for f in &rd.files {
        assert_eq!(local.file_diff(&cmp, &f.path, &DiffOpts::default()).unwrap(), r.file_diff(&cmp, &f.path, &DiffOpts::default()).unwrap());
    }

    assert_eq!(r.show_file(&head, "a.txt").unwrap(), b"one\nTWO\nthree\nfour\n");
    assert_eq!(r.show_file("HEAD~1", "dir/sp ace.txt").unwrap(), local.show_file("HEAD~1", "dir/sp ace.txt").unwrap());
    assert_eq!(
        r.blame(&Treeish::Commit(head.clone()), "a.txt", None).unwrap(),
        local.blame(&Treeish::Commit(head.clone()), "a.txt", None).unwrap()
    );

    // working tree: modified tracked file + untracked file
    t.write("a.txt", b"one\nTWO\nthree\nfour\nfive\n");
    t.write("new file's.txt", b"untracked\n");
    assert_eq!(r.read_worktree_file("a.txt").unwrap(), b"one\nTWO\nthree\nfour\nfive\n");
    assert_eq!(r.read_worktree_file("new file's.txt").unwrap(), b"untracked\n");
    assert!(matches!(r.read_worktree_file("nope.txt"), Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound));
    assert!(matches!(r.read_worktree_file("../x"), Err(Error::Invalid(_))));
    assert!(matches!(r.read_worktree_file("/etc/passwd"), Err(Error::Invalid(_))));
    let wt = Comparison::commit_vs_worktree(&head).unwrap();
    assert_eq!(r.diff(&wt, &DiffOpts::default()).unwrap(), local.diff(&wt, &DiffOpts::default()).unwrap());
    // uncommitted lines carry "now" as their time: compare without it
    let norm = |mut v: Vec<BlameLine>| {
        v.iter_mut().filter(|b| b.sha.bytes().all(|c| c == b'0')).for_each(|b| b.time = 0);
        v
    };
    assert_eq!(
        norm(r.blame(&Treeish::Worktree, "a.txt", None).unwrap()),
        norm(local.blame(&Treeish::Worktree, "a.txt", None).unwrap())
    );
    let mut lf = local.list_files().unwrap();
    let mut rf = r.list_files().unwrap();
    lf.sort();
    rf.sort();
    assert_eq!(lf, rf);
    assert!(rf.contains(&"new file's.txt".to_string()));

    // git_output goes the same way
    let out = r.git_output(&["status", "--porcelain=v2", "-z", "-uall"], &GitCall::default()).unwrap();
    assert_eq!(out, local.git_output(&["status", "--porcelain=v2", "-z", "-uall"], &GitCall::default()).unwrap());
    assert!(out.windows(14).any(|w| w == b"new file's.txt"));
    // exit code 1 accepted on request
    let call = GitCall { ok_codes: vec![1], ..Default::default() };
    r.git_output(&["diff", "--quiet", "--exit-code"], &call).unwrap();
    assert!(r.git_output(&["diff", "--quiet", "--exit-code"], &GitCall::default()).is_err());
}

#[test]
fn open_remote_errors_are_typed() {
    let t = fixture();
    let plain = tempfile::tempdir().unwrap();
    let e = Repo::open_remote(&spec("repo-err"), plain.path().to_str().unwrap()).unwrap_err();
    assert!(matches!(&e, Error::Remote { kind: RemoteErrorKind::NotARepo, .. }), "{e:?}");
    assert!(e.to_string().contains("not a git repository"), "{e}");
    let e = Repo::open_remote(&spec("repo-err"), "/definitely/not/here").unwrap_err();
    assert!(matches!(&e, Error::Remote { kind: RemoteErrorKind::NotARepo, .. }), "{e:?}");
    // simulated ssh-level failures
    for (host, kind) in [
        ("hk-unknown", RemoteErrorKind::HostKeyUnknown),
        ("hk-changed", RemoteErrorKind::HostKeyChanged),
        ("auth-fail", RemoteErrorKind::AuthFailed),
        ("no-resolve", RemoteErrorKind::Unreachable),
        ("refused", RemoteErrorKind::Unreachable),
        ("net-timeout", RemoteErrorKind::Timeout),
    ] {
        let e = Repo::open_remote(&spec(host), &path_str(&t)).unwrap_err();
        assert!(matches!(&e, Error::Remote { kind: k, .. } if *k == kind), "{host}: {e:?}");
        let e2 = remote::test_connection(&spec(host), Duration::from_secs(10)).unwrap_err();
        assert!(matches!(&e2, Error::Remote { kind: k, .. } if *k == kind), "{host}: {e2:?}");
    }
    let e = Repo::open_remote(&spec("hk-unknown"), "/x").unwrap_err();
    assert!(e.to_string().contains("Trust new host key"), "{e}");
    // invalid specs never reach ssh
    let mut bad = spec("repo-err");
    bad.host = "-oProxyCommand=touch /tmp/pwned".into();
    assert!(matches!(Repo::open_remote(&bad, "/x"), Err(Error::Invalid(_))));
    assert!(!Path::new("/tmp/pwned").exists());
    assert!(matches!(remote::list_dir(&bad, "/", None), Err(Error::Invalid(_))));
    assert!(matches!(remote::test_connection(&bad, Duration::from_secs(5)), Err(Error::Invalid(_))));
}

#[test]
fn ssh_flags_reach_the_command_line() {
    let t = fixture();
    let mut s = spec("repo-flags");
    s.port = Some(2222);
    s.identity_file = Some("/tmp/some key".into());
    s.transport = Transport::TailscaleNc;
    s.trust_new_host = true;
    // the fake only parses the flag *shapes*; -i/-p/-o all take one value
    Repo::open_remote(&s, &path_str(&t)).unwrap();
    let l = log_lines_for("repo-flags");
    let first = &l[0];
    for want in ["-p 2222", "-i /tmp/some key", "IdentitiesOnly=yes", "ProxyCommand=tailscale nc %h %p", "StrictHostKeyChecking=accept-new", "BatchMode=yes", "ControlMaster=auto", "ControlPersist=120", "ForwardAgent=no", "-T"] {
        assert!(first.contains(want), "missing {want:?} in {first}");
    }
    assert!(!first.contains("StrictHostKeyChecking=no") && !first.contains("StrictHostKeyChecking=yes"));
    let s2 = spec("repo-flags2");
    Repo::open_remote(&s2, &path_str(&t)).unwrap();
    let l = log_lines_for("repo-flags2");
    assert!(l[0].contains("StrictHostKeyChecking=yes") && !l[0].contains("ProxyCommand") && !l[0].contains("-i "), "{}", l[0]);
    // the remote command is a single `sh -c '...'` word
    assert!(l[0].contains("cmd=sh -c 'cd "), "{}", l[0]);
}

#[test]
fn test_connection_reports_host_home_git() {
    let info = remote::test_connection(&spec("repo-info"), Duration::from_secs(10)).unwrap();
    assert!(!info.hostname.is_empty());
    assert_eq!(info.home, std::env::var("HOME").unwrap());
    assert!(info.git_version.chars().next().unwrap().is_ascii_digit(), "{}", info.git_version);
    assert!(info.latency_ms < 5000);
    // close is a no-op for the fake but must return quickly and never panic
    let t0 = Instant::now();
    remote::close_connection(&spec("repo-info"));
    assert!(t0.elapsed() < Duration::from_secs(2) + Duration::from_millis(500));
    assert!(std::fs::read_to_string(&env().log).unwrap().contains("ctl=exit | target=tester@repo-info"));
    // timeouts are typed
    let e = remote::test_connection(&spec("delay-5"), Duration::from_millis(300)).unwrap_err();
    assert!(matches!(e, Error::Remote { kind: RemoteErrorKind::Timeout, .. }), "{e:?}");
}

#[test]
fn list_dir_marks_repos_and_skips_hidden() {
    let parent = tempfile::Builder::new().prefix("lsd's dir ").tempdir().unwrap();
    for d in ["zeta", "Alpha", "with space", ".hidden", "it's"] {
        std::fs::create_dir(parent.path().join(d)).unwrap();
    }
    std::fs::write(parent.path().join("file.txt"), "x").unwrap();
    git_in(&parent.path().join("Alpha"), &["init", "-q"]);
    git_in(&parent.path().join("it's"), &["init", "-q"]);
    let v = remote::list_dir(&spec("repo-ls"), parent.path().to_str().unwrap(), None).unwrap();
    let got: Vec<(&str, bool)> = v.iter().map(|e| (e.name.as_str(), e.is_git_repo)).collect();
    assert_eq!(got, vec![("Alpha", true), ("it's", true), ("with space", false), ("zeta", false)]);
    // `~` expands remotely
    let home = remote::list_dir(&spec("repo-ls"), "~", None).unwrap();
    assert!(home.len() <= 500);
    assert!(home.iter().all(|e| !e.name.starts_with('.')));
    assert!(matches!(remote::list_dir(&spec("repo-ls"), "/no/such/dir", None), Err(Error::Remote { kind: RemoteErrorKind::NotARepo, .. })));
    // capped at 500
    let many = tempfile::tempdir().unwrap();
    for i in 0..620 {
        std::fs::create_dir(many.path().join(format!("d{i:04}"))).unwrap();
    }
    assert_eq!(remote::list_dir(&spec("repo-ls"), many.path().to_str().unwrap(), None).unwrap().len(), 500);
}

fn collect(r: &Repo, q: &SearchQuery) -> (Vec<SearchHit>, SearchStats) {
    let (tx, rx) = sync_channel(256);
    let stats = r.search_content(q, Arc::new(AtomicBool::new(false)), tx).unwrap();
    let mut hits: Vec<SearchHit> = rx.try_iter().collect();
    hits.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    (hits, stats)
}

#[test]
fn remote_search_files_and_content() {
    let t = fixture();
    t.write("untracked.txt", b"hello from untracked\n");
    t.write(".gitignore", b"ignored.txt\n");
    t.write("ignored.txt", b"hello ignored\n");
    t.write("bin.dat", b"hello\0binary");
    t.commit_all("more");
    t.write("later untracked.txt", b"hello later\n");
    let local = t.repo();
    let r = Repo::open_remote(&spec("repo-search"), &path_str(&t)).unwrap();

    let q = SearchQuery { pattern: "hello".into(), ..Default::default() };
    let (rh, st) = collect(&r, &q);
    let (lh, _) = collect(&local, &q);
    let paths: Vec<&str> = rh.iter().map(|h| h.path.as_str()).collect();
    assert_eq!(paths, vec!["dir/moved.txt", "later untracked.txt", "untracked.txt"], "{paths:?}");
    assert_eq!(rh, lh, "remote and local searches must agree here");
    assert_eq!(rh[0].ranges, vec![(0, 5)]);
    assert_eq!(st.matches, 3);
    assert_eq!(st.files_matched, 3);
    assert!(!st.cancelled && !st.aborted);

    // case, regex (ERE), globs
    let q = SearchQuery { pattern: "HELLO".into(), case_sensitive: false, ..Default::default() };
    assert_eq!(collect(&r, &q).0.len(), 3);
    let q = SearchQuery { pattern: "HELLO".into(), case_sensitive: true, ..Default::default() };
    assert_eq!(collect(&r, &q).0.len(), 0);
    let q = SearchQuery { pattern: "wor(ld|ms)".into(), regex: true, ..Default::default() };
    assert_eq!(collect(&r, &q).0.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), vec!["dir/moved.txt"]);
    let q = SearchQuery { pattern: "it's".into(), ..Default::default() };
    assert_eq!(collect(&r, &q).0.len(), 1, "quote in the pattern");
    let q = SearchQuery { pattern: "$(id)".into(), ..Default::default() };
    assert_eq!(collect(&r, &q).0.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), vec!["dir/q'uote.rs"]);
    let q = SearchQuery { pattern: "hello".into(), globs: vec!["untracked.txt".into()], ..Default::default() };
    assert_eq!(collect(&r, &q).0.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), vec!["untracked.txt"]);
    let q = SearchQuery { pattern: "hello".into(), globs: vec!["!dir/**".into()], ..Default::default() };
    assert_eq!(collect(&r, &q).0.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), vec!["later untracked.txt", "untracked.txt"]);
    assert!(matches!(r.search_content(&SearchQuery::default(), Arc::new(AtomicBool::new(false)), sync_channel(1).0), Err(Error::Invalid(_))));
    let nl = SearchQuery { pattern: "a\nb".into(), ..Default::default() };
    assert!(matches!(r.search_content(&nl, Arc::new(AtomicBool::new(false)), sync_channel(1).0), Err(Error::Search(_))));

    // file-name search: list_files + the in-process fuzzy index
    let idx = FileIndex::build(r.list_files().unwrap());
    let top = idx.query("moved", 3);
    assert_eq!(top[0].0, "dir/moved.txt");
    assert!(idx.query("zzzzqqq", 3).is_empty());
}

#[test]
fn remote_search_cancel_and_stall_stop_the_child() {
    let t = nasty_repo();
    // a big file with many matching lines so grep has lots to say
    let line = "needle in a haystack line\n".repeat(200_000);
    t.write("big.txt", line.as_bytes());
    t.commit_all("big");
    let r = Repo::open_remote(&spec("repo-big"), &path_str(&t)).unwrap();
    let q = SearchQuery { pattern: "needle".into(), ..Default::default() };

    // consumer drops the receiver after a few hits → search aborts, ssh child is reaped
    let (tx, rx) = sync_channel(4);
    let h = {
        let r = r.clone();
        let q = q.clone();
        std::thread::spawn(move || r.search_content(&q, Arc::new(AtomicBool::new(false)), tx))
    };
    for _ in 0..3 {
        rx.recv().unwrap();
    }
    drop(rx);
    let st = h.join().unwrap().unwrap();
    assert!(st.aborted);
    assert!(st.matches < 200_000);

    // cancel flag
    let flag = Arc::new(AtomicBool::new(false));
    let (tx, rx) = sync_channel(4);
    let (r2, q2, f2) = (r.clone(), q.clone(), flag.clone());
    let h = std::thread::spawn(move || r2.search_content(&q2, f2, tx));
    rx.recv().unwrap();
    flag.store(true, Ordering::Relaxed);
    let st = h.join().unwrap().unwrap();
    assert!(st.cancelled);
    drop(rx);
}

#[test]
fn per_call_timeout_and_cancel_kill_ssh_children() {
    let t = fixture();
    let p = path_str(&t);
    // slowcmd-33: opening is fast, `head()` (rev-parse --verify) makes the fake ssh sleep 33 s
    let slow = Repo::open_remote(&spec("slowcmd-33"), &p).unwrap().with_timeout(Duration::from_millis(400));
    let t0 = Instant::now();
    let e = slow.head().unwrap_err();
    assert!(matches!(e, Error::Timeout(_)), "{e:?}");
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    std::thread::sleep(Duration::from_millis(200));
    assert!(!pgrep("sleep 33"), "timed-out ssh child must be killed with its group");

    let flag = Arc::new(AtomicBool::new(false));
    let cancelled = Repo::open_remote(&spec("slowcmd-34"), &p).unwrap().with_cancel(flag.clone());
    let f2 = flag.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        f2.store(true, Ordering::Relaxed);
    });
    let t0 = Instant::now();
    let e = cancelled.head().unwrap_err();
    assert!(matches!(e, Error::Cancelled), "{e:?}");
    assert!(t0.elapsed() < Duration::from_secs(3));
    std::thread::sleep(Duration::from_millis(200));
    assert!(!pgrep("sleep 34"), "cancelled ssh child must be killed with its group");
}

#[test]
fn output_cap_applies_to_remote_output() {
    let t = nasty_repo();
    t.write("big.bin", &vec![b'x'; 3 * 1024 * 1024]);
    t.commit_all("big");
    let r = Repo::open_remote(&spec("repo-cap"), &path_str(&t)).unwrap().with_max_output(100_000);
    let e = r.show_file("HEAD", "big.bin").unwrap_err();
    assert!(matches!(e, Error::TooLarge { limit: 100_000, .. }), "{e:?}");
    let e = r.read_worktree_file("big.bin").unwrap_err();
    assert!(matches!(e, Error::TooLarge { .. }), "{e:?}");
    // the handle stays usable
    assert!(r.head().is_ok());
}

/// Fake-ssh calls running right now (killed ones leave their marker file behind: check the pid).
fn live_fake_ssh(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .filter(|e| {
                    let pid = e.file_name().to_string_lossy().into_owned();
                    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s[s.rfind(')').unwrap_or(0)..].starts_with(") Z"))
                })
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn never_more_than_the_slot_limit_of_ssh_children() {
    let t = fixture();
    let p = path_str(&t);
    // each head() call holds its slot for ~0.2 s
    let slow = Repo::open_remote(&spec("slowcmd-0.2"), &p).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (stop, dir) = (stop.clone(), env().active.clone());
        std::thread::spawn(move || {
            let mut max = 0usize;
            while !stop.load(Ordering::Relaxed) {
                max = max.max(live_fake_ssh(&dir).max(git_slots().in_use()));
                std::thread::sleep(Duration::from_millis(2));
            }
            max
        })
    };
    let handles: Vec<_> = (0..50)
        .map(|_| {
            let r = slow.clone();
            std::thread::spawn(move || r.head().map(|_| ()))
        })
        .collect();
    for h in handles {
        h.join().unwrap().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    let max = sampler.join().unwrap();
    assert!(max >= 2, "the sampler saw no concurrency at all ({max})");
    assert!(max <= MAX_GIT_PROCS, "{max} ssh children alive at once (limit {MAX_GIT_PROCS})");
}

#[test]
fn records_cache_downloads_once_and_follows_changes() {
    let t = fixture();
    let r = Repo::open_remote(&spec("repo-rec"), &path_str(&t)).unwrap();
    let cache = tempfile::tempdir().unwrap();
    assert_eq!(r.fetch_records_cache_at(cache.path()).unwrap(), None, "no log yet");

    let logdir = t.path().join(".git/codetrail");
    std::fs::create_dir_all(&logdir).unwrap();
    std::fs::write(logdir.join("log.ct"), b"frame-1").unwrap();
    let cat_calls = || log_lines_for("repo-rec").iter().filter(|l| l.contains("log.ct") && l.contains("exec cat")).count();
    let before = cat_calls();
    let p = r.fetch_records_cache_at(cache.path()).unwrap().unwrap();
    assert!(p.starts_with(cache.path()));
    assert_eq!(std::fs::read(&p).unwrap(), b"frame-1");
    assert_eq!(cat_calls(), before + 1);
    // unchanged → only the cheap stat call
    let p2 = r.fetch_records_cache_at(cache.path()).unwrap().unwrap();
    assert_eq!(p, p2);
    assert_eq!(cat_calls(), before + 1, "unchanged remote file must not be downloaded again");
    // changed size → re-download
    std::fs::write(logdir.join("log.ct"), b"frame-1+frame-2").unwrap();
    r.fetch_records_cache_at(cache.path()).unwrap().unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"frame-1+frame-2");
    assert_eq!(cat_calls(), before + 2);

    // default location honours XDG_CACHE_HOME and the result is readable by ct-store's reader path
    let d = r.fetch_records_cache().unwrap().unwrap();
    assert!(d.starts_with(std::env::var("XDG_CACHE_HOME").unwrap()), "{d:?}");
    // local repos return the real file unchanged
    let l = t.repo().fetch_records_cache().unwrap().unwrap();
    assert_eq!(l, canon(&logdir).join("log.ct"));
}

#[test]
fn records_cache_is_bounded() {
    let cache = tempfile::tempdir().unwrap();
    let mut keep = Vec::new();
    for i in 0..11 {
        let t = nasty_repo();
        t.write("f", b"x");
        t.commit_all("x");
        let d = t.path().join(".git/codetrail");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("log.ct"), format!("log {i}")).unwrap();
        let r = Repo::open_remote(&spec("repo-lru"), &path_str(&t)).unwrap();
        let p = r.fetch_records_cache_at(cache.path()).unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), format!("log {i}"));
        keep.push((t, p));
        std::thread::sleep(Duration::from_millis(15));
    }
    let n = std::fs::read_dir(cache.path()).unwrap().count();
    assert_eq!(n, 8, "cache dirs beyond 8 must be evicted");
    assert!(keep.last().unwrap().1.exists(), "newest entry survives");
    assert!(!keep[0].1.exists(), "oldest entry is evicted");

    // size cap: a 65 MiB (sparse) file is refused without downloading
    let t = nasty_repo();
    t.write("f", b"x");
    t.commit_all("x");
    let d = t.path().join(".git/codetrail");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::File::create(d.join("log.ct")).unwrap().set_len(65 * 1024 * 1024).unwrap();
    let r = Repo::open_remote(&spec("repo-lru"), &path_str(&t)).unwrap();
    let e = r.fetch_records_cache_at(cache.path()).unwrap_err();
    assert!(matches!(e, Error::TooLarge { .. }), "{e:?}");
}

/// Real OpenSSH to 127.0.0.1: BatchMode + known_hosts only. Needs sshd on :22 that accepts your key.
#[test]
#[ignore = "needs CT_LIVE_SSH=1 and key-based ssh to 127.0.0.1"]
fn live_ssh_localhost() {
    if std::env::var("CT_LIVE_SSH").as_deref() != Ok("1") {
        eprintln!("CT_LIVE_SSH!=1; skipped");
        return;
    }
    std::env::remove_var("CT_SSH_BIN");
    let t = fixture();
    let mut s = RemoteSpec { name: "live".into(), host: "127.0.0.1".into(), user: None, port: None, identity_file: None, transport: Transport::Ssh, trust_new_host: false };
    if let Ok(u) = std::env::var("USER") {
        s.user = Some(u);
    }
    let t0 = Instant::now();
    let info = remote::test_connection(&s, Duration::from_secs(20)).unwrap();
    eprintln!("live: first call (connect + auth) {} ms, host {} git {}", info.latency_ms, info.hostname, info.git_version);
    let r = Repo::open_remote(&s, &path_str(&t)).unwrap();
    let mut samples = Vec::new();
    for _ in 0..10 {
        let t1 = Instant::now();
        r.head().unwrap();
        samples.push(t1.elapsed().as_millis());
    }
    eprintln!("live: 10 calls over the ControlMaster ms: {samples:?}; total {:?}", t0.elapsed());
    assert_eq!(r.log(&LogQuery::default()).unwrap().len(), 2);
    remote::close_connection(&s);
}

#[test]
fn measure_overhead_with_fake_ssh() {
    let t = fixture();
    let local = t.repo();
    let r = Repo::open_remote(&spec("repo-bench"), &path_str(&t)).unwrap();
    let time = |f: &dyn Fn()| {
        let t0 = Instant::now();
        for _ in 0..20 {
            f();
        }
        t0.elapsed().as_secs_f64() * 1000.0 / 20.0
    };
    let (l, rm) = (time(&|| drop(local.head().unwrap())), time(&|| drop(r.head().unwrap())));
    eprintln!("overhead: local head {l:.1} ms, remote(fake ssh) head {rm:.1} ms, delta {:.1} ms", rm - l);
}

// ───────────────────────── review-fix regressions (t033) ─────────────────────────

fn mkfifo(p: &Path) {
    assert!(std::process::Command::new("mkfifo").arg(p).status().unwrap().success());
}

/// Review #2: remote worktree reads never follow symlinks out of the repository.
#[test]
fn remote_read_refuses_symlinks_fifos_and_oversize() {
    let t = fixture();
    let r = Repo::open_remote(&spec("repo-symlink"), &path_str(&t)).unwrap();
    let root = t.path();
    std::os::unix::fs::symlink("/etc/passwd", root.join("link")).unwrap();
    std::os::unix::fs::symlink("/etc", root.join("dir-link")).unwrap();
    std::os::unix::fs::symlink("dir", root.join("inner-link")).unwrap();
    std::os::unix::fs::symlink("nowhere-at-all", root.join("dangling")).unwrap();
    mkfifo(&root.join("pipe"));

    // final symlink → typed error carrying the target, content never read
    match r.read_worktree_file("link") {
        Err(Error::Invalid(m)) => assert_eq!(m, "symlink: /etc/passwd"),
        o => panic!("{o:?}"),
    }
    assert!(matches!(r.read_worktree_file("dangling"), Err(Error::Invalid(m)) if m == "symlink: nowhere-at-all"));
    // symlinked intermediate directory that leaves the repository
    let e = r.read_worktree_file("dir-link/passwd").unwrap_err();
    assert!(matches!(&e, Error::Invalid(m) if m.contains("outside the repository")), "{e:?}");
    // FIFO: must return immediately instead of blocking on open()
    let t0 = Instant::now();
    let e = r.read_worktree_file("pipe").unwrap_err();
    assert!(matches!(&e, Error::Invalid(m) if m.contains("not a regular file")), "{e:?}");
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert!(matches!(r.read_worktree_file("dir"), Err(Error::Invalid(m)) if m.contains("not a regular file")));
    // a symlinked directory that stays inside the repository is fine; so is a regular file
    assert_eq!(r.read_worktree_file("inner-link/q'uote.rs").unwrap(), b"fn main() { println!(\"$(id)\"); }\n");
    assert_eq!(r.read_worktree_file("a.txt").unwrap(), b"one\nTWO\nthree\nfour\n");
    // over the cap: refused before any content is transferred
    let small = r.clone().with_max_output(10);
    assert!(matches!(small.read_worktree_file("a.txt"), Err(Error::TooLarge { limit: 10, .. })));
    assert!(matches!(r.read_worktree_file("missing"), Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound));
}

fn cache_path(r: &Repo, root: &Path) -> PathBuf {
    r.fetch_records_cache_at(root).unwrap().unwrap()
}

/// Review #4/#5: the records cache never follows symlinks and validates freshness by content.
#[test]
fn records_cache_is_symlink_safe_and_content_validated() {
    let t = fixture();
    let sp = spec("repo-cache-sec");
    let r = Repo::open_remote(&sp, &path_str(&t)).unwrap();
    let logdir = t.path().join(".git/codetrail");
    std::fs::create_dir_all(&logdir).unwrap();
    let remote_log = logdir.join("log.ct");
    std::fs::write(&remote_log, b"frame-1").unwrap();
    let victim_dir = tempfile::tempdir().unwrap();
    let victim = victim_dir.path().join("precious.txt");
    std::fs::write(&victim, b"precious").unwrap();

    let root = tempfile::tempdir().unwrap();
    let log = cache_path(&r, root.path());
    let entry = log.parent().unwrap().to_path_buf();
    // root and entry are private
    assert_eq!(std::fs::metadata(&entry).unwrap().permissions().mode() & 0o777, 0o700);
    use std::os::unix::fs::{symlink, PermissionsExt};

    // symlinked meta → not followed (victim intact), replaced by a regular file
    std::fs::remove_file(entry.join("meta")).unwrap();
    symlink(&victim, entry.join("meta")).unwrap();
    std::fs::write(&remote_log, b"frame-1-and-2").unwrap();
    let log = cache_path(&r, root.path());
    assert_eq!(std::fs::read(&log).unwrap(), b"frame-1-and-2");
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    assert!(!std::fs::symlink_metadata(entry.join("meta")).unwrap().file_type().is_symlink());

    // symlinked log.ct whose target looks "fresh" → replaced, victim intact and never returned
    std::fs::remove_file(&log).unwrap();
    symlink(&victim, &log).unwrap();
    let log = cache_path(&r, root.path());
    assert_eq!(std::fs::read(&log).unwrap(), b"frame-1-and-2");
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    assert!(!std::fs::symlink_metadata(&log).unwrap().file_type().is_symlink());

    // the whole entry directory replaced by a symlink to someone else's directory
    std::fs::remove_dir_all(&entry).unwrap();
    symlink(victim_dir.path(), &entry).unwrap();
    let log = cache_path(&r, root.path());
    assert_eq!(std::fs::read(&log).unwrap(), b"frame-1-and-2");
    assert!(!std::fs::symlink_metadata(&entry).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_dir(victim_dir.path()).unwrap().count(), 1, "nothing was written into the symlink target");
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");

    // predictable temp names can no longer be pre-planted: no leftovers, no foreign writes
    assert!(std::fs::read_dir(&entry).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")));

    // a loose root is tightened
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    cache_path(&r, root.path());
    assert_eq!(std::fs::metadata(root.path()).unwrap().permissions().mode() & 0o777, 0o700);
    // a symlinked root is refused
    let linked_root = victim_dir.path().join("root-link");
    symlink(root.path(), &linked_root).unwrap();
    assert!(r.fetch_records_cache_at(&linked_root).is_err());

    // same size + same mtime but different bytes → still re-downloaded (digest differs)
    let mtime = std::fs::metadata(&remote_log).unwrap().modified().unwrap();
    std::fs::write(&remote_log, b"AAAAAAAA").unwrap();
    std::fs::File::options().write(true).open(&remote_log).unwrap().set_modified(mtime).unwrap();
    assert_eq!(std::fs::read(cache_path(&r, root.path())).unwrap(), b"AAAAAAAA");
    std::fs::write(&remote_log, b"BBBBBBBB").unwrap();
    std::fs::File::options().write(true).open(&remote_log).unwrap().set_modified(mtime).unwrap();
    assert_eq!(std::fs::read(cache_path(&r, root.path())).unwrap(), b"BBBBBBBB");
}

/// Review #5: identity file / transport / trust policy are part of the cache namespace.
#[test]
fn records_cache_key_covers_every_connection_field() {
    let t = fixture();
    let logdir = t.path().join(".git/codetrail");
    std::fs::create_dir_all(&logdir).unwrap();
    std::fs::write(logdir.join("log.ct"), b"same").unwrap();
    let root = tempfile::tempdir().unwrap();
    let base = spec("repo-cache-key");
    let mut seen = std::collections::HashSet::new();
    let variants: Vec<Box<dyn Fn(&mut RemoteSpec)>> = vec![
        Box::new(|_| {}),
        Box::new(|s| s.identity_file = Some("/k/a".into())),
        Box::new(|s| s.identity_file = Some("/k/b".into())),
        Box::new(|s| s.transport = Transport::TailscaleNc),
        Box::new(|s| s.trust_new_host = true),
        Box::new(|s| s.port = Some(2200)),
        Box::new(|s| s.user = Some("other".into())),
    ];
    for v in &variants {
        let mut s = base.clone();
        v(&mut s);
        let r = Repo::open_remote(&s, &path_str(&t)).unwrap();
        assert!(seen.insert(cache_path(&r, root.path())), "{s:?} reuses another endpoint's cache entry");
    }
}

/// Review #7: connection tests leave no master behind; real calls keep ControlPersist.
#[test]
fn test_connection_uses_an_ephemeral_master() {
    remote::test_connection(&spec("repo-eph"), Duration::from_secs(10)).unwrap();
    let t = fixture();
    Repo::open_remote(&spec("repo-eph2"), &path_str(&t)).unwrap();
    let eph = log_lines_for("repo-eph");
    assert!(eph[0].contains("ControlPersist=no") && !eph[0].contains("ControlPersist=120"), "{}", eph[0]);
    let norm = log_lines_for("repo-eph2");
    assert!(norm[0].contains("ControlPersist=120"), "{}", norm[0]);
    // both address the same socket when the policy is the same
    let cp = |l: &str| l.split(" -o ").find(|w| w.starts_with("ControlPath=")).map(|w| w.split('/').next_back().unwrap().to_string());
    assert!(cp(&eph[0]).is_some());
}
