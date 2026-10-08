//! Resource-guard tests. Own binary + a global lock: they inspect process-wide state (env, threads).
use ct_agentd::with;
use ct_agentd::*;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

static SERIAL: Mutex<()> = Mutex::new(());

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

struct Env {
    dir: tempfile::TempDir,
    cfg: Config,
}
impl Env {
    fn new(scenario: &str) -> Env {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("scenario"), scenario).unwrap();
        let mut cfg = Config::new(dir.path().join("data"));
        cfg.pi_path = Some(fixture("fake-pi"));
        cfg.claude_bin = fixture("fake-claude");
        cfg.rpc_timeout = Duration::from_secs(10);
        Env { dir, cfg }
    }
    fn opts(&self, read_only: bool) -> SessionOpts {
        SessionOpts { cwd: self.dir.path().to_path_buf(), model: None, read_only, env_allow: vec![], system_note: None }
    }
}

fn until_exit(s: &dyn AgentSession, secs: u64) -> Vec<AgentEvent> {
    let end = Instant::now() + Duration::from_secs(secs);
    let mut v = Vec::new();
    while let Ok(e) = s.events().recv_timeout(end.saturating_duration_since(Instant::now())) {
        let done = matches!(e, AgentEvent::Exited(_));
        v.push(e);
        if done {
            break;
        }
    }
    v
}

fn alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "))
}

// ------------------------------------------------------------------ resource guards (t021)

fn threads_of_self() -> usize {
    std::fs::read_dir("/proc/self/task").unwrap().count()
}

/// Thread count once threads of previously finished tests have wound down.
fn settled_threads() -> usize {
    let mut last = threads_of_self();
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        let now = threads_of_self();
        if now == last {
            return now;
        }
        last = now;
    }
    last
}

#[test]
fn child_env_is_minimal_and_carries_depth_guard() {
    let _g = SERIAL.lock();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("env.txt");
    let script = dir.path().join("dump-env");
    std::fs::write(&script, format!("#!/bin/sh\nenv > {}\nprintf '%s\\n' '{{\"type\":\"response\"}}'\n", out.display())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    // secrets in the parent env must not reach the child
    std::env::set_var("ANTHROPIC_API_KEY", "SECRET-PARENT-KEY");
    std::env::set_var("GITHUB_TOKEN", "SECRET-GH");
    let mut env = Env::new("");
    env.cfg.pi_path = Some(script);
    env.cfg.rpc_timeout = Duration::from_millis(300);
    let _ = with::start_session(&env.cfg, BackendKind::Pi, env.opts(false)); // handshake fails; env was dumped
    std::thread::sleep(Duration::from_millis(200));
    let dumped = std::fs::read_to_string(&out).unwrap();
    assert!(!dumped.contains("SECRET"), "{dumped}");
    assert!(dumped.contains("CT_AGENTD_DEPTH=1"));
    assert!(dumped.contains("PI_CODING_AGENT_DIR="));
    let names: Vec<&str> = dumped.lines().filter_map(|l| l.split('=').next()).collect();
    for n in &names {
        let ok = ["PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LANGUAGE", "TERM", "TMPDIR", "TZ", "PWD", "SHLVL", "_", "OLDPWD",
            "CT_AGENTD_DEPTH", "PI_CODING_AGENT_DIR", "PI_SKIP_VERSION_CHECK", "PI_TELEMETRY", "DISPLAY", "COLORTERM"]
            .contains(n)
            || n.starts_with("XDG_")
            || n.starts_with("LC_")
            || n.to_ascii_lowercase().ends_with("_proxy")
            || n.starts_with("SSL_CERT")
            || *n == "NODE_EXTRA_CA_CERTS";
        assert!(ok, "unexpected var in child env: {n}");
    }
}

#[test]
fn ceiling_kills_the_process_group_and_idle_has_no_threads() {
    let _g = SERIAL.lock();
    let before = settled_threads();
    let mut env = Env::new("OUT:{\"type\":\"system\",\"subtype\":\"init\"}\nSLEEP:60");
    env.cfg.ask_ceiling = Duration::from_millis(500);
    let mut s = with::start_session(&env.cfg, BackendKind::Claude, env.opts(false)).unwrap();
    assert_eq!(threads_of_self(), before, "a session that has not prompted owns no threads");
    s.prompt("x").unwrap();
    let pid = s.pid().unwrap() as i32;
    let t = Instant::now();
    let evs = until_exit(s.as_ref(), 10);
    assert!(t.elapsed() < Duration::from_secs(6), "{:?}", t.elapsed());
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::Settled { ok: false, .. })), "{evs:?}");
    assert!(!alive(pid));
    s.close();
    assert!(settled_threads() <= before + 1, "threads leaked: {} -> {}", before, threads_of_self());
}

#[test]
fn pi_idle_session_blocks_without_polling() {
    let _g = SERIAL.lock();
    let env = Env::new("");
    let s = with::start_session(&env.cfg, BackendKind::Pi, env.opts(false)).unwrap();
    let pid = s.pid().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let ticks = |p: &str| -> u64 {
        let st = std::fs::read_to_string(p).unwrap();
        let f: Vec<&str> = st.rsplit(')').next().unwrap().split_whitespace().collect();
        f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap()
    };
    let me = || -> u64 { std::fs::read_dir("/proc/self/task").unwrap().map(|e| ticks(&format!("{}/stat", e.unwrap().path().display()))).sum::<u64>() };
    let a = me();
    std::thread::sleep(Duration::from_secs(2));
    let b = me();
    assert!(b - a <= 2, "idle pi session burned {} cpu ticks (1 tick=10ms) in 2s", b - a);
    let _ = pid;
    s.close();
}
