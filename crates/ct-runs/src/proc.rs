//! Process identity (pid + start time) and process-group signalling. Linux `/proc` based.

use std::time::{Duration, Instant};

/// `/proc/<pid>/stat` start time (clock ticks since boot). `None` if the process does not exist or
/// is a zombie (already dead, only waiting to be reaped).
pub fn process_start_time(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "<pid> (<comm, may contain spaces and parens>) <state> ... "; fields after the LAST ')'.
    let rest = &s[s.rfind(')')? + 2..];
    let mut f = rest.split_whitespace();
    let state = f.next()?;
    if state == "Z" || state == "X" {
        return None;
    }
    // state is field 3; starttime is field 22 => 19 fields after state.
    f.nth(18)?.parse().ok()
}

/// The recorded process (same pid AND same start time) is still running.
pub fn is_alive(pid: u32, start: Option<u64>) -> bool {
    match (process_start_time(pid), start) {
        (Some(now), Some(then)) => now == then,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

fn pid_is_signalable(pid: u32) -> bool {
    // never signal init, kernel, ourselves, or our own process group
    pid > 1 && pid != std::process::id() && unsafe { libc::getpgid(0) } != pid as i32
}

/// Signals the process group led by `pid` (agent sessions are started with `setsid`). Falls back to
/// the single process when `pid` does not lead a group. Returns false when nothing was signalled.
pub fn signal_group(pid: u32, sig: i32) -> bool {
    if !pid_is_signalable(pid) {
        return false;
    }
    unsafe {
        if libc::kill(-(pid as i32), sig) == 0 {
            return true;
        }
        process_start_time(pid).is_some() && libc::kill(pid as i32, sig) == 0
    }
}

/// Zombies (dead, unreaped) count as gone. Stray group members of a dead leader are not tracked.
pub fn everything_gone(pid: u32, start: Option<u64>) -> bool {
    !is_alive(pid, start)
}

/// SIGTERM the group, wait up to `term_grace`, then SIGKILL, wait up to `kill_grace`.
/// Returns true when the process (and its group) is gone.
pub fn terminate(pid: u32, start: Option<u64>, term_grace: Duration, kill_grace: Duration) -> bool {
    if everything_gone(pid, start) {
        return true;
    }
    signal_group(pid, libc::SIGTERM);
    if wait_gone(pid, start, term_grace) {
        return true;
    }
    signal_group(pid, libc::SIGKILL);
    wait_gone(pid, start, kill_grace)
}

pub fn wait_gone(pid: u32, start: Option<u64>, grace: Duration) -> bool {
    let end = Instant::now() + grace;
    loop {
        if everything_gone(pid, start) {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
