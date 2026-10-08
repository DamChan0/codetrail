//! Resource indicator: this process plus every descendant (git, agents), read from /proc.
//! Sampled on a worker every 2 s, only while the window is focused.

use crate::app::{App, Msg};
use crate::jobs::JobKind;
use std::time::{Duration, Instant};

pub const EVERY: Duration = Duration::from_secs(2);
/// Budgets the indicator colours against.
pub const APP_BUDGET_MB: u64 = 150;
pub const RUNS_BUDGET_MB: u64 = 6144;
const PAGE_KB: u64 = 4;
const CLK_TCK: f32 = 100.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample {
    pub app_rss_kb: u64,
    pub child_rss_kb: u64,
    pub children: u32,
    pub cpu_pct: f32,
    /// utime+stime ticks of app + descendants, for the next delta.
    pub ticks: u64,
}

struct Proc {
    ppid: u32,
    rss_pages: u64,
    ticks: u64,
}

/// `/proc/<pid>/stat` after the `)` that ends the comm field.
fn parse_stat(s: &str) -> Option<(u32, Proc)> {
    let pid: u32 = s.split_whitespace().next()?.parse().ok()?;
    let rest = &s[s.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // f[0]=state f[1]=ppid ... f[11]=utime f[12]=stime ... f[21]=rss
    Some((pid, Proc { ppid: f.get(1)?.parse().ok()?, ticks: f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?, rss_pages: f.get(21)?.parse().ok()? }))
}

fn vm_rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find_map(|l| l.strip_prefix("VmRSS:")).and_then(|v| v.split_whitespace().next()?.parse().ok())).unwrap_or(0)
}

pub fn sample(prev: Option<(u64, Instant)>) -> Sample {
    let me = std::process::id();
    let mut all = std::collections::HashMap::new();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            if !e.file_name().as_encoded_bytes().iter().all(u8::is_ascii_digit) {
                continue;
            }
            if let Some((pid, p)) = std::fs::read_to_string(e.path().join("stat")).ok().and_then(|s| parse_stat(&s)) {
                all.insert(pid, p);
            }
        }
    }
    let mut set = std::collections::HashSet::from([me]);
    loop {
        let before = set.len();
        for (pid, p) in &all {
            if set.contains(&p.ppid) {
                set.insert(*pid);
            }
        }
        if set.len() == before {
            break;
        }
    }
    let (mut child_pages, mut ticks, mut children) = (0, 0, 0);
    for pid in &set {
        if let Some(p) = all.get(pid) {
            ticks += p.ticks;
            if *pid != me {
                child_pages += p.rss_pages;
                children += 1;
            }
        }
    }
    let cpu_pct = prev.map_or(0.0, |(t0, at)| {
        let dt = at.elapsed().as_secs_f32().max(0.001);
        (ticks.saturating_sub(t0) as f32 / CLK_TCK / dt * 100.0).max(0.0)
    });
    Sample { app_rss_kb: vm_rss_kb(), child_rss_kb: child_pages * PAGE_KB, children, cpu_pct, ticks }
}

#[derive(Default)]
pub struct ResState {
    pub sample: Option<Sample>,
    pub runs: Vec<(String, crate::agents::Resource)>,
    pub last: Option<Instant>,
    pub prev: Option<(u64, Instant)>,
    pub was_focused: bool,
}

/// 0 = fine, 1 = amber (>70% of budget), 2 = red (>100%).
pub fn level(used_mb: u64, budget_mb: u64) -> u8 {
    if used_mb > budget_mb {
        2
    } else if used_mb * 10 > budget_mb * 7 {
        1
    } else {
        0
    }
}

pub fn fmt_mb(kb: u64) -> String {
    if kb >= 1024 * 1024 {
        format!("{:.1} GB", kb as f64 / 1024.0 / 1024.0)
    } else {
        format!("{} MB", kb / 1024)
    }
}

impl App {
    /// Every 2 s while focused; nothing at all while unfocused.
    pub fn res_tick(&mut self, ctx: &egui::Context) {
        if self.smoke.is_some() {
            return;
        }
        if !ctx.input(|i| i.focused) {
            self.res.was_focused = false;
            return;
        }
        let due = !self.res.was_focused || self.res.last.is_none_or(|t| t.elapsed() >= EVERY);
        self.res.was_focused = true;
        if due {
            self.res.last = Some(Instant::now());
            let prev = self.res.prev;
            let svc = if self.ag.runs.active() > 0 { self.ag.runs_svc.ready().cloned() } else { None };
            self.jobs.spawn(JobKind::Resources, move |c| {
                let s = sample(prev);
                let runs = svc.map(|r| r.resources()).unwrap_or_default();
                c.finish(Msg::Resources { sample: s, runs });
            });
        }
        crate::widgets::repaint_if_focused(ctx, EVERY.saturating_sub(self.res.last.map_or(Duration::ZERO, |t| t.elapsed())).max(Duration::from_millis(100)));
    }

    pub fn res_handle(&mut self, sample: Sample, runs: Vec<(String, crate::agents::Resource)>) {
        self.res.prev = Some((sample.ticks, Instant::now()));
        self.res.sample = Some(sample);
        self.res.runs = runs;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_line_with_spaces_and_parens_in_comm_parses() {
        let line = "4242 (my (weird) app) S 17 4242 4242 0 -1 4194304 100 0 0 0 30 12 0 0 20 0 1 0 5000 1000000 777 18446744073709551615 0";
        let (pid, p) = parse_stat(line).unwrap();
        assert_eq!((pid, p.ppid, p.ticks, p.rss_pages), (4242, 17, 42, 777));
    }

    #[test]
    fn levels_cross_at_70_and_100_percent() {
        assert_eq!([level(100, 150), level(106, 150), level(150, 150), level(151, 150)], [0, 1, 1, 2]);
    }

    #[test]
    fn sees_own_rss_and_a_spawned_child() {
        let mut ch = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let s = sample(None);
        let _ = ch.kill();
        let _ = ch.wait();
        assert!(s.app_rss_kb > 1000);
        assert!(s.children >= 1);
        assert_eq!(fmt_mb(143 * 1024), "143 MB");
        assert_eq!(fmt_mb(3 * 1024 * 1024 / 2), "1.5 GB");
    }
}

impl App {
    /// "RAM 143 MB · CPU 0%", plus children and the runs subtotal when there are any.
    pub fn resource_label(&self, ui: &mut egui::Ui, th: &crate::theme::Theme, font: egui::FontId) {
        let Some(s) = self.res.sample else { return };
        let narrow = ui.ctx().screen_rect().width() < 1000.0;
        let tint = |lvl: u8| match lvl {
            2 => crate::widgets::level_color(th, crate::widgets::Level::Error),
            1 => th.warn(),
            _ => th.muted(),
        };
        let runs: Vec<&crate::agents::Resource> = self.res.runs.iter().map(|(_, r)| r).collect();
        if !runs.is_empty() {
            let kb: u64 = runs.iter().map(|r| r.rss_kb).sum();
            let cpu: f32 = runs.iter().map(|r| r.cpu_pct).sum();
            let procs: u32 = runs.iter().map(|r| r.procs).sum();
            let txt = if narrow { format!("runs {}", fmt_mb(kb)) } else { format!("runs {} · {} · {} proc", runs.len(), fmt_mb(kb), procs) };
            ui.label(egui::RichText::new(txt).font(font.clone()).color(tint(level(kb / 1024, RUNS_BUDGET_MB)))).on_hover_text(format!("Agent runs: {} · CPU {:.0}% · budget {} MB total", fmt_mb(kb), cpu, RUNS_BUDGET_MB));
        }
        let mut txt = if narrow { format!("{}", fmt_mb(s.app_rss_kb)) } else { format!("RAM {} · CPU {:.0}%", fmt_mb(s.app_rss_kb), s.cpu_pct) };
        if !narrow && s.children > 0 {
            txt.push_str(&format!(" · +{} child", fmt_mb(s.child_rss_kb)));
        }
        ui.label(egui::RichText::new(txt).font(font).color(tint(level(s.app_rss_kb / 1024, APP_BUDGET_MB))))
            .on_hover_text(format!("codetrail: {} (budget {} MB). Children: {} process(es), {}. CPU is app + children.", fmt_mb(s.app_rss_kb), APP_BUDGET_MB, s.children, fmt_mb(s.child_rss_kb)));
    }
}
