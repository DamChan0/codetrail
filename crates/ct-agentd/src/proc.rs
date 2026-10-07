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
}

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

fn spawn_in_thread(spec: Spec) -> io::Result<Spawned> {
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
