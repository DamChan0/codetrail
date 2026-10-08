//! git subprocess runner: timeout + cancellation (kills the child), streaming stdout.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use parking_lot::Mutex;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::{Error, Result};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard cap for buffered stdout (and for a single streamed line).
pub const MAX_OUTPUT: usize = 64 * 1024 * 1024;
const MAX_STDERR: usize = 64 * 1024;
/// Process-wide cap on concurrently running git subprocesses.
pub const MAX_GIT_PROCS: usize = 4;

#[derive(Debug, Clone)]
pub struct RunOpts {
    pub timeout: Duration,
    pub cancel: Option<Arc<AtomicBool>>,
    pub max_output: usize,
}

impl Default for RunOpts {
    fn default() -> Self {
        RunOpts { timeout: DEFAULT_TIMEOUT, cancel: None, max_output: MAX_OUTPUT }
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
    fn acquire(&'static self, o: &RunOpts) -> Result<Slot> {
        let deadline = Instant::now() + o.timeout;
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
        let slot = git_slots().acquire(o)?;
        let mut cmd = Command::new("git");
        cmd.current_dir(cwd)
            .args(["-c", "core.quotepath=false", "-c", "color.ui=never", "-c", "core.pager=cat"])
            .args(args)
            .env("LC_ALL", "C")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() });
        let desc = format!("git {}", args.join(" "));
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
            let deadline = Instant::now() + o.timeout;
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
            Err(Error::Git {
                cmd: self.desc.clone(),
                code,
                stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
            })
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
    let mut p = Proc::spawn(cwd, args, stdin, o)?;
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
        return Err(Error::TooLarge { cmd: p_desc(args), limit: o.max_output });
    }
    drop(so);
    let code = p.finish(ok_codes)?;
    read.map_err(|e| Error::Spawn(format!("read stdout: {e}")))?;
    Ok((out, code))
}

/// Stream stdout lines (split on `\n`); `f` returns false to stop early (child is killed).
pub fn run_lines(
    cwd: &Path,
    args: &[&str],
    o: &RunOpts,
    mut f: impl FnMut(&[u8]) -> bool,
) -> Result<()> {
    let mut p = Proc::spawn(cwd, args, None, o)?;
    let mut rd = BufReader::with_capacity(64 * 1024, p.take_stdout());
    let mut buf = Vec::new();
    let mut stopped = false;
    loop {
        buf.clear();
        // a single line is bounded too (a huge minified blob in a diff must not exhaust memory)
        match (&mut rd).take(o.max_output as u64 + 1).read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {
                if buf.len() > o.max_output {
                    p.kill();
                    drop(rd);
                    let _ = p.finish(&[-1, 128 + 9, 141]);
                    return Err(Error::TooLarge { cmd: p_desc(args), limit: o.max_output });
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
    p.finish(&[]).map(|_| ())
}

fn p_desc(args: &[&str]) -> String {
    format!("git {}", args.join(" "))
}

pub fn is_hex40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit())
}
