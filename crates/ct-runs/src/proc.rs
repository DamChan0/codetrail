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

fn pid_is_signalable(pid: u32) -> bool {
    // never signal init, kernel, ourselves, or our own process group
    pid > 1 && pid != std::process::id() && unsafe { libc::getpgid(0) } != pid as i32
}

/// `(pid, pgrp, session, starttime, state)` of a live-or-zombie process.
fn stat_fields(pid: u32) -> Option<(i32, i32, u64, char)> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let mut f = rest.split_whitespace();
    let state = f.next()?.chars().next()?;
    let _ppid = f.next()?;
    let pgrp = f.next()?.parse().ok()?;
    let sid = f.next()?.parse().ok()?;
    // after sid: 15 more fields precede starttime (field 22)
    let start = f.nth(15)?.parse().ok()?;
    Some((pgrp, sid, start, state))
}

/// Live processes still belonging to the agent's group/session `leader` (the leader itself may be
/// long dead: grandchildren survive it). Linux never reuses a pid that is still a pgid/sid of a
/// live process, so a missing leader with members means "the old group". When the leader pid now
/// belongs to a *different* process (start time differs) the old group is empty by that rule.
/// Members must have started no earlier than the leader. Excludes zombies and ourselves.
pub fn group_members(leader: u32, start: Option<u64>) -> Vec<u32> {
    if !pid_is_signalable(leader) {
        return vec![];
    }
    if let (Some(now), Some(then)) = (process_start_time(leader), start) {
        if now != then {
            return vec![];
        }
    }
    let me = std::process::id();
    let Ok(rd) = std::fs::read_dir("/proc") else { return vec![] };
    rd.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|p| *p != me)
        .filter(|p| {
            stat_fields(*p).is_some_and(|(pgrp, sid, st, state)| {
                (pgrp == leader as i32 || sid == leader as i32) && state != 'Z' && state != 'X' && start.is_none_or(|s| st >= s)
            })
        })
        .collect()
}

/// The recorded process or anything left in its group/session is still running.
pub fn is_alive(pid: u32, start: Option<u64>) -> bool {
    match (process_start_time(pid), start) {
        (Some(now), Some(then)) if now == then => true,
        (Some(_), None) => true,
        _ => !group_members(pid, start).is_empty(),
    }
}

/// Signals the process group led by `pid` and every remaining session member individually (job
/// control shells may have moved some into other groups). Returns false when nothing was signalled.
pub fn signal_all(pid: u32, start: Option<u64>, sig: i32) -> bool {
    if !pid_is_signalable(pid) {
        return false;
    }
    let mut any = false;
    unsafe {
        if libc::kill(-(pid as i32), sig) == 0 {
            any = true;
        }
        if process_start_time(pid).is_some() && libc::kill(pid as i32, sig) == 0 {
            any = true;
        }
        for m in group_members(pid, start) {
            if libc::kill(m as i32, sig) == 0 {
                any = true;
            }
        }
    }
    any
}

/// Zombies (dead, unreaped) count as gone; so does a leader whose group is empty.
pub fn everything_gone(pid: u32, start: Option<u64>) -> bool {
    !is_alive(pid, start)
}

/// SIGTERM the group, wait up to `term_grace`, then SIGKILL, wait up to `kill_grace`.
/// Returns true when the process and everything in its group is gone.
pub fn terminate(pid: u32, start: Option<u64>, term_grace: Duration, kill_grace: Duration) -> bool {
    if everything_gone(pid, start) {
        return true;
    }
    signal_all(pid, start, libc::SIGTERM);
    if wait_gone(pid, start, term_grace) {
        return true;
    }
    signal_all(pid, start, libc::SIGKILL);
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
