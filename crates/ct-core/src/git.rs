//! git subprocess runner: timeout + cancellation (kills the child), streaming stdout.
//!
//! The same machinery runs `git` locally or on a remote host through `ssh` (see [`crate::remote`]):
//! a remote call is just another child that holds one of the [`MAX_GIT_PROCS`] slots, lives in its
//! own process group, is killed on timeout/cancel and has its stdout capped.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use parking_lot::Mutex;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::remote::{exec, RemoteErrorKind, RemoteSpec};
use crate::{Error, Result};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default per-call timeout for remote repositories (ssh round trips are slower).
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard cap for buffered stdout (and for a single streamed line).
pub const MAX_OUTPUT: usize = 64 * 1024 * 1024;
/// Default cap for the cumulative stdout of one streamed command ([`run_lines`]).
pub const MAX_TOTAL_OUTPUT: usize = 256 * 1024 * 1024;
const MAX_STDERR: usize = 64 * 1024;
/// Process-wide cap on concurrently running git subprocesses.
pub const MAX_GIT_PROCS: usize = 4;

#[derive(Debug, Clone)]
pub struct RunOpts {
    pub timeout: Duration,
    pub cancel: Option<Arc<AtomicBool>>,
    pub max_output: usize,
    /// `Some`: run git on that host through ssh instead of locally (`cwd` is then a remote path).
    pub remote: Option<Arc<RemoteSpec>>,
    /// Cumulative stdout cap of a streamed command (`run_lines*`); exceeding it kills the child and
    /// yields `Error::TooLarge`. Buffered `run` is governed by `max_output` alone.
    pub max_total_output: usize,
    /// Remote only: let the ssh ControlMaster exit with this call (`ControlPersist=no`) instead of
    /// lingering 120 s (connection tests).
    pub ephemeral_master: bool,
}

impl Default for RunOpts {
    fn default() -> Self {
        RunOpts { timeout: DEFAULT_TIMEOUT, cancel: None, max_output: MAX_OUTPUT, remote: None, max_total_output: MAX_TOTAL_OUTPUT, ephemeral_master: false }
    }
}

/// Counting semaphore bounding live git subprocesses. Blocking acquire honours the call's timeout
/// and cancel flag, so a saturated pool can never wedge a caller forever.
pub struct GitSlots {
    used: Mutex<usize>,
    cv: parking_lot::Condvar,
    limit: usize,
}

pub struct Slot(&'static GitSlots);

impl Drop for Slot {
    fn drop(&mut self) {
        *self.0.used.lock() -= 1;
        self.0.cv.notify_one();
    }
}

impl GitSlots {
    fn acquire(&'static self, o: &RunOpts, deadline: Instant) -> Result<Slot> {
        let mut g = self.used.lock();
        loop {
            if let Some(c) = &o.cancel {
                if c.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
            }
            if *g < self.limit {
                *g += 1;
                return Ok(Slot(self));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Timeout(o.timeout));
            }
            let _ = self.cv.wait_for(&mut g, (deadline - now).min(Duration::from_millis(50)));
        }
    }

    /// Git subprocesses currently holding a slot.
    pub fn in_use(&self) -> usize {
        *self.used.lock()
    }
}

static GIT_SLOTS: std::sync::LazyLock<GitSlots> =
    std::sync::LazyLock::new(|| GitSlots { used: Mutex::new(0), cv: parking_lot::Condvar::new(), limit: MAX_GIT_PROCS });

/// The process-wide git subprocess limiter (at most [`MAX_GIT_PROCS`] = 4 concurrent children).
pub fn git_slots() -> &'static GitSlots {
    &GIT_SLOTS
}

const WHY_NONE: u8 = 0;
const WHY_TIMEOUT: u8 = 1;
const WHY_CANCEL: u8 = 2;

/// A running git process with a watchdog thread enforcing timeout/cancel.
pub struct Proc {
    child: Arc<Mutex<Option<Child>>>,
    stdout: Option<ChildStdout>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    stdin: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
    why: Arc<AtomicU8>,
    watchdog: Option<thread::Thread>,
    desc: String,
    timeout: Duration,
    remote: bool,
    /// Held until the child is reaped (field order: dropped last).
    _slot: Slot,
}

impl Proc {
    pub fn spawn(cwd: &Path, args: &[&str], stdin_data: Option<Vec<u8>>, o: &RunOpts) -> Result<Proc> {
        if let Some(c) = &o.cancel {
            if c.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
        }
        if let Some(spec) = &o.remote {
            let cwd = cwd.to_str().ok_or_else(|| Error::Invalid(format!("remote path {cwd:?} is not UTF-8")))?;
            let script = exec::git_script(cwd, args)?;
            let cmd = exec::ssh_command(spec, &exec::wrap_sh(&script), !o.ephemeral_master)?;
            let desc = format!("git {} (on {})", args.join(" "), spec.name);
            return Proc::start(cmd, desc, true, stdin_data, o);
        }
        let mut cmd = Command::new("git");
        cmd.current_dir(cwd)
            .args(["-c", "core.quotepath=false", "-c", "color.ui=never", "-c", "core.pager=cat"])
            .args(args)
            .env("LC_ALL", "C")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat");
        Proc::start(cmd, format!("git {}", args.join(" ")), false, stdin_data, o)
    }

    /// Runs a (non-git) shell `script` on the remote under the same limits.
    pub(crate) fn spawn_remote_script(spec: &RemoteSpec, script: &str, desc: &str, stdin_data: Option<Vec<u8>>, o: &RunOpts) -> Result<Proc> {
        if let Some(c) = &o.cancel {
            if c.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
        }
        exec::check_no_nul(script)?;
        let cmd = exec::ssh_command(spec, &exec::wrap_sh(script), !o.ephemeral_master)?;
        Proc::start(cmd, format!("{desc} (on {})", spec.name), true, stdin_data, o)
    }

    fn start(mut cmd: Command, desc: String, remote: bool, stdin_data: Option<Vec<u8>>, o: &RunOpts) -> Result<Proc> {
        // one absolute deadline for the whole call: slot wait and runtime share it
        let deadline = Instant::now() + o.timeout;
        let slot = git_slots().acquire(o, deadline)?;
        cmd.process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() });
        let mut child = cmd.spawn().map_err(|e| Error::Spawn(format!("{desc}: {e}")))?;
        let stdout = child.stdout.take();
        let mut stderr_pipe = child.stderr.take().expect("piped");
        let stderr = thread::spawn(move || {
            // keep the head for the error message, drain (and drop) the rest
            let mut b = Vec::new();
            let _ = (&mut stderr_pipe).take(MAX_STDERR as u64).read_to_end(&mut b);
            let _ = std::io::copy(&mut stderr_pipe, &mut std::io::sink());
            b
        });
        let stdin = match (stdin_data, child.stdin.take()) {
            (Some(data), Some(mut pipe)) => Some(thread::spawn(move || {
                let _ = pipe.write_all(&data);
            })),
            _ => None,
        };
        let child = Arc::new(Mutex::new(Some(child)));
        let done = Arc::new(AtomicBool::new(false));
        let why = Arc::new(AtomicU8::new(WHY_NONE));
        let watchdog = {
            let (child, done, why) = (child.clone(), done.clone(), why.clone());
            let cancel = o.cancel.clone();
            let h = thread::spawn(move || {
                while !done.load(Ordering::Acquire) {
                    let reason = if cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
                        WHY_CANCEL
                    } else if Instant::now() >= deadline {
                        WHY_TIMEOUT
                    } else {
                        WHY_NONE
                    };
                    if reason != WHY_NONE {
                        let mut g = child.lock();
                        if let Some(c) = g.as_mut() {
                            why.store(reason, Ordering::Release);
                            kill_group(c);
                        }
                        return;
                    }
                    thread::park_timeout(Duration::from_millis(5));
                }
            });
            h.thread().clone()
        };
        Ok(Proc {
            child,
            stdout,
            stderr: Some(stderr),
            stdin,
            done,
            why,
            watchdog: Some(watchdog),
            desc,
            timeout: o.timeout,
            remote,
            _slot: slot,
        })
    }

    pub fn take_stdout(&mut self) -> ChildStdout {
        self.stdout.take().expect("stdout already taken")
    }

    /// Kill the child (used when a streaming consumer stops early).
    pub fn kill(&self) {
        if let Some(c) = self.child.lock().as_mut() {
            kill_group(c);
        }
    }

    /// Reap the child. `ok_codes`: exit codes treated as success (0 always).
    /// Returns Ok(exit_code) or Err (cancel/timeout/failed with stderr).
    pub fn finish(mut self, ok_codes: &[i32]) -> Result<i32> {
        let status = {
            let mut g = self.child.lock();
            let mut c = g.take();
            drop(g);
            c.as_mut().map(|c| c.wait())
        };
        self.done.store(true, Ordering::Release);
        if let Some(w) = self.watchdog.take() {
            w.unpark();
        }
        if let Some(h) = self.stdin.take() {
            let _ = h.join();
        }
        let stderr = self.stderr.take().map(|h| h.join().unwrap_or_default()).unwrap_or_default();
        match self.why.load(Ordering::Acquire) {
            WHY_CANCEL => return Err(Error::Cancelled),
            WHY_TIMEOUT => return Err(Error::Timeout(self.timeout)),
            _ => {}
        }
        let status = status.expect("child present").map_err(|e| Error::Spawn(format!("{}: {e}", self.desc)))?;
        let code = status.code().unwrap_or(-1);
        if code == 0 || ok_codes.contains(&code) {
            Ok(code)
        } else {
            let text = String::from_utf8_lossy(&stderr).trim().to_string();
            if self.remote {
                // 255 is ssh's own failure code; other codes only count when the text is recognisable
                let kind = exec::classify_stderr(&text).or((code == 255).then_some(RemoteErrorKind::Other));
                if let Some(kind) = kind {
                    return Err(Error::Remote { kind, detail: exec::short_detail(&text) });
                }
            }
            Err(Error::Git { cmd: self.desc.clone(), code, stderr: text })
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        // Early-dropped (e.g. streaming consumer bailed out): make sure nothing lingers.
        if !self.done.load(Ordering::Acquire) {
            self.kill();
            if let Some(c) = self.child.lock().take().as_mut() {
                let _ = c.wait();
            }
            self.done.store(true, Ordering::Release);
            if let Some(w) = self.watchdog.take() {
                w.unpark();
            }
        }
        // stderr/stdin helper threads end with the pipes; the slot is released by `_slot`'s drop
    }
}

/// SIGKILL the child's whole process group (git helpers, hooks, filters, ssh) while the leader is
/// still unreaped, so its pid cannot have been recycled.
fn kill_group(c: &mut Child) {
    unsafe {
        libc::kill(-(c.id() as i32), libc::SIGKILL);
    }
    let _ = c.kill();
}

/// Run to completion, returning stdout bytes. Non-zero exit → Err.
pub fn run(cwd: &Path, args: &[&str], stdin: Option<Vec<u8>>, o: &RunOpts) -> Result<Vec<u8>> {
    run_ok(cwd, args, stdin, o, &[]).map(|(b, _)| b)
}

/// Like [`run`] but additionally treats `ok_codes` as success; returns (stdout, exit code).
pub fn run_ok(
    cwd: &Path,
    args: &[&str],
    stdin: Option<Vec<u8>>,
    o: &RunOpts,
    ok_codes: &[i32],
) -> Result<(Vec<u8>, i32)> {
    let p = Proc::spawn(cwd, args, stdin, o)?;
    collect(p, &p_desc(args), o, ok_codes)
}

/// Runs a shell script on the remote (one ssh call); same limits as git calls.
pub(crate) fn run_remote_script(
    spec: &RemoteSpec,
    script: &str,
    desc: &str,
    stdin: Option<Vec<u8>>,
    o: &RunOpts,
    ok_codes: &[i32],
) -> Result<(Vec<u8>, i32)> {
    let p = Proc::spawn_remote_script(spec, script, desc, stdin, o)?;
    collect(p, desc, o, ok_codes)
}

fn collect(mut p: Proc, desc: &str, o: &RunOpts, ok_codes: &[i32]) -> Result<(Vec<u8>, i32)> {
    let mut out = Vec::new();
    let mut so = p.take_stdout();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut too_large = false;
    let read: std::io::Result<()> = loop {
        match so.read(&mut chunk) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                if out.len() + n > o.max_output {
                    too_large = true;
                    break Ok(());
                }
                out.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => break Err(e),
        }
    };
    if too_large {
        // stop the producer first; the partial buffer is dropped
        p.kill();
        drop(so);
        let _ = p.finish(&[-1, 128 + 9, 141]);
        return Err(Error::TooLarge { cmd: desc.to_string(), limit: o.max_output });
    }
    drop(so);
    let code = p.finish(ok_codes)?;
    read.map_err(|e| Error::Spawn(format!("read stdout: {e}")))?;
    Ok((out, code))
}

/// Stream stdout lines (split on `\n`); `f` returns false to stop early (child is killed).
pub fn run_lines(cwd: &Path, args: &[&str], o: &RunOpts, f: impl FnMut(&[u8]) -> bool) -> Result<()> {
    run_lines_ok(cwd, args, o, &[], f)
}

/// [`run_lines`] that additionally treats `ok_codes` as success (e.g. `git grep` exits 1 for "no match").
pub fn run_lines_ok(
    cwd: &Path,
    args: &[&str],
    o: &RunOpts,
    ok_codes: &[i32],
    mut f: impl FnMut(&[u8]) -> bool,
) -> Result<()> {
    let mut p = Proc::spawn(cwd, args, None, o)?;
    let mut rd = BufReader::with_capacity(64 * 1024, p.take_stdout());
    let mut buf = Vec::new();
    let mut stopped = false;
    let mut total = 0usize;
    loop {
        buf.clear();
        // a single line is bounded too (a huge minified blob in a diff must not exhaust memory)
        match (&mut rd).take(o.max_output as u64 + 1).read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {
                total = total.saturating_add(buf.len());
                if buf.len() > o.max_output || total > o.max_total_output {
                    p.kill();
                    drop(rd);
                    let _ = p.finish(&[-1, 128 + 9, 141]);
                    return Err(Error::TooLarge { cmd: p_desc(args), limit: if buf.len() > o.max_output { o.max_output } else { o.max_total_output } });
                }
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                }
                if !f(&buf) {
                    stopped = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if stopped {
        p.kill();
        drop(rd);
        // Killed on purpose: exit status is irrelevant.
        let _ = p.finish(&[-1, 128 + 9, 141]);
        return Ok(());
    }
    drop(rd);
    p.finish(ok_codes).map(|_| ())
}

fn p_desc(args: &[&str]) -> String {
    format!("git {}", args.join(" "))
}

pub fn is_hex40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review #6: the timeout is one absolute deadline for slot wait + runtime (it used to restart
    /// after the wait, allowing ~2x). This is the only lib unit test that touches the slot pool.
    #[test]
    fn timeout_covers_slot_wait_plus_runtime() {
        let o = RunOpts { timeout: Duration::from_millis(700), ..RunOpts::default() };
        let far = Instant::now() + Duration::from_secs(30);
        // hold every slot ourselves, free one after 0.4 s
        let mut held: Vec<Slot> = (0..MAX_GIT_PROCS).map(|_| git_slots().acquire(&o, far).unwrap()).collect();
        let one = held.pop().unwrap();
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(400));
            drop(one);
        });
        let t0 = Instant::now();
        let mut cmd = Command::new("sleep");
        cmd.arg("0.5"); // would end at ~0.9 s: after the 0.7 s budget, before a restarted budget
        let mut p = Proc::start(cmd, "sleep".into(), false, None, &o).unwrap();
        let waited = t0.elapsed();
        // like `collect`: read to EOF (the watchdog kills the group at the deadline), then reap
        let _ = std::io::copy(&mut p.take_stdout(), &mut std::io::sink());
        let e = p.finish(&[]).unwrap_err();
        let total = t0.elapsed();
        releaser.join().unwrap();
        drop(held);
        assert!(waited >= Duration::from_millis(350), "slot was free too early: {waited:?}");
        assert!(matches!(e, Error::Timeout(_)), "{e:?}");
        assert!(total < Duration::from_millis(700 + 250), "{total:?}");
    }

    /// Review #3: the cumulative cap applies across lines, not per line.
    #[test]
    fn total_output_budget_is_cumulative() {
        let dir = std::env::temp_dir();
        // `git --list-cmds=main` prints many short lines (a few KiB in total)
        let small = RunOpts { max_total_output: 200, ..RunOpts::default() };
        let mut n = 0;
        let e = run_lines(&dir, &["--list-cmds=main,nohelpers"], &small, |_| {
            n += 1;
            true
        })
        .unwrap_err();
        assert!(matches!(e, Error::TooLarge { limit: 200, .. }), "{e:?}");
        assert!(n > 3, "streamed some lines first ({n})");
        let mut total = 0;
        run_lines(&dir, &["--list-cmds=main,nohelpers"], &RunOpts::default(), |l| {
            total += l.len();
            true
        })
        .unwrap();
        assert!(total > 200);
    }
}
