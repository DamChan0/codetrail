//! Child process hosting: own session/process group, `PR_SET_PDEATHSIG`, env whitelist,
//! staged termination.

use parking_lot::{Condvar, Mutex};
use std::ffi::OsString;
use std::io::{self, Read as _};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Exit info: `Some(code)` for a normal exit, `None` code for death by signal.
pub(crate) type ExitCode = Option<i32>;

struct Shared {
    pid: u32,
    exit: Mutex<Option<ExitCode>>,
    cv: Condvar,
}

/// Handle to a hosted child. Cheap to clone.
#[derive(Clone)]
pub(crate) struct Host(Arc<Shared>);

pub(crate) struct Spawned {
    pub host: Host,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

pub(crate) struct Spec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    /// The complete child environment (the parent's is never inherited).
    pub env: Vec<(String, String)>,
    /// Hard wall-clock ceiling: the whole process group is terminated (SIGTERM, then SIGKILL) when it
    /// elapses. `None` = the owner of the handle is responsible (it kills on drop/close).
    pub ceiling: Option<Duration>,
}

/// Variables forwarded to every child (besides `LC_*`, backend-specific vars and caller `env_allow`).
///
/// PLAN §10.6 names PATH/HOME/LANG as the core; the rest is deliberately wider and each group is needed:
/// * `HTTP(S)_PROXY`/`ALL_PROXY`/`NO_PROXY` (+ lowercase), `SSL_CERT_*`, `NODE_EXTRA_CA_CERTS`: agents
///   must reach their API behind corporate proxies / private CAs. Proxy URLs can embed credentials;
///   they are the user's own network config and are accepted knowingly (decision t013).
/// * `XDG_*`: relocate where claude/codex keep their login state, so they stay logged in.
/// * `USER`, `LOGNAME`, `SHELL`, `TERM`, `TMPDIR`, `TZ`, `LANGUAGE`: tool basics (shell tool, temp files).
///
/// No provider API keys or tokens are forwarded unless the caller lists them in `env_allow`.
const PASS_THROUGH: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LANGUAGE", "TERM", "TMPDIR", "TZ", "XDG_CONFIG_HOME",
    "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR", "HTTP_PROXY", "HTTPS_PROXY",
    "NO_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "no_proxy", "all_proxy", "SSL_CERT_FILE", "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
];

/// Whitelisted environment: common basics + `backend_vars` (what that backend needs) + `allow`
/// (caller-requested names). Every other variable — in particular other providers' tokens — is dropped.
pub(crate) fn whitelist_env(backend_vars: &[&str], allow: &[String]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in std::env::vars_os() {
        let (Some(k), Some(v)) = (k.to_str(), v.to_str()) else { continue };
        let keep = k.starts_with("LC_")
            || PASS_THROUGH.contains(&k)
            || backend_vars.contains(&k)
            || allow.iter().any(|a| a == k);
        if keep {
            out.push((k.to_string(), v.to_string()));
        }
    }
    out
}

pub(crate) fn set_env(env: &mut Vec<(String, String)>, k: &str, v: impl Into<String>) {
    env.retain(|(ek, _)| ek != k);
    env.push((k.to_string(), v.into()));
}

/// Spawn-depth guard: every child carries `CT_AGENTD_DEPTH`; a process that was itself started by us
/// (e.g. a PATH shim that re-enters codetrail) refuses to fan out past this depth, so a recursive
/// PATH entry can never multiply processes.
pub(crate) const DEPTH_VAR: &str = "CT_AGENTD_DEPTH";
const MAX_DEPTH: u32 = 3;

fn check_depth(v: Option<&str>) -> io::Result<u32> {
    let depth: u32 = v.and_then(|v| v.parse().ok()).unwrap_or(0);
    if depth >= MAX_DEPTH {
        return Err(io::Error::other(format!("refusing to spawn: nested agent depth {depth} (recursive PATH?)")));
    }
    Ok(depth)
}

fn spawn_in_thread(mut spec: Spec) -> io::Result<Spawned> {
    let depth = check_depth(std::env::var(DEPTH_VAR).ok().as_deref())?;
    set_env(&mut spec.env, DEPTH_VAR, (depth + 1).to_string());
    let (tx, rx) = mpsc::channel::<io::Result<Spawned>>();
    // The child's PDEATHSIG fires when the *thread* that forked it exits, so the thread that
    // spawns also waits for the child: it outlives it by construction.
    std::thread::Builder::new().name("ct-agentd-host".into()).spawn(move || {
        let parent = std::process::id();
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args).env_clear().envs(spec.env.iter().map(|(k, v)| (k, v)));
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // SAFETY: only async-signal-safe libc calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Parent may have died between fork and prctl.
                if libc::getppid() as u32 != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        let pid = child.id();
        let shared = Arc::new(Shared { pid, exit: Mutex::new(None), cv: Condvar::new() });
        let (stdin, stdout, stderr) = match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = tx.send(Err(io::Error::other("child stdio missing")));
                return;
            }
        };
        let host = Host(shared.clone());
        if let Some(c) = spec.ceiling {
            // blocks on the exit condvar: no polling, ends as soon as the child is gone
            let h = host.clone();
            let _ = std::thread::Builder::new().name("ct-agentd-ceiling".into()).spawn(move || {
                if h.wait_exit(c).is_none() {
                    h.terminate(Duration::from_secs(2));
                }
            });
        }
        if tx.send(Ok(Spawned { host, stdin, stdout, stderr })).is_err() {
            // caller vanished: no one will manage this child
            kill_group(pid, libc::SIGKILL);
            let _ = child.wait();
            return;
        }
        let code = child.wait().ok().and_then(|st| st.code());
        // Reap stragglers of the group (grandchildren) so nothing outlives the session.
        kill_group(pid, libc::SIGKILL);
        *shared.exit.lock() = Some(code);
        shared.cv.notify_all();
    })?;
    rx.recv().map_err(|_| io::Error::other("host thread died before spawning"))?
}

pub(crate) fn spawn(spec: Spec) -> io::Result<Spawned> {
    spawn_in_thread(spec)
}

fn kill_group(pid: u32, sig: libc::c_int) {
    // SAFETY: plain syscall; negative pid addresses the process group led by `pid`.
    unsafe {
        libc::kill(-(pid as libc::pid_t), sig);
    }
}

impl Host {
    pub fn pid(&self) -> u32 {
        self.0.pid
    }

    /// `Some(code)` once the child has exited (inner `None` = killed by a signal).
    pub fn exited(&self) -> Option<ExitCode> {
        *self.0.exit.lock()
    }

    pub fn wait_exit(&self, d: Duration) -> Option<ExitCode> {
        let deadline = Instant::now() + d;
        let mut g = self.0.exit.lock();
        while g.is_none() {
            if self.0.cv.wait_until(&mut g, deadline).timed_out() {
                break;
            }
        }
        *g
    }

    pub fn signal_group(&self, sig: libc::c_int) {
        if self.exited().is_none() {
            kill_group(self.0.pid, sig);
        }
    }

    /// SIGTERM(group) -> wait `grace_term` -> SIGKILL(group). Blocking.
    pub fn terminate(&self, grace_term: Duration) -> Option<ExitCode> {
        self.signal_group(libc::SIGTERM);
        if let Some(c) = self.wait_exit(grace_term) {
            return Some(c);
        }
        self.signal_group(libc::SIGKILL);
        self.wait_exit(Duration::from_secs(2))
    }

    /// Same as [`terminate`](Self::terminate) without blocking the caller.
    pub fn terminate_detached(&self, grace_term: Duration) {
        let h = self.clone();
        let _ = std::thread::Builder::new().name("ct-agentd-term".into()).spawn(move || {
            h.terminate(grace_term);
        });
    }
}

/// Runs a short-lived command to completion with a timeout, capturing stdout (masked stderr on failure).
pub(crate) struct Captured {
    pub code: ExitCode,
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub timed_out: bool,
}

pub(crate) fn run_capture(spec: Spec, timeout: Duration) -> io::Result<Captured> {
    let sp = spawn(spec)?;
    drop(sp.stdin);
    let host = sp.host.clone();
    let mut out = sp.stdout;
    let mut err = sp.stderr;
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = (&mut out).take(16 * 1024 * 1024).read_to_end(&mut b);
        b
    });
    let t_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = (&mut err).take(1024 * 1024).read_to_end(&mut b);
        b
    });
    let mut timed_out = false;
    let code = match host.wait_exit(timeout) {
        Some(c) => c,
        None => {
            timed_out = true;
            host.terminate(Duration::from_secs(2)).unwrap_or(None)
        }
    };
    let stdout = t_out.join().unwrap_or_default();
    let stderr = crate::util::lossy_masked(&t_err.join().unwrap_or_default());
    Ok(Captured { code, stdout, stderr, timed_out })
}

#[cfg(test)]
mod tests {
    use super::check_depth;

    #[test]
    fn depth_guard_stops_nested_spawns() {
        assert_eq!(check_depth(None).unwrap(), 0);
        assert_eq!(check_depth(Some("2")).unwrap(), 2);
        assert!(check_depth(Some("3")).unwrap_err().to_string().contains("nested agent depth"));
        assert_eq!(check_depth(Some("junk")).unwrap(), 0);
    }
}
