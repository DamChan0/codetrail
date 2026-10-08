//! Per-run resource accounting and limits: process-group totals read from `/proc`, sampled by one
//! thread that exists only while runs are active.

use std::sync::LazyLock;

use crate::proc;

/// Process-group totals of one run at the last sample.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Resource {
    pub rss_kb: u64,
    /// CPU use over the last sampling interval, in percent of one core (can exceed 100).
    pub cpu_pct: f32,
    pub procs: u32,
}

/// Per-run limits. `max_runtime_min == 0` and `max_rss_mb == 0` disable that limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_rss_mb: u64,
    pub max_cpu_seconds_total: Option<u64>,
    pub max_runtime_min: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_rss_mb: 3072, max_cpu_seconds_total: None, max_runtime_min: 120 }
    }
}

/// Default global budget over all running runs' RSS.
pub const DEFAULT_MAX_TOTAL_RSS_MB: u64 = 6144;
/// Consecutive over-limit samples before a run is stopped (a short spike is not a leak).
pub const RSS_STRIKES: u32 = 3;

static PAGE_KB: LazyLock<u64> = LazyLock::new(|| (unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64) / 1024);
static CLK_TCK: LazyLock<f64> = LazyLock::new(|| (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1)) as f64);

/// `(resident pages, utime + stime ticks)` of one process.
fn one(pid: u32) -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 2..];
    let mut f = rest.split_whitespace();
    // state ppid pgrp sid tty tpgid flags minflt cminflt majflt cmajflt utime stime
    let ut: u64 = f.nth(11)?.parse().ok()?;
    let st: u64 = f.next()?.parse().ok()?;
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let rss: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some((rss, ut + st))
}

/// Totals of everything in the group/session led by `leader`: (rss KiB, cpu ticks, process count).
pub fn group_usage(leader: u32, start: Option<u64>) -> (u64, u64, u32) {
    let (mut rss, mut ticks, mut n) = (0, 0, 0);
    for p in proc::group_members(leader, start) {
        if let Some((r, t)) = one(p) {
            rss += r * *PAGE_KB;
            ticks += t;
            n += 1;
        }
    }
    (rss, ticks, n)
}

pub fn ticks_to_secs(t: u64) -> f64 {
    t as f64 / *CLK_TCK
}

/// Sampling state of one run.
#[derive(Default)]
pub struct Track {
    pub strikes: u32,
    prev_ticks: Option<u64>,
    /// CPU seconds consumed over the run (monotone even when members come and go).
    pub cpu_secs: f64,
}

impl Track {
    /// Folds one sample in and returns the `Resource` plus a limit-breach reason, if any.
    pub fn sample(&mut self, l: &Limits, usage: (u64, u64, u32), dt_secs: f64, runtime_ms: u64) -> (Resource, Option<String>) {
        let (rss_kb, ticks, procs) = usage;
        // members leaving shrink the sum; only growth counts as consumption
        let delta = self.prev_ticks.map_or(0, |p| ticks.saturating_sub(p));
        self.prev_ticks = Some(ticks);
        let d_secs = ticks_to_secs(delta);
        self.cpu_secs += d_secs;
        let cpu_pct = if dt_secs > 0.0 { (d_secs / dt_secs * 100.0) as f32 } else { 0.0 };
        let res = Resource { rss_kb, cpu_pct, procs };

        if l.max_rss_mb > 0 && rss_kb > l.max_rss_mb * 1024 {
            self.strikes += 1;
        } else {
            self.strikes = 0;
        }
        let reason = if self.strikes >= RSS_STRIKES {
            Some(format!("resource limit: memory {} MB exceeded the {} MB limit for {} consecutive samples", rss_kb / 1024, l.max_rss_mb, RSS_STRIKES))
        } else if l.max_cpu_seconds_total.is_some_and(|m| self.cpu_secs >= m as f64) {
            Some(format!("resource limit: used {:.0} CPU seconds (limit {})", self.cpu_secs, l.max_cpu_seconds_total.unwrap_or(0)))
        } else if l.max_runtime_min > 0 && runtime_ms >= l.max_runtime_min * 60_000 {
            Some(format!("resource limit: runtime exceeded {} min", l.max_runtime_min))
        } else {
            None
        };
        (res, reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_needs_consecutive_strikes_and_a_dip_resets() {
        let l = Limits { max_rss_mb: 100, ..Limits::default() };
        let mut t = Track::default();
        let hi = (200 * 1024, 0, 1);
        let lo = (50 * 1024, 0, 1);
        assert!(t.sample(&l, hi, 2.0, 0).1.is_none());
        assert!(t.sample(&l, hi, 2.0, 0).1.is_none());
        assert!(t.sample(&l, lo, 2.0, 0).1.is_none());
        assert!(t.sample(&l, hi, 2.0, 0).1.is_none());
        assert!(t.sample(&l, hi, 2.0, 0).1.is_none());
        let r = t.sample(&l, hi, 2.0, 0).1.unwrap();
        assert!(r.starts_with("resource limit: memory"), "{r}");
    }

    #[test]
    fn runtime_and_cpu_budget_trigger() {
        let l = Limits { max_runtime_min: 2, max_cpu_seconds_total: Some(10), ..Limits::default() };
        let mut t = Track::default();
        assert!(t.sample(&l, (1, 0, 1), 2.0, 119_999).1.is_none());
        assert!(t.sample(&l, (1, 0, 1), 2.0, 120_000).1.unwrap().contains("runtime exceeded 2 min"));
        let mut t = Track::default();
        let tck = *CLK_TCK as u64;
        t.sample(&l, (1, 0, 1), 2.0, 0);
        assert!(t.sample(&l, (1, 5 * tck, 1), 2.0, 0).1.is_none());
        // group shrinks (a member exited): no negative consumption, nothing lost
        assert!(t.sample(&l, (1, 2 * tck, 1), 2.0, 0).1.is_none());
        assert!(t.sample(&l, (1, 8 * tck, 1), 2.0, 0).1.unwrap().contains("CPU seconds"));
    }

    #[test]
    fn group_usage_counts_our_own_group_members() {
        // a child in its own group: leader's rss is visible and non-zero
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new("sleep").arg("5").process_group(0).spawn().unwrap();
        let pid = c.id();
        let (rss, _t, n) = group_usage(pid, proc::process_start_time(pid));
        let _ = c.kill();
        let _ = c.wait();
        assert_eq!(n, 1);
        assert!(rss > 0);
    }
}
