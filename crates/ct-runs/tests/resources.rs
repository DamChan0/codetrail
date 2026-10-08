//! Resource guards: sampler, per-run limits, global budget. Fake sessions run REAL child
//! processes (memory hog / CPU spinner) in their own process group. Serial: the sampler-thread
//! assertions count threads of the whole test process.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use ct_agentd::{AgentEvent, AgentSession, BackendKind, ModelSel, SessionOpts};
use ct_runs::{Limits, RunInfo, RunManager, RunSpec, RunState, SessionFactory, Timings};

static SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const T: Timings = Timings {
    abort_grace: Duration::from_millis(300),
    term_grace: Duration::from_millis(300),
    tick: Duration::from_millis(10),
    finalize_delay: Duration::ZERO,
    sample_interval: Duration::from_millis(100),
};

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

struct Env {
    tmp: PathBuf,
    _dir: tempfile::TempDir,
    repo: PathBuf,
    data: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().canonicalize().unwrap();
    let repo = tmp.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "user.email", "t@t"]);
    std::fs::write(repo.join("f"), "a\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    Env { data: tmp.join("data"), repo, tmp, _dir: dir }
}

fn spec(e: &Env) -> RunSpec {
    RunSpec {
        repo: e.repo.clone(),
        prompt: "p".into(),
        model: ModelSel { backend: BackendKind::Pi, provider: None, id: "fake".into(), thinking: None },
        base_ref: "HEAD".into(),
        isolate: false,
    }
}

/// Every test leaves no process behind: nothing running with a cwd inside its temp dir, nothing
/// whose command line carries its marker. Checked on drop (declare before the manager).
struct Audit {
    root: PathBuf,
    marker: String,
}

fn live_pids() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|p| {
            std::fs::read_to_string(format!("/proc/{p}/stat")).map(|s| !s[s.rfind(')').unwrap_or(0)..].starts_with(") Z")).unwrap_or(false)
        })
        .collect()
}

fn leftovers(root: &Path, marker: &str) -> Vec<u32> {
    let me = std::process::id();
    live_pids()
        .into_iter()
        .filter(|p| *p != me)
        .filter(|p| {
            let cwd = std::fs::read_link(format!("/proc/{p}/cwd")).ok();
            let cmd = std::fs::read(format!("/proc/{p}/cmdline")).unwrap_or_default();
            cwd.is_some_and(|c| c.starts_with(root)) || String::from_utf8_lossy(&cmd).contains(marker)
        })
        .collect()
}

impl Drop for Audit {
    fn drop(&mut self) {
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            let l = leftovers(&self.root, &self.marker);
            if l.is_empty() {
                return;
            }
            if Instant::now() > end {
                if !std::thread::panicking() {
                    panic!("leftover processes after test: {l:?}");
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn sampler_threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .flatten()
        .filter(|t| std::fs::read_to_string(t.path().join("comm")).is_ok_and(|c| c.trim() == "run-sampler"))
        .count()
}

// ---- fake session: a shell snippet in its own process group, no events ever
struct Fac {
    script: String,
}

struct Fake {
    child: Child,
    _tx: Sender<AgentEvent>,
    rx: Receiver<AgentEvent>,
}

impl SessionFactory for Fac {
    fn start(&self, _b: BackendKind, o: SessionOpts) -> ct_agentd::Result<Box<dyn AgentSession>> {
        let child = Command::new("sh")
            .args(["-c", &self.script])
            .current_dir(&o.cwd)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| ct_agentd::Error::Other(e.to_string()))?;
        let (tx, rx) = channel();
        Ok(Box::new(Fake { child, _tx: tx, rx }))
    }
}

impl AgentSession for Fake {
    fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }
    fn prompt(&mut self, _t: &str) -> ct_agentd::Result<()> {
        Ok(())
    }
    fn steer(&mut self, _t: &str) -> ct_agentd::Result<()> {
        Ok(())
    }
    fn follow_up(&mut self, _t: &str) -> ct_agentd::Result<()> {
        Ok(())
    }
    fn abort(&mut self) -> ct_agentd::Result<()> {
        Ok(())
    }
    fn set_model(&mut self, _m: &ModelSel) -> ct_agentd::Result<()> {
        Ok(())
    }
    fn events(&self) -> &Receiver<AgentEvent> {
        &self.rx
    }
    fn close(mut self: Box<Self>) {
        let _ = self.child.wait();
    }
}

fn hog(mb: u64, marker: &str) -> Arc<Fac> {
    // `b'x' * N` touches every page (a bare bytearray(N) is lazily zeroed and stays out of RSS)
    Arc::new(Fac { script: format!("python3 -c \"x = b'x' * ({mb} * 1024 * 1024); import time; time.sleep(600)  # {marker}\" & wait") })
}

fn spin() -> Arc<Fac> {
    Arc::new(Fac { script: "(while :; do :; done) & wait".into() })
}

fn sleeper() -> Arc<Fac> {
    Arc::new(Fac { script: "sleep 600 & wait".into() })
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < end, "timeout waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for(m: &RunManager, id: &str, what: &str, secs: u64, pred: impl Fn(&RunInfo) -> bool) -> RunInfo {
    let end = Instant::now() + Duration::from_secs(secs);
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

#[test]
fn memory_hog_is_stopped_with_resource_limit_reason_and_group_dies() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = env();
    let marker = format!("hogmarker-{}", std::process::id());
    let _audit = Audit { root: e.tmp.clone(), marker: marker.clone() };
    let m = RunManager::open_with(&e.data, 1, hog(400, &marker), T).unwrap();
    m.set_limits(Limits { max_rss_mb: 150, ..Limits::default() });
    let t0 = Instant::now();
    let id = m.submit(spec(&e)).unwrap();
    let mut peak_kb = 0;
    let mut procs = 0;
    let info = loop {
        for (rid, r) in m.resources() {
            assert_eq!(rid, id);
            peak_kb = peak_kb.max(r.rss_kb);
            procs = procs.max(r.procs);
        }
        let i = m.get(&id).unwrap();
        if !i.state.is_active() {
            break i;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "run never stopped; peak {peak_kb} KB");
        std::thread::sleep(Duration::from_millis(10));
    };
    let took = t0.elapsed();
    match &info.state {
        RunState::Failed(r) => assert!(r.contains("resource limit") && r.contains("memory"), "{r}"),
        s => panic!("expected Failed(resource limit), got {s:?}"),
    }
    assert!(peak_kb > 300 * 1024, "sampler saw only {peak_kb} KB of a 400 MB hog");
    assert!(procs >= 2, "group totals must include sh + python, saw {procs}");
    assert!(leftovers(&e.tmp, &marker).is_empty(), "group not dead: {:?}", leftovers(&e.tmp, &marker));
    eprintln!("MEASURED hog: peak_group_rss={} MB (hog 400 MB, limit 150 MB), stopped+dead after {took:?}, procs seen={procs}", peak_kb / 1024);
    assert!(m.resources().is_empty());
}

#[test]
fn cpu_spinner_is_stopped_by_cpu_budget() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = env();
    let _audit = Audit { root: e.tmp.clone(), marker: "never-matches-zzz".into() };
    let m = RunManager::open_with(&e.data, 1, spin(), T).unwrap();
    m.set_limits(Limits { max_cpu_seconds_total: Some(1), ..Limits::default() });
    let t0 = Instant::now();
    let id = m.submit(spec(&e)).unwrap();
    let mut peak_pct = 0f32;
    let info = loop {
        for (_, r) in m.resources() {
            peak_pct = peak_pct.max(r.cpu_pct);
        }
        let i = m.get(&id).unwrap();
        if !i.state.is_active() {
            break i;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "spinner never stopped");
        std::thread::sleep(Duration::from_millis(10));
    };
    match &info.state {
        RunState::Failed(r) => assert!(r.contains("resource limit") && r.contains("CPU"), "{r}"),
        s => panic!("{s:?}"),
    }
    assert!(peak_pct > 50.0, "cpu_pct of a spinner was {peak_pct}");
    eprintln!("MEASURED spinner: peak cpu_pct={peak_pct:.0}, cpu budget 1s hit and group dead after {:?}", t0.elapsed());
}

#[test]
fn sampler_thread_exists_only_while_runs_are_active() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = env();
    let _audit = Audit { root: e.tmp.clone(), marker: "never-matches-zzz".into() };
    let m = RunManager::open_with(&e.data, 2, sleeper(), T).unwrap();
    // a sampler of the previous test may still be on its way out: poll, don't assume
    wait_until("no sampler for an idle manager", || sampler_threads() == 0);
    let a = m.submit(spec(&e)).unwrap();
    let b = m.submit(spec(&e)).unwrap();
    // both runs have a sample (session start + first tick are asynchronous)
    wait_until("a sample of both runs", || {
        let r = m.resources();
        r.len() == 2 && r.iter().all(|(_, r)| r.procs >= 2 && r.rss_kb > 0)
    });
    assert_eq!(sampler_threads(), 1, "exactly one sampler for two runs");
    m.abort(&a).unwrap();
    m.abort(&b).unwrap();
    wait_for(&m, &a, "a done", 10, |i| !i.state.is_active());
    wait_for(&m, &b, "b done", 10, |i| !i.state.is_active());
    let end = Instant::now() + Duration::from_secs(2);
    while sampler_threads() != 0 {
        assert!(Instant::now() < end, "sampler still alive with no active run");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(m.resources().is_empty());
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(sampler_threads(), 0);
    // and it comes back for the next run
    let c = m.submit(spec(&e)).unwrap();
    wait_until("sampler restarts", || sampler_threads() == 1);
    m.abort(&c).unwrap();
    wait_for(&m, &c, "c done", 10, |i| !i.state.is_active());
    let end = Instant::now() + Duration::from_secs(2);
    while sampler_threads() != 0 {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn global_rss_budget_keeps_new_runs_queued() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = env();
    let marker = format!("budgetmarker-{}", std::process::id());
    let _audit = Audit { root: e.tmp.clone(), marker: marker.clone() };
    let m = RunManager::open_with(&e.data, 2, hog(250, &marker), T).unwrap();
    m.set_limits(Limits { max_rss_mb: 2000, ..Limits::default() });
    m.set_max_total_rss_mb(100);
    let a = m.submit(spec(&e)).unwrap();
    // first run always starts (nothing else running); wait until it has blown the budget
    let end = Instant::now() + Duration::from_secs(15);
    while m.resources().iter().map(|(_, r)| r.rss_kb).sum::<u64>() < 200 * 1024 {
        assert!(Instant::now() < end, "hog never showed up in resources");
        std::thread::sleep(Duration::from_millis(20));
    }
    let b = m.submit(spec(&e)).unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(m.get(&b).unwrap().state, RunState::Queued, "budget exceeded: second run must stay queued");
    assert_eq!(m.get(&a).unwrap().state, RunState::Running);
    eprintln!("MEASURED budget: run A rss={} MB > budget 100 MB -> run B Queued for >=1.2s", m.resources()[0].1.rss_kb / 1024);
    // budget frees up -> queue resumes without any other trigger
    m.abort(&a).unwrap();
    wait_for(&m, &a, "a done", 10, |i| !i.state.is_active());
    wait_for(&m, &b, "b started", 10, |i| i.state == RunState::Running);
    m.abort(&b).unwrap();
    wait_for(&m, &b, "b done", 10, |i| !i.state.is_active());
}
