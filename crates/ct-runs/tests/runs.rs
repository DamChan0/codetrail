//! ct-runs behaviour tests. Agents are FAKES (in-process threads / `sh` children behind a fake
//! `SessionFactory`); everything git-related runs against real temp repos through the git CLI.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ct_agentd::{AgentEvent, AgentSession, BackendKind, ModelSel, SessionOpts};
use ct_runs::{ApplyOutcome, RunInfo, RunManager, RunSpec, RunState, RunUpdate, SessionFactory, Timings};
use ct_store::{best_confidence, decode_reason, Confidence, Store};
use serde_json::{json, Value};

// ------------------------------------------------------------------ git helpers

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git");
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git").current_dir(dir).args(args).output().map(|o| o.status.success()).unwrap_or(false)
}

struct Env {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    data: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    let data = root.join("data");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "tester"]);
    git(&repo, &["config", "user.email", "t@t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    fs::write(repo.join("f.txt"), "a\nb\nc\n").unwrap();
    fs::write(repo.join("g.txt"), "g1\ng2\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    Env { _tmp: tmp, repo, data }
}

fn spec(e: &Env, prompt: &str, isolate: bool) -> RunSpec {
    RunSpec {
        repo: e.repo.clone(),
        prompt: prompt.into(),
        model: ModelSel { backend: BackendKind::Pi, provider: None, id: "fake".into(), thinking: None },
        base_ref: "HEAD".into(),
        isolate,
    }
}

// ------------------------------------------------------------------ fake agents

#[derive(Clone)]
enum Beh {
    /// Edits files, then settles. `gate`: wait until set (or the run is aborted).
    Edit { files: Vec<(String, String)>, intro: String, outro: String, ok: bool, gate: Option<Arc<AtomicBool>> },
    /// A real child process (own process group) that never produces events.
    Hang { ignore_abort: bool, block_abort: bool },
}

fn edit(files: &[(&str, &str)], intro: &str, outro: &str) -> Beh {
    Beh::Edit { files: files.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(), intro: intro.into(), outro: outro.into(), ok: true, gate: None }
}

type Make = Box<dyn Fn(&SessionOpts) -> Option<Beh> + Send + Sync>;

struct Fac {
    make: Make,
    cur: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl Fac {
    fn new(make: impl Fn(&SessionOpts) -> Option<Beh> + Send + Sync + 'static) -> Arc<Fac> {
        Arc::new(Fac { make: Box::new(make), cur: Arc::new(AtomicUsize::new(0)), peak: Arc::new(AtomicUsize::new(0)) })
    }
}

struct Fake {
    tx: Sender<AgentEvent>,
    rx: Receiver<AgentEvent>,
    beh: Beh,
    cwd: PathBuf,
    child: Option<Child>,
    stop: Arc<AtomicBool>,
    cur: Arc<AtomicUsize>,
}

impl SessionFactory for Fac {
    fn start(&self, _b: BackendKind, o: SessionOpts) -> ct_agentd::Result<Box<dyn AgentSession>> {
        let beh = (self.make)(&o).ok_or_else(|| ct_agentd::Error::Other("fake: refuse to start".into()))?;
        let now = self.cur.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        let child = match &beh {
            Beh::Hang { .. } => Some(
                Command::new("sh")
                    .args(["-c", "trap '' TERM; while :; do sleep 1; done"])
                    .process_group(0)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| ct_agentd::Error::Other(e.to_string()))?,
            ),
            _ => None,
        };
        let (tx, rx) = channel();
        Ok(Box::new(Fake { tx, rx, beh, cwd: o.cwd, child, stop: Arc::new(AtomicBool::new(false)), cur: self.cur.clone() }))
    }
}

impl AgentSession for Fake {
    fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }
    fn prompt(&mut self, _text: &str) -> ct_agentd::Result<()> {
        if let Beh::Edit { files, intro, outro, ok, gate } = self.beh.clone() {
            let (tx, cwd, stop) = (self.tx.clone(), self.cwd.clone(), self.stop.clone());
            std::thread::spawn(move || {
                let _ = tx.send(AgentEvent::Started);
                if let Some(g) = gate {
                    while !g.load(Ordering::SeqCst) {
                        if stop.load(Ordering::SeqCst) {
                            let _ = tx.send(AgentEvent::Exited(Some(137)));
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                let _ = tx.send(AgentEvent::TextDelta(intro));
                let _ = tx.send(AgentEvent::ToolStart { id: "t1".into(), name: "write".into(), summary: "write".into() });
                for (p, c) in &files {
                    let f = cwd.join(p);
                    fs::create_dir_all(f.parent().unwrap()).unwrap();
                    fs::write(&f, c).unwrap();
                }
                let path = files.first().map(|(p, _)| cwd.join(p).to_string_lossy().into_owned());
                let _ = tx.send(AgentEvent::ToolEnd { id: "t1".into(), name: "write".into(), ok: true, path });
                let _ = tx.send(AgentEvent::TextDelta(outro));
                let _ = tx.send(AgentEvent::Settled { ok, error: (!ok).then(|| "boom".to_string()) });
            });
        }
        Ok(())
    }
    fn steer(&mut self, _t: &str) -> ct_agentd::Result<()> {
        Ok(())
    }
    fn follow_up(&mut self, _t: &str) -> ct_agentd::Result<()> {
        Ok(())
    }
    fn abort(&mut self) -> ct_agentd::Result<()> {
        if let Beh::Hang { block_abort: true, .. } = self.beh {
            std::thread::sleep(Duration::from_secs(4));
        }
        match self.beh {
            Beh::Hang { ignore_abort: true, .. } => {}
            _ => self.stop.store(true, Ordering::SeqCst),
        }
        Ok(())
    }
    fn set_model(&mut self, _m: &ModelSel) -> ct_agentd::Result<()> {
        Ok(())
    }
    fn events(&self) -> &Receiver<AgentEvent> {
        &self.rx
    }
    fn close(mut self: Box<Self>) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.wait();
        }
        self.cur.fetch_sub(1, Ordering::SeqCst);
    }
}

// ------------------------------------------------------------------ run helpers

const FAST: Timings = Timings { abort_grace: Duration::from_millis(300), term_grace: Duration::from_millis(300), tick: Duration::from_millis(10), finalize_delay: Duration::ZERO };

fn mgr(e: &Env, max: usize, f: Arc<Fac>) -> RunManager {
    RunManager::open_with(&e.data, max, f, FAST).unwrap()
}

fn wait_for(m: &RunManager, id: &str, what: &str, pred: impl Fn(&RunInfo) -> bool) -> RunInfo {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(i) = m.get(id) {
            if pred(&i) {
                return i;
            }
        }
        assert!(Instant::now() < end, "timeout waiting for {what}; now {:?}", m.get(id).map(|i| i.state));
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn done(m: &RunManager, id: &str) -> RunInfo {
    wait_for(m, id, "run to finish", |i| !i.state.is_active())
}

fn pid_gone(pid: u32) -> bool {
    ct_runs::process_start_time(pid).is_none()
}

// ------------------------------------------------------------------ tests

#[test]
fn isolated_run_leaves_user_tree_untouched_and_commits_artifact() {
    let e = env();
    let head0 = git(&e.repo, &["rev-parse", "HEAD"]);
    let status0 = git(&e.repo, &["status", "--porcelain=v1", "-uall"]);
    let m = mgr(&e, 2, Fac::new(|_| Some(edit(&[("f.txt", "a\nB\nc\nd\n"), ("new/n.txt", "n\n")], "Changing b.", "All done."))));
    let id = m.submit(spec(&e, "make b uppercase and add n", true)).unwrap();
    let info = done(&m, &id);
    assert_eq!(info.state, RunState::Succeeded, "{info:?}");
    assert_eq!(info.files_changed, 2);
    assert_eq!(info.base_sha, head0);

    // user's checkout untouched
    assert_eq!(git(&e.repo, &["rev-parse", "HEAD"]), head0);
    assert_eq!(git(&e.repo, &["status", "--porcelain=v1", "-uall"]), status0);
    assert_eq!(fs::read_to_string(e.repo.join("f.txt")).unwrap(), "a\nb\nc\n");
    assert!(!e.repo.join("new").exists());

    // worktree + branch live under the data dir
    let wt = info.worktree.clone().unwrap();
    assert!(wt.starts_with(e.data.join("worktrees")), "{wt:?}");
    assert_eq!(fs::read_to_string(wt.join("f.txt")).unwrap(), "a\nB\nc\nd\n");
    let branch = info.branch.clone().unwrap();
    assert!(branch.starts_with("ct/"), "{branch}");
    let (base, head) = m.comparison(&id).unwrap();
    assert_eq!(base, head0);
    assert_eq!(git(&e.repo, &["rev-parse", &format!("refs/heads/{branch}")]), head);
    assert_eq!(git(&e.repo, &["log", "-1", "--format=%an|%ae|%s", &head]), format!("codetrail-run|{id}@codetrail.local|codetrail run {id}"));
    let names = git(&e.repo, &["diff", "--name-only", &base, &head]);
    assert_eq!(names.lines().collect::<Vec<_>>(), vec!["f.txt", "new/n.txt"]);
    assert_eq!(git(&e.repo, &["rev-list", "--count", &format!("{base}..{head}")]), "1");

    // persisted: a fresh manager sees the same terminal record + event log exists
    drop(m);
    let m2 = mgr(&e, 2, Fac::new(|_| None));
    let again = m2.get(&id).unwrap();
    assert_eq!(again.state, RunState::Succeeded);
    assert_eq!(again.files_changed, 2);
    let log = fs::read_to_string(m2.events_log(&id)).unwrap();
    assert!(log.contains("\"ev\":\"settled\"") && log.contains("All done."), "{log}");
}

#[test]
fn run_base_ref_is_resolved_to_a_sha_at_submit() {
    let e = env();
    git(&e.repo, &["branch", "side"]);
    let side0 = git(&e.repo, &["rev-parse", "side"]);
    let gate = Arc::new(AtomicBool::new(false));
    let g2 = gate.clone();
    let m = mgr(&e, 1, Fac::new(move |_| Some(Beh::Edit { files: vec![("x".into(), "x\n".into())], intro: "i".into(), outro: "o".into(), ok: true, gate: Some(g2.clone()) })));
    let mut s = spec(&e, "p", true);
    s.base_ref = "side".into();
    let id = m.submit(s).unwrap();
    wait_for(&m, &id, "worktree created", |i| i.worktree.as_ref().is_some_and(|w| w.exists()));
    // the branch moves while the run is in flight
    git(&e.repo, &["checkout", "-q", "side"]);
    fs::write(e.repo.join("later.txt"), "l\n").unwrap();
    git(&e.repo, &["add", "-A"]);
    git(&e.repo, &["commit", "-q", "-m", "later"]);
    gate.store(true, Ordering::SeqCst);
    let info = done(&m, &id);
    assert_eq!(info.base_sha, side0);
    let (base, head) = m.comparison(&id).unwrap();
    assert_eq!(base, side0);
    assert_eq!(git(&e.repo, &["diff", "--name-only", &base, &head]), "x");
    assert!(m.submit(RunSpec { base_ref: "no-such-ref".into(), ..spec(&e, "p", true) }).is_err());
    assert!(m.submit(spec(&e, "   ", true)).is_err());
}

#[test]
fn queue_respects_max_concurrent_and_resumes() {
    let e = env();
    let gate = Arc::new(AtomicBool::new(false));
    let g2 = gate.clone();
    let fac = Fac::new(move |o| {
        let n = o.cwd.file_name().unwrap().to_string_lossy().into_owned();
        Some(Beh::Edit { files: vec![(format!("{n}.txt"), "x\n".into())], intro: "i".into(), outro: "o".into(), ok: true, gate: Some(g2.clone()) })
    });
    let (peak, cur) = (fac.peak.clone(), fac.cur.clone());
    let m = mgr(&e, 2, fac);
    let ids: Vec<String> = (0..5).map(|i| m.submit(spec(&e, &format!("task {i}"), true)).unwrap()).collect();
    wait_for(&m, &ids[1], "second run running", |i| i.state == RunState::Running);
    std::thread::sleep(Duration::from_millis(150));
    let states: Vec<RunState> = ids.iter().map(|i| m.get(i).unwrap().state).collect();
    assert_eq!(states.iter().filter(|s| **s == RunState::Running).count(), 2, "{states:?}");
    assert_eq!(states.iter().filter(|s| **s == RunState::Queued).count(), 3, "{states:?}");
    assert_eq!(cur.load(Ordering::SeqCst), 2);

    // abort of a queued run never starts it
    m.abort(&ids[4]).unwrap();
    assert_eq!(m.get(&ids[4]).unwrap().state, RunState::Aborted);

    gate.store(true, Ordering::SeqCst);
    for id in &ids[..4] {
        assert_eq!(done(&m, id).state, RunState::Succeeded);
    }
    assert_eq!(m.get(&ids[4]).unwrap().state, RunState::Aborted);
    assert!(m.get(&ids[4]).unwrap().worktree.is_none());
    assert!(peak.load(Ordering::SeqCst) <= 2, "peak {}", peak.load(Ordering::SeqCst));
}

#[test]
fn subscribers_receive_state_and_events() {
    let e = env();
    let m = mgr(&e, 1, Fac::new(|_| Some(edit(&[("a.txt", "a\n")], "intro", "outro"))));
    let rx = m.subscribe();
    let id = m.submit(spec(&e, "p", true)).unwrap();
    done(&m, &id);
    let mut states = vec![];
    let mut texts = vec![];
    while let Ok(u) = rx.recv_timeout(Duration::from_millis(300)) {
        match u {
            RunUpdate::State(i) => states.push(i.state),
            RunUpdate::Event(rid, AgentEvent::TextDelta(t)) => {
                assert_eq!(rid, id);
                texts.push(t);
            }
            RunUpdate::Event(..) => {}
        }
    }
    assert_eq!(states.first(), Some(&RunState::Queued));
    assert!(states.contains(&RunState::Running));
    assert_eq!(states.last(), Some(&RunState::Succeeded));
    assert_eq!(texts, vec!["intro", "outro"]);
}

#[test]
fn abort_is_staged_and_kills_a_child_that_ignores_abort() {
    let e = env();
    let m = mgr(&e, 1, Fac::new(|_| Some(Beh::Hang { ignore_abort: true, block_abort: false })));
    let id = m.submit(spec(&e, "hang", true)).unwrap();
    let running = wait_for(&m, &id, "pid recorded", |i| i.state == RunState::Running && i.worktree.is_some());
    // pid is persisted (and visible in run.json)
    let rec: Value = serde_json::from_slice(&fs::read(e.data.join("runs").join(&id).join("run.json")).unwrap()).unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    let pid = loop {
        let rec2: Value = serde_json::from_slice(&fs::read(e.data.join("runs").join(&id).join("run.json")).unwrap()).unwrap();
        if let Some(p) = rec2["pid"].as_u64() {
            break p as u32;
        }
        assert!(Instant::now() < end, "pid never recorded: {rec}");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(!pid_gone(pid));
    assert!(rec["pid_start"].is_null() || rec["pid_start"].is_u64());
    drop(running);

    let t = Instant::now();
    m.abort(&id).unwrap();
    let info = done(&m, &id);
    let took = t.elapsed();
    assert_eq!(info.state, RunState::Aborted);
    // ignored `abort` → waited abort_grace, then SIGTERM (trapped) → waited term_grace, then SIGKILL
    assert!(took >= Duration::from_millis(550), "killed too early: {took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert!(pid_gone(pid), "agent process survived abort");
    // process group is empty too (the `sleep` grandchild)
    assert!(unsafe { libc_kill_group_gone(pid) });
    // the run is reviewable and discardable afterwards
    m.discard(&id).unwrap();
    assert!(m.get(&id).is_none());
}

/// `kill(-pgid, 0)` fails with ESRCH when the group is empty.
unsafe fn libc_kill_group_gone(pgid: u32) -> bool {
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        let alive = Command::new("sh").args(["-c", &format!("kill -0 -- -{pgid} 2>/dev/null")]).status().map(|s| s.success()).unwrap_or(false);
        if !alive {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn abort_with_cooperative_agent_is_quick_and_keeps_partial_work_discardable() {
    let e = env();
    let gate = Arc::new(AtomicBool::new(false));
    let g2 = gate.clone();
    let m = mgr(&e, 1, Fac::new(move |_| Some(Beh::Edit { files: vec![("x".into(), "x\n".into())], intro: "i".into(), outro: "o".into(), ok: true, gate: Some(g2.clone()) })));
    let id = m.submit(spec(&e, "p", true)).unwrap();
    wait_for(&m, &id, "running", |i| i.state == RunState::Running);
    m.abort(&id).unwrap();
    let info = done(&m, &id);
    assert_eq!(info.state, RunState::Aborted);
    m.discard(&id).unwrap();
    assert!(!git_ok(&e.repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", info.branch.unwrap())]));
}

#[test]
fn failures_leave_no_half_state() {
    let e = env();
    // agent cannot start
    let m = mgr(&e, 1, Fac::new(|_| None));
    let id = m.submit(spec(&e, "p", true)).unwrap();
    let i = done(&m, &id);
    assert!(matches!(&i.state, RunState::Failed(s) if s.contains("cannot start agent")), "{:?}", i.state);
    assert!(i.worktree.is_none() && i.branch.is_none());
    assert_eq!(git(&e.repo, &["branch", "--list", "ct/*"]), "");
    assert_eq!(git(&e.repo, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
    let leftovers: Vec<_> = walk(&e.data.join("worktrees")).into_iter().filter(|p| !p.is_dir() || p.join(".git").exists()).collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // worktree creation fails (<data>/worktrees is a file)
    let e2 = env();
    fs::create_dir_all(&e2.data).unwrap();
    fs::write(e2.data.join("worktrees"), "not a dir").unwrap();
    let m2 = mgr(&e2, 1, Fac::new(|_| Some(edit(&[("a", "a\n")], "i", "o"))));
    let id2 = m2.submit(spec(&e2, "p", true)).unwrap();
    let i2 = done(&m2, &id2);
    assert!(matches!(&i2.state, RunState::Failed(s) if s.contains("worktree")), "{:?}", i2.state);
    assert!(i2.worktree.is_none() && i2.branch.is_none());
    assert_eq!(git(&e2.repo, &["branch", "--list", "ct/*"]), "");

    // agent reports failure: state Failed, but its work is still kept as an artifact
    let e3 = env();
    let m3 = mgr(&e3, 1, Fac::new(|_| Some(Beh::Edit { files: vec![("a".into(), "a\n".into())], intro: "i".into(), outro: "o".into(), ok: false, gate: None })));
    let id3 = m3.submit(spec(&e3, "p", true)).unwrap();
    let i3 = done(&m3, &id3);
    assert_eq!(i3.state, RunState::Failed("boom".into()));
    assert_eq!(i3.files_changed, 1);
}

fn walk(p: &Path) -> Vec<PathBuf> {
    let mut v = vec![];
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            v.push(e.path());
            if e.path().is_dir() {
                v.extend(walk(&e.path()));
            }
        }
    }
    v
}

#[test]
fn restart_reconcile_kills_orphans_and_resumes_queue() {
    let e = env();
    let m0 = mgr(&e, 1, Fac::new(|_| None));
    drop(m0); // creates data/runs
    // an orphaned "running" agent from a previous app instance
    let mut orphan = Command::new("sh")
        .args(["-c", "trap '' TERM; while :; do sleep 1; done"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let opid = orphan.id();
    let ostart = ct_runs::process_start_time(opid).unwrap();
    // a pid whose start time does NOT match (pid reuse) must not be killed
    let mut innocent = Command::new("sleep").arg("600").process_group(0).stdout(Stdio::null()).spawn().unwrap();
    let ipid = innocent.id();
    let base = git(&e.repo, &["rev-parse", "HEAD"]);
    let rec = |id: &str, state: &str, pid: Option<u32>, start: Option<u64>, ts: i64| {
        json!({"id": id, "repo": e.repo, "prompt": "p", "backend": "pi", "provider": null, "model_id": "fake", "thinking": null,
               "base_ref": "HEAD", "isolate": false, "base_sha": base, "branch": null, "worktree": null, "state": state,
               "state_msg": null, "submitted_ms": ts, "started_ms": ts, "ended_ms": null, "files_changed": 0,
               "pid": pid, "pid_start": start, "head_sha": null})
    };
    let write = |id: &str, v: Value| {
        let d = e.data.join("runs").join(id);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("run.json"), serde_json::to_vec(&v).unwrap()).unwrap();
    };
    write("18c00000-0000000a", rec("18c00000-0000000a", "running", Some(opid), Some(ostart), 1000));
    write("18c00000-0000000b", rec("18c00000-0000000b", "running", Some(ipid), Some(ct_runs::process_start_time(ipid).unwrap() + 1), 2000));
    write("18c00000-0000000c", rec("18c00000-0000000c", "queued", None, None, 3000));
    fs::create_dir_all(e.data.join("runs/garbage")).unwrap(); // ignored

    let m = mgr(&e, 1, Fac::new(|_| Some(edit(&[("resumed.txt", "r\n")], "i", "o"))));
    let a = m.get("18c00000-0000000a").unwrap();
    assert_eq!(a.state, RunState::Interrupted);
    assert!(a.ended_ms.is_some());
    assert!(pid_gone(opid), "orphaned agent still alive");
    orphan.wait().unwrap();
    let b = m.get("18c00000-0000000b").unwrap();
    assert_eq!(b.state, RunState::Interrupted);
    assert!(!pid_gone(ipid), "a different process (pid reuse) was killed");
    let _ = innocent.kill();
    let _ = innocent.wait();

    // queued run resumed and executed (in place → creates the file in the repo)
    let c = done(&m, "18c00000-0000000c");
    assert_eq!(c.state, RunState::Succeeded, "{c:?}");
    assert_eq!(fs::read_to_string(e.repo.join("resumed.txt")).unwrap(), "r\n");
    let order: Vec<String> = m.list().into_iter().map(|i| i.id).collect();
    assert_eq!(order, vec!["18c00000-0000000a", "18c00000-0000000b", "18c00000-0000000c"]);
}

#[test]
fn restart_reconcile_keeps_uncommitted_worktree_changes_of_an_interrupted_isolated_run() {
    let e = env();
    let gate = Arc::new(AtomicBool::new(false));
    let g2 = gate.clone();
    // run 1 creates its worktree, then we "crash" by rewriting the record as if the process died
    let m = mgr(&e, 1, Fac::new(move |_| Some(Beh::Edit { files: vec![("w.txt".into(), "w\n".into())], intro: "i".into(), outro: "o".into(), ok: true, gate: Some(g2.clone()) })));
    let id = m.submit(spec(&e, "p", true)).unwrap();
    let info = wait_for(&m, &id, "worktree", |i| i.worktree.as_ref().is_some_and(|w| w.exists()));
    let wt = info.worktree.unwrap();
    fs::write(wt.join("leftover.txt"), "uncommitted\n").unwrap();
    m.abort(&id).unwrap(); // let the first manager finish cleanly, then fake a crash state
    done(&m, &id);
    drop(m);
    let p = e.data.join("runs").join(&id).join("run.json");
    let mut v: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    v["state"] = json!("running");
    v["pid"] = json!(null);
    v["head_sha"] = json!(null);
    fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
    fs::write(wt.join("more.txt"), "more\n").unwrap();

    let m2 = mgr(&e, 1, Fac::new(|_| None));
    let i = m2.get(&id).unwrap();
    assert_eq!(i.state, RunState::Interrupted);
    let (base, head) = m2.comparison(&id).unwrap();
    let names = git(&e.repo, &["diff", "--name-only", &base, &head]);
    assert!(names.contains("more.txt") && names.contains("leftover.txt"), "{names}");
}

fn finished_edit_run(e: &Env, m: &RunManager, files: &[(&str, &str)]) -> String {
    let _ = e;
    let id = m.submit(spec(e, "do it", true)).unwrap();
    let i = done(m, &id);
    assert_eq!(i.state, RunState::Succeeded, "{i:?}");
    let _ = files;
    id
}

#[test]
fn apply_merges_with_no_ff_when_clean() {
    let e = env();
    let m = mgr(&e, 1, Fac::new(|_| Some(edit(&[("f.txt", "a\nb\nc\nnew\n")], "i", "o"))));
    let id = finished_edit_run(&e, &m, &[]);
    // user moved on (HEAD descends from base) without touching f.txt
    fs::write(e.repo.join("g.txt"), "g1\ng2\ng3\n").unwrap();
    git(&e.repo, &["commit", "-q", "-am", "user work"]);
    let out = m.apply(&id).unwrap();
    let ApplyOutcome::Merged(sha) = out else { panic!("{out:?}") };
    assert_eq!(git(&e.repo, &["rev-parse", "HEAD"]), sha);
    assert_eq!(git(&e.repo, &["rev-list", "--parents", "-n1", "HEAD"]).split(' ').count(), 3, "expected a 2-parent merge commit");
    assert_eq!(fs::read_to_string(e.repo.join("f.txt")).unwrap(), "a\nb\nc\nnew\n");
    assert_eq!(fs::read_to_string(e.repo.join("g.txt")).unwrap(), "g1\ng2\ng3\n");
    assert_eq!(git(&e.repo, &["status", "--porcelain=v1"]), "");
    // applying again is a no-op
    assert_eq!(m.apply(&id).unwrap(), ApplyOutcome::NothingToApply);
}

#[test]
fn apply_conflict_is_aborted_and_reported() {
    let e = env();
    let m = mgr(&e, 1, Fac::new(|_| Some(edit(&[("f.txt", "a\nRUN\nc\n")], "i", "o"))));
    let id = finished_edit_run(&e, &m, &[]);
    fs::write(e.repo.join("f.txt"), "a\nUSER\nc\n").unwrap();
    git(&e.repo, &["commit", "-q", "-am", "user edit"]);
    let head = git(&e.repo, &["rev-parse", "HEAD"]);
    let out = m.apply(&id).unwrap();
    assert_eq!(out, ApplyOutcome::Conflict(vec!["f.txt".into()]));
    assert_eq!(git(&e.repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&e.repo, &["status", "--porcelain=v1", "-uall"]), "");
    assert!(!git_ok(&e.repo, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]));
    assert_eq!(fs::read_to_string(e.repo.join("f.txt")).unwrap(), "a\nUSER\nc\n");
}

#[test]
fn apply_refuses_dirty_tree_and_unrelated_head() {
    let e = env();
    let m = mgr(&e, 1, Fac::new(|_| Some(edit(&[("new.txt", "n\n")], "i", "o"))));
    let id = finished_edit_run(&e, &m, &[]);
    let head = git(&e.repo, &["rev-parse", "HEAD"]);

    fs::write(e.repo.join("g.txt"), "dirty\n").unwrap(); // tracked + modified
    let err = m.apply(&id).unwrap_err().to_string();
    assert!(err.contains("uncommitted"), "{err}");
    assert_eq!(git(&e.repo, &["rev-parse", "HEAD"]), head);
    assert!(!e.repo.join("new.txt").exists());
    git(&e.repo, &["checkout", "-q", "--", "g.txt"]);

    // HEAD that does not descend from the run's base
    git(&e.repo, &["checkout", "-q", "--orphan", "other"]);
    git(&e.repo, &["rm", "-rfq", "."]);
    fs::write(e.repo.join("o.txt"), "o\n").unwrap();
    git(&e.repo, &["add", "-A"]);
    git(&e.repo, &["commit", "-q", "-m", "unrelated root"]);
    let other = git(&e.repo, &["rev-parse", "HEAD"]);
    let err = m.apply(&id).unwrap_err().to_string();
    assert!(err.contains("does not descend"), "{err}");
    assert_eq!(git(&e.repo, &["rev-parse", "HEAD"]), other);

    // active runs cannot be applied or discarded
    let gate = Arc::new(AtomicBool::new(false));
    let g2 = gate.clone();
    let m2 = mgr(&e, 1, Fac::new(move |_| Some(Beh::Edit { files: vec![("z".into(), "z\n".into())], intro: "i".into(), outro: "o".into(), ok: true, gate: Some(g2.clone()) })));
    let id2 = m2.submit(spec(&e, "p", true)).unwrap();
    wait_for(&m2, &id2, "running", |i| i.state == RunState::Running);
    assert!(m2.apply(&id2).is_err());
    assert!(m2.discard(&id2).is_err());
    gate.store(true, Ordering::SeqCst);
    done(&m2, &id2);
}

#[test]
fn discard_removes_worktree_branch_and_record() {
    let e = env();
    let m = mgr(&e, 1, Fac::new(|_| Some(edit(&[("a.txt", "a\n")], "i", "o"))));
    let id = finished_edit_run(&e, &m, &[]);
    let info = m.get(&id).unwrap();
    let (wt, branch) = (info.worktree.unwrap(), info.branch.unwrap());
    assert!(wt.exists());
    m.discard(&id).unwrap();
    assert!(!wt.exists());
    assert!(!git_ok(&e.repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]));
    assert_eq!(git(&e.repo, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
    assert!(m.get(&id).is_none());
    assert!(!e.data.join("runs").join(&id).exists());
    assert!(m.discard(&id).is_err());
}

#[test]
fn discard_refuses_paths_outside_the_worktrees_dir() {
    let e = env();
    let victim = e.data.parent().unwrap().join("victim");
    fs::create_dir_all(&victim).unwrap();
    fs::write(victim.join("precious.txt"), "keep\n").unwrap();
    let m = mgr(&e, 1, Fac::new(|_| Some(edit(&[("a.txt", "a\n")], "i", "o"))));
    let id = finished_edit_run(&e, &m, &[]);
    let branch = m.get(&id).unwrap().branch.unwrap();
    drop(m);

    let p = e.data.join("runs").join(&id).join("run.json");
    let orig: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    let try_with = |wt: &Path| -> String {
        let mut v = orig.clone();
        v["worktree"] = json!(wt);
        fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
        let m = mgr(&e, 1, Fac::new(|_| None));
        m.discard(&id).unwrap_err().to_string()
    };
    // absolute path elsewhere
    assert!(try_with(&victim).contains("refusing"), "outside");
    // `..` escape that lexically starts with the worktrees dir
    let sneaky = e.data.join("worktrees").join("..").join("..").join("victim");
    assert!(try_with(&sneaky).contains("refusing"), "dotdot");
    // the worktrees dir itself
    assert!(try_with(&e.data.join("worktrees")).contains("refusing"), "root");
    // symlink inside worktrees pointing outside
    let link = e.data.join("worktrees").join("evil-link");
    std::os::unix::fs::symlink(&victim, &link).unwrap();
    assert!(try_with(&link).contains("refusing"), "symlink");
    assert_eq!(fs::read_to_string(victim.join("precious.txt")).unwrap(), "keep\n");
    // nothing was deleted: branch + record intact
    assert!(git_ok(&e.repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]));
    assert!(p.exists());
    // branch names outside ct/ are refused as well
    let mut v = orig.clone();
    v["branch"] = json!("main");
    fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
    let m = mgr(&e, 1, Fac::new(|_| None));
    assert!(m.discard(&id).unwrap_err().to_string().contains("refusing"));
    assert!(git_ok(&e.repo, &["rev-parse", "--verify", "--quiet", "refs/heads/main"]));
}

#[test]
fn auto_reasons_are_recorded_and_link_high() {
    let e = env();
    let m = mgr(&e, 1, Fac::new(|_| {
        Some(edit(&[("f.txt", "a\nb\nc\nadded one\nadded two\n"), ("sub/new.txt", "fresh\n")], "Appending two lines to f.", "Final summary: appended lines and a new file."))
    }));
    let id = m.submit(spec(&e, "please extend f.txt", true)).unwrap();
    let info = done(&m, &id);
    assert_eq!(info.state, RunState::Succeeded);
    let (base, head) = m.comparison(&id).unwrap();

    let store = Store::open(&e.repo.join(".git")).unwrap();
    let recs = store.for_path("f.txt");
    assert_eq!(recs.len(), 1);
    let r = &recs[0];
    assert_eq!(r.session, id);
    assert_eq!(r.head_at_edit, base);
    assert_eq!(r.agent, ct_store::Agent::parse("pi"));
    assert_eq!(r.spans.len(), 1);
    assert_eq!((r.spans[0].new_start, r.spans[0].new_len), (4, 2));
    let reason = decode_reason(r);
    assert!(reason.contains("please extend f.txt"), "{reason}");
    assert!(reason.contains("Final summary: appended lines and a new file."), "{reason}");
    assert!(reason.contains("Appending two lines to f."), "intent missing: {reason}");
    assert!(r.transcript.as_ref().is_some_and(|t| t.path.ends_with("events.jsonl")));

    // one record per changed file, linked High by blob to the run commit
    let new = store.for_path("sub/new.txt");
    assert_eq!(new.len(), 1);
    let blob = git(&e.repo, &["rev-parse", &format!("{head}:f.txt")]);
    let m1 = store.match_hunk("f.txt", &["added one".into(), "added two".into()], &[blob.clone()]);
    assert_eq!(best_confidence(&m1), Confidence::High);
    assert_eq!(m1[0].0.id, r.id);
    // and by fingerprint when the blob differs (e.g. after a rebase)
    let m2 = store.match_hunk("f.txt", &["added one".into(), "added two".into()], &["0".repeat(40)]);
    assert_eq!(best_confidence(&m2), Confidence::Medium);
}

#[test]
fn reasons_use_the_text_after_the_last_tool_as_summary_and_cap_lengths() {
    let e = env();
    let long_prompt = format!("{}{}", "P".repeat(1000), "TAILPROMPT");
    let long_sum = format!("{}{}", "S".repeat(500), "TAILSUMMARY");
    let m = mgr(&e, 1, Fac::new({
        let long_sum = long_sum.clone();
        move |_| Some(edit(&[("f.txt", "a\nb\nc\nd\n")], "before tool", &long_sum))
    }));
    let id = m.submit(spec(&e, &long_prompt, true)).unwrap();
    assert_eq!(done(&m, &id).state, RunState::Succeeded);
    let store = Store::open(&e.repo.join(".git")).unwrap();
    let reason = decode_reason(&store.for_path("f.txt")[0]);
    assert!(reason.contains(&"P".repeat(1000)) && !reason.contains("TAILPROMPT"), "prompt cap");
    assert!(reason.contains(&"S".repeat(500)) && !reason.contains("TAILSUMMARY"), "summary cap");
}

#[test]
fn secrets_are_masked_in_reasons_and_event_log() {
    let e = env();
    let m = mgr(&e, 1, Fac::new(|_| {
        Some(edit(&[("f.txt", "a\nb\nc\nd\n")], "using key sk-abcdefghijklmnopqrstuvwxyz123456 now", "Done. password = \"hunter2hunter2hunter2\" was used."))
    }));
    let id = m.submit(spec(&e, "deploy with API_KEY = \"abcdef1234567890xyzXYZ\" and token ghp_abcdefghijklmnopqrstuvwxyz0123456789", true)).unwrap();
    assert_eq!(done(&m, &id).state, RunState::Succeeded);
    let store = Store::open(&e.repo.join(".git")).unwrap();
    let reason = decode_reason(&store.for_path("f.txt")[0]);
    for s in ["abcdef1234567890xyzXYZ", "ghp_abcdefghijklmnopqrstuvwxyz0123456789", "hunter2hunter2hunter2", "sk-abcdefghijklmnopqrstuvwxyz123456"] {
        assert!(!reason.contains(s), "secret {s} leaked into reason: {reason}");
    }
    assert!(reason.contains("[REDACTED]"), "{reason}");
    assert!(reason.contains("deploy with"), "non-secret text must survive: {reason}");
    let log = fs::read_to_string(m.events_log(&id)).unwrap();
    assert!(!log.contains("hunter2hunter2hunter2") && !log.contains("sk-abcdefghijklmnopqrstuvwxyz123456"), "{log}");
}

#[test]
fn mask_secrets_unit() {
    for (input, secret) in [
        ("AKIAABCDEFGHIJKLMNOP", "AKIAABCDEFGHIJKLMNOP"),
        ("-----BEGIN RSA PRIVATE KEY-----\nMIIEabc\n-----END RSA PRIVATE KEY-----", "MIIEabc"),
        ("Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123", "abcdefghijklmnopqrstuvwxyz0123"),
        ("xoxb-1234567890-abcdef", "xoxb-1234567890-abcdef"),
    ] {
        let out = ct_runs::mask_secrets(input);
        assert!(!out.contains(secret), "{input} -> {out}");
    }
    assert_eq!(ct_runs::mask_secrets("nothing secret here"), "nothing secret here");
}

#[test]
fn non_isolated_run_never_touches_users_index_head_or_branches() {
    let e = env();
    // user has staged + unstaged + untracked work that must survive untouched
    fs::write(e.repo.join("g.txt"), "g1\ng2\nSTAGED\n").unwrap();
    git(&e.repo, &["add", "g.txt"]);
    fs::write(e.repo.join("scratch.txt"), "user scratch\n").unwrap();
    let index_before = fs::read(e.repo.join(".git/index")).unwrap();
    let head_before = git(&e.repo, &["rev-parse", "HEAD"]);
    let branches_before = git(&e.repo, &["for-each-ref", "--format=%(refname) %(objectname)"]);
    let cached_before = git(&e.repo, &["diff", "--cached"]);

    let m = mgr(&e, 1, Fac::new(|_| Some(edit(&[("f.txt", "a\nb\nc\nin place\n")], "i", "o"))));
    let id = m.submit(spec(&e, "edit in place", false)).unwrap();
    let info = done(&m, &id);
    assert_eq!(info.state, RunState::Succeeded, "{info:?}");
    assert!(info.branch.is_none() && info.worktree.is_none());

    assert_eq!(fs::read(e.repo.join(".git/index")).unwrap(), index_before, "user's index file changed");
    assert_eq!(git(&e.repo, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git(&e.repo, &["diff", "--cached"]), cached_before);
    assert_eq!(fs::read_to_string(e.repo.join("scratch.txt")).unwrap(), "user scratch\n");
    // the agent's edit is in the user's tree (that is what "in place" means)
    assert_eq!(fs::read_to_string(e.repo.join("f.txt")).unwrap(), "a\nb\nc\nin place\n");
    // only addition: the detached run ref
    let after = git(&e.repo, &["for-each-ref", "--format=%(refname) %(objectname)"]);
    let added: Vec<&str> = after.lines().filter(|l| !branches_before.contains(l)).collect();
    assert_eq!(added.len(), 1, "{added:?}");
    assert!(added[0].starts_with(&format!("refs/codetrail/runs/{id} ")), "{added:?}");
    assert_eq!(git(&e.repo, &["branch", "--list", "ct/*"]), "");

    let (base, head) = m.comparison(&id).unwrap();
    assert_eq!(base, head_before);
    assert_ne!(head, base);
    assert_eq!(git(&e.repo, &["rev-parse", &format!("refs/codetrail/runs/{id}")]), head);
    // snapshot = base..work tree state: includes the agent's edit
    assert!(git(&e.repo, &["diff", "--name-only", &base, &head]).contains("f.txt"));
    // records exist for the changed files, linked to the snapshot blob
    let store = Store::open(&e.repo.join(".git")).unwrap();
    let r = &store.for_path("f.txt")[0];
    assert_eq!(r.session, id);
    assert_eq!(r.post_blob.as_deref(), Some(git(&e.repo, &["rev-parse", &format!("{head}:f.txt")]).as_str()));
    // apply is meaningless for in-place runs; discard just forgets the run + ref
    assert!(m.apply(&id).is_err());
    m.discard(&id).unwrap();
    assert!(!git_ok(&e.repo, &["rev-parse", "--verify", "--quiet", &format!("refs/codetrail/runs/{id}")]));
    assert_eq!(fs::read_to_string(e.repo.join("f.txt")).unwrap(), "a\nb\nc\nin place\n", "discard must not revert the user's tree");
}

#[test]
fn non_isolated_run_without_changes_creates_no_ref() {
    let e = env();
    let m = mgr(&e, 1, Fac::new(|_| Some(edit(&[("f.txt", "a\nb\nc\n")], "i", "o")))); // rewrites identical content
    let id = m.submit(spec(&e, "noop", false)).unwrap();
    let info = done(&m, &id);
    assert_eq!(info.state, RunState::Succeeded);
    assert_eq!(info.files_changed, 0);
    assert_eq!(git(&e.repo, &["for-each-ref", "refs/codetrail"]), "");
    let (b, h) = m.comparison(&id).unwrap();
    assert_eq!(b, h);
}

#[test]
fn shutdown_all_aborts_running_runs_and_keeps_queue() {
    let e = env();
    let m = mgr(&e, 1, Fac::new(|_| Some(Beh::Hang { ignore_abort: false, block_abort: false })));
    let a = m.submit(spec(&e, "one", true)).unwrap();
    let b = m.submit(spec(&e, "two", true)).unwrap();
    wait_for(&m, &a, "running", |i| i.state == RunState::Running);
    m.shutdown_all();
    let ia = m.get(&a).unwrap();
    assert!(matches!(ia.state, RunState::Aborted), "{:?}", ia.state);
    // the queued run was NOT started by the freed slot and stays queued for the next start
    assert_eq!(m.get(&b).unwrap().state, RunState::Queued);
    assert!(m.submit(spec(&e, "late", true)).is_err());
    drop(m);
    let m2 = mgr(&e, 1, Fac::new(|_| Some(edit(&[("q.txt", "q\n")], "i", "o"))));
    assert_eq!(done(&m2, &b).state, RunState::Succeeded);
}

#[test]
fn steer_reaches_the_running_session() {
    let e = env();
    let gate = Arc::new(AtomicBool::new(false));
    let g2 = gate.clone();
    let m = mgr(&e, 1, Fac::new(move |_| Some(Beh::Edit { files: vec![("x".into(), "x\n".into())], intro: "i".into(), outro: "o".into(), ok: true, gate: Some(g2.clone()) })));
    let id = m.submit(spec(&e, "p", true)).unwrap();
    wait_for(&m, &id, "running", |i| i.state == RunState::Running);
    std::thread::sleep(Duration::from_millis(100));
    m.steer(&id, "use tabs").unwrap();
    gate.store(true, Ordering::SeqCst);
    done(&m, &id);
    assert!(m.steer(&id, "late").is_err());
    assert!(m.steer("nope", "x").is_err());
}

// ------------------------------------------------------------------ LIVE (real agent backends)

fn live(backend: BackendKind) {
    if std::env::var("CT_LIVE").as_deref() != Ok("1") {
        eprintln!("CT_LIVE!=1; skipped");
        return;
    }
    let e = env();
    let models = ct_agentd::list_models(backend).expect("list_models");
    let pick = match backend {
        BackendKind::Claude => models.iter().find(|m| m.id == "haiku"),
        _ => models.first(),
    }
    .expect("no model");
    let sel = ModelSel { backend, provider: Some(pick.provider.clone()), id: pick.id.clone(), thinking: None };
    eprintln!("live model: {sel:?}");
    let head0 = git(&e.repo, &["rev-parse", "HEAD"]);
    let m = RunManager::open(&e.data, 2).unwrap(); // REAL default SessionFactory
    let prompt = "Create a file hello.txt containing exactly: hi. Do nothing else.";
    let id = m.submit(RunSpec { model: sel.clone(), ..spec(&e, prompt, true) }).unwrap();
    let end = Instant::now() + Duration::from_secs(std::env::var("CT_LIVE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(170));
    let info = loop {
        let i = m.get(&id).unwrap();
        if !i.state.is_active() {
            break i;
        }
        assert!(Instant::now() < end, "live run timed out: {:?}\nlog:\n{}", i.state, fs::read_to_string(m.events_log(&id)).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(200));
    };
    let log = fs::read_to_string(m.events_log(&id)).unwrap_or_default();
    assert_eq!(info.state, RunState::Succeeded, "{info:?}\n{log}");
    let (wt, branch) = (info.worktree.clone().unwrap(), info.branch.clone().unwrap());
    assert!(wt.exists() && git_ok(&e.repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]));
    assert_eq!(git(&e.repo, &["rev-parse", "HEAD"]), head0);
    assert_eq!(git(&e.repo, &["status", "--porcelain=v1", "-uall"]), "");
    assert_eq!(info.files_changed, 1, "{log}");
    let (base, head) = m.comparison(&id).unwrap();
    assert_eq!(git(&e.repo, &["diff", "--name-only", &base, &head]), "hello.txt");
    let store = Store::open(&e.repo.join(".git")).unwrap();
    let recs = store.for_path("hello.txt");
    assert_eq!(recs.len(), 1, "{log}");
    let blob = git(&e.repo, &["rev-parse", &format!("{head}:hello.txt")]);
    let hit = store.match_hunk("hello.txt", &["hi".into()], &[blob]);
    assert_eq!(best_confidence(&hit), Confidence::High);
    eprintln!("reason: {}\nlog:\n{log}", decode_reason(&recs[0]));
    let ApplyOutcome::Merged(_) = m.apply(&id).unwrap() else { panic!("apply") };
    assert_eq!(fs::read_to_string(e.repo.join("hello.txt")).unwrap().trim().trim_end_matches('.'), "hi"); // models sometimes add the sentence period

    // second run: discard cleans up
    let id2 = m.submit(RunSpec { model: sel, ..spec(&e, prompt, true) }).unwrap();
    let end = Instant::now() + Duration::from_secs(std::env::var("CT_LIVE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(170));
    let i2 = loop {
        let i = m.get(&id2).unwrap();
        if !i.state.is_active() {
            break i;
        }
        assert!(Instant::now() < end, "second live run timed out");
        std::thread::sleep(Duration::from_millis(200));
    };
    let wt2 = i2.worktree.clone().unwrap();
    m.discard(&id2).unwrap();
    assert!(!wt2.exists());
    m.shutdown_all();
    // no leftover agent children
    let name = if backend == BackendKind::Codex { "codex" } else { "claude" };
    let out = Command::new("pgrep").args(["-f", &format!("{name}.*{}", e.data.display())]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty(), "leftover {name}: {}", String::from_utf8_lossy(&out.stdout));
}

#[test]
#[ignore = "needs CT_LIVE=1 and a logged-in codex"]
fn live_codex() {
    live(BackendKind::Codex);
}

#[test]
#[ignore = "needs CT_LIVE=1 and a logged-in claude"]
fn live_claude() {
    live(BackendKind::Claude);
}

// ------------------------------------------------------------------ review fixes (t012)

#[test]
fn git_children_get_a_cleaned_env_and_worktree_add_runs_no_hooks() {
    let e = env();
    let out = e.data.parent().unwrap().join("hookenv.txt");
    let wt_hook = e.data.parent().unwrap().join("checkout-ran");
    for (name, body) in [
        ("post-merge", format!("#!/bin/sh\nenv > '{}'\n", out.display())),
        ("post-checkout", format!("#!/bin/sh\ntouch '{}'\n", wt_hook.display())),
    ] {
        let p = e.repo.join(".git/hooks").join(name);
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    }
    std::env::set_var("CT_RUNS_SENTINEL_TOKEN", "sk-live-sentinel-value");
    let m = mgr(&e, 1, Fac::new(|_| Some(edit(&[("n.txt", "n\n")], "i", "o"))));
    let id = finished_edit_run(&e, &m, &[]);
    assert!(!wt_hook.exists(), "post-checkout hook ran during worktree add");
    assert!(matches!(m.apply(&id).unwrap(), ApplyOutcome::Merged(_)));
    std::env::remove_var("CT_RUNS_SENTINEL_TOKEN");
    let dumped = fs::read_to_string(&out).expect("post-merge hook (user hook semantics) must still run on apply");
    assert!(!dumped.contains("CT_RUNS_SENTINEL_TOKEN") && !dumped.contains("sentinel-value"), "secret leaked to hook:\n{dumped}");
    assert!(dumped.contains("PATH=") && dumped.contains("HOME="), "{dumped}");
}

#[test]
fn abort_escalation_does_not_wait_for_a_blocking_session_abort() {
    let e = env();
    let m = mgr(&e, 1, Fac::new(|_| Some(Beh::Hang { ignore_abort: true, block_abort: true })));
    let id = m.submit(spec(&e, "hang", true)).unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    let pid = loop {
        let v: Value = serde_json::from_slice(&fs::read(e.data.join("runs").join(&id).join("run.json")).unwrap()).unwrap();
        if let Some(p) = v["pid"].as_u64() {
            break p as u32;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    };
    let t = Instant::now();
    m.abort(&id).unwrap();
    // session.abort() blocks for 4s; abort_grace + term_grace is 0.6s
    while !pid_gone(pid) {
        assert!(t.elapsed() < Duration::from_millis(2500), "escalation waited for the session ({:?})", t.elapsed());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(done(&m, &id).state, RunState::Aborted);
}

#[test]
fn reconcile_kills_grandchildren_that_outlive_a_dead_leader() {
    let e = env();
    drop(mgr(&e, 1, Fac::new(|_| None)));
    let flag = e.data.parent().unwrap().join("leader-go");
    let gpid_file = e.data.parent().unwrap().join("gpid");
    // leader: starts a TERM-ignoring grandchild, waits for the flag, exits
    let script = format!(
        "trap '' TERM; (sleep 600 & echo $! > '{}'; wait) & while [ ! -e '{}' ]; do sleep 0.02; done",
        gpid_file.display(),
        flag.display()
    );
    let mut leader = Command::new("sh").args(["-c", &script]).process_group(0).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let lpid = leader.id();
    let lstart = ct_runs::process_start_time(lpid).unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    let gpid: u32 = loop {
        if let Ok(s) = fs::read_to_string(&gpid_file) {
            if let Ok(p) = s.trim().parse() {
                break p;
            }
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    };
    fs::write(&flag, "go").unwrap();
    leader.wait().unwrap(); // leader dead + reaped, grandchild survives in its group
    assert!(ct_runs::process_start_time(lpid).is_none());
    assert!(!pid_gone(gpid), "grandchild must survive the leader for this test to mean anything");

    let base = git(&e.repo, &["rev-parse", "HEAD"]);
    let id = "18c00000-0000000d";
    let rec = json!({"id": id, "repo": e.repo, "prompt": "p", "backend": "pi", "provider": null, "model_id": "fake", "thinking": null,
        "base_ref": "HEAD", "isolate": false, "base_sha": base, "branch": null, "worktree": null, "state": "running",
        "state_msg": null, "submitted_ms": 1, "started_ms": 1, "ended_ms": null, "files_changed": 0,
        "pid": lpid, "pid_start": lstart, "head_sha": null});
    let d = e.data.join("runs").join(id);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("run.json"), serde_json::to_vec(&rec).unwrap()).unwrap();
    let m = mgr(&e, 1, Fac::new(|_| None));
    assert_eq!(m.get(id).unwrap().state, RunState::Interrupted);
    let end = Instant::now() + Duration::from_secs(3);
    while !pid_gone(gpid) {
        assert!(Instant::now() < end, "grandchild of a dead leader survived reconcile");
        std::thread::sleep(Duration::from_millis(20));
    }
    let v: Value = serde_json::from_slice(&fs::read(d.join("run.json")).unwrap()).unwrap();
    assert!(v["pid"].is_null(), "pid info cleared only after the group is gone");
}

#[test]
fn accepted_abort_wins_over_a_settled_that_was_already_seen() {
    let e = env();
    let t = Timings { finalize_delay: Duration::from_millis(600), ..FAST };
    let m = RunManager::open_with(&e.data, 1, Fac::new(|_| Some(edit(&[("a.txt", "a\n")], "i", "o"))), t).unwrap();
    let rx = m.subscribe();
    let id = m.submit(spec(&e, "p", true)).unwrap();
    // wait until the supervisor has consumed Settled (it is now committing / finalising)
    loop {
        match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
            RunUpdate::Event(_, AgentEvent::Settled { .. }) => break,
            _ => {}
        }
    }
    m.abort(&id).expect("abort is still acceptable before the terminal state is written");
    assert_eq!(done(&m, &id).state, RunState::Aborted);
    // once finalised, abort is refused and the state stays
    assert!(m.abort(&id).is_err());
    assert_eq!(m.get(&id).unwrap().state, RunState::Aborted);

    // and without an abort the same flow ends Succeeded
    let id2 = m.submit(spec(&e, "p2", true)).unwrap();
    assert_eq!(done(&m, &id2).state, RunState::Succeeded);
}

#[test]
fn apply_merges_only_the_reviewed_head_and_refuses_a_moved_branch() {
    let e = env();
    let m = mgr(&e, 1, Fac::new(|_| Some(edit(&[("n.txt", "n\n")], "i", "o"))));
    let id = finished_edit_run(&e, &m, &[]);
    let info = m.get(&id).unwrap();
    let (wt, branch) = (info.worktree.clone().unwrap(), info.branch.clone().unwrap());
    let (_, reviewed) = m.comparison(&id).unwrap();
    // something lands on the run branch after review
    fs::write(wt.join("sneaky.txt"), "x\n").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["-c", "user.name=x", "-c", "user.email=x@x", "commit", "-q", "-m", "after review"]);
    let moved = git(&e.repo, &["rev-parse", &format!("refs/heads/{branch}")]);
    assert_ne!(moved, reviewed);
    let head = git(&e.repo, &["rev-parse", "HEAD"]);
    let err = m.apply(&id).unwrap_err().to_string();
    assert!(err.contains("moved"), "{err}");
    assert_eq!(git(&e.repo, &["rev-parse", "HEAD"]), head);
    assert!(!e.repo.join("sneaky.txt").exists() && !e.repo.join("n.txt").exists());
    // comparison keeps showing the reviewed head
    assert_eq!(m.comparison(&id).unwrap().1, reviewed);
}
