//! Live working-tree status: polled while the window is focused, refreshed on focus gain and
//! after editor saves / run applies. Results carry a token so only the latest one counts.

use crate::app::{App, Msg, TargetSel};
use crate::jobs::JobKind;
use crate::worktree::{self, Summary};
use std::time::{Duration, Instant};

pub const POLL: Duration = Duration::from_secs(3);
pub const MAX_INTERVAL: Duration = Duration::from_secs(15);
const SLOW: Duration = Duration::from_millis(500);
const VERY_SLOW: Duration = Duration::from_secs(2);
/// More than this many consecutive very slow polls (huge repo) switch auto-refresh off.
const MAX_SLOW_STREAK: u32 = 3;
/// A poll that never reports back must not block the next one forever.
const STUCK: Duration = Duration::from_secs(60);

pub struct WtState {
    pub summary: Option<Summary>,
    pub token: u64,
    pub last: Instant,
    pub was_focused: bool,
    /// `cmp_gen` of a refresh that must keep the visible file/scroll (0 = none).
    pub quiet_gen: u64,
    pub inflight_since: Option<Instant>,
    pub interval: Duration,
    pub slow_streak: u32,
    /// Auto-refresh off (repo too slow); only manual refresh / save / apply update the status.
    pub paused: bool,
}

impl Default for WtState {
    fn default() -> Self {
        WtState { summary: None, token: 0, last: Instant::now(), was_focused: false, quiet_gen: 0, inflight_since: None, interval: POLL, slow_streak: 0, paused: false }
    }
}

impl WtState {
    /// Adapts the poll cadence to how long the last poll took.
    pub fn record_poll(&mut self, took: Duration) {
        if took > VERY_SLOW {
            self.slow_streak += 1;
            if self.slow_streak > MAX_SLOW_STREAK {
                self.paused = true;
            }
        } else {
            self.slow_streak = 0;
        }
        self.interval = if took > SLOW { (self.interval * 2).min(MAX_INTERVAL) } else { (self.interval / 2).max(POLL) };
    }
}

impl App {
    pub fn request_status(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        self.wt.token += 1;
        self.wt.last = Instant::now();
        self.wt.inflight_since = Some(self.wt.last);
        let (token, head, timeout, prev) = (self.wt.token, self.head.clone(), self.git_timeout(), self.wt.summary.clone());
        self.jobs.spawn(JobKind::Status, move |c| {
            let repo = repo.with_timeout(timeout).with_cancel(c.cancel.clone());
            c.finish(Msg::Status { token, res: worktree::summary(&repo, &head, prev.as_ref(), Some(&c.cancel)) });
        });
    }

    /// Zero cost while unfocused: no job, no repaint request.
    pub fn wt_tick(&mut self, ctx: &egui::Context) {
        if self.repo.is_none() || self.smoke.is_some() {
            return;
        }
        if !ctx.input(|i| i.focused) {
            self.wt.was_focused = false;
            return;
        }
        let regained = !self.wt.was_focused;
        self.wt.was_focused = true;
        if self.wt.paused {
            return;
        }
        let busy = self.wt.inflight_since.is_some_and(|t| t.elapsed() < STUCK);
        if !busy && (regained || self.wt.last.elapsed() >= self.wt.interval) {
            self.request_status();
        }
        ctx.request_repaint_after(self.wt.interval.saturating_sub(self.wt.last.elapsed()).max(Duration::from_millis(100)));
    }

    pub fn wt_handle(&mut self, token: u64, res: Result<Summary, String>) {
        if token != self.wt.token {
            return;
        }
        if let Some(t) = self.wt.inflight_since.take() {
            self.wt.record_poll(t.elapsed());
        }
        let Ok(s) = res else { return };
        let changed = self.wt.summary.as_ref().map(|o| o.sig) != Some(s.sig);
        self.wt.summary = Some(s);
        if changed && matches!(self.target, Some(TargetSel::Worktree)) && self.range.is_none() {
            self.wt.quiet_gen = u64::MAX; // consumed by request_diff
            self.request_diff();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents_fake::FakeAgents;
    use crate::app::Loadable;
    use std::path::Path;
    use std::sync::Arc;

    fn git(dir: &Path, args: &[&str]) {
        let o = std::process::Command::new("git").current_dir(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap();
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }

    fn repo(dirty: bool) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        git(p, &["init", "-q"]);
        for f in ["a", "b"] {
            std::fs::write(p.join(f), "1\n2\n3\n").unwrap();
        }
        git(p, &["add", "."]);
        git(p, &["commit", "-q", "-m", "init"]);
        if dirty {
            std::fs::write(p.join("a"), "1\n2\n3\n4\n").unwrap();
            git(p, &["add", "a"]);
            std::fs::write(p.join("b"), "1\n3\n").unwrap();
            std::fs::write(p.join("new.txt"), "n\n").unwrap();
        }
        d
    }

    fn app_for(path: &Path) -> App {
        let (th, _) = crate::theme::ThemeFile::load(Path::new("/nonexistent/theme.toml"));
        App::new(egui::Context::default(), path.to_path_buf(), th, crate::settings::Settings::default(), Vec::new(), String::new(), None, Arc::new(FakeAgents::new()))
    }

    fn pump_until(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
        let t = Instant::now();
        while !done(app) {
            assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn files(app: &App) -> Vec<String> {
        app.diffset.ready().map(|s| s.files.iter().map(|f| f.path.clone()).collect()).unwrap_or_default()
    }

    #[test]
    fn dirty_repo_opens_on_working_tree_with_all_changes_and_counts() {
        let d = repo(true);
        let mut app = app_for(d.path());
        pump_until(&mut app, "worktree diff", |a| a.diffset.ready().is_some() && !a.commits.is_empty());
        assert_eq!(app.target, Some(TargetSel::Worktree), "commit list arrival must not steal the selection");
        assert_eq!(files(&app), ["a", "b", "new.txt"], "staged + unstaged + untracked");
        let s = app.wt.summary.clone().unwrap();
        assert_eq!((s.files, s.add, s.del), (3, 2, 1));
        pump_until(&mut app, "first file", |a| a.prepared.ready().is_some());
        assert_eq!(app.file_sel.as_deref(), Some("a"));
        app.open_file_diff("new.txt");
        pump_until(&mut app, "untracked file diff", |a| a.prepared.ready().is_some_and(|p| p.model.file.path == "new.txt"));
    }

    #[test]
    fn clean_repo_opens_on_head_commit() {
        let d = repo(false);
        let mut app = app_for(d.path());
        pump_until(&mut app, "commits", |a| !a.commits.is_empty() && a.target.is_some());
        assert_eq!(app.target, Some(TargetSel::Commit(app.commits[0].sha.clone())));
        assert!(!app.wt.summary.clone().unwrap().dirty());
        assert_eq!(app.wt.summary.unwrap().text(), "clean");
    }

    #[test]
    fn status_change_refreshes_selected_worktree_without_losing_selection() {
        let d = repo(true);
        let mut app = app_for(d.path());
        pump_until(&mut app, "first file", |a| a.prepared.ready().is_some());
        app.open_file_diff("b");
        pump_until(&mut app, "b diff", |a| a.prepared.ready().is_some_and(|p| p.model.file.path == "b"));
        app.diff_sel = None;
        std::fs::write(d.path().join("late.txt"), "x\ny\n").unwrap();
        app.request_status();
        pump_until(&mut app, "new file listed", |a| files(a).contains(&"late.txt".to_string()));
        assert_eq!(app.file_sel.as_deref(), Some("b"), "selection kept");
        assert!(!matches!(app.diffset, Loadable::Loading));
        pump_until(&mut app, "b still prepared", |a| a.prepared.ready().is_some_and(|p| p.model.file.path == "b"));
        assert_eq!(app.wt.summary.as_ref().unwrap().files, 4);
        // Unchanged status must not trigger another refresh.
        let gen = app.cmp_gen;
        app.request_status();
        pump_until(&mut app, "status answered", |a| a.wt.inflight_since.is_none());
        assert_eq!(app.cmp_gen, gen);
    }

    #[test]
    fn poll_backs_off_when_slow_recovers_and_pauses_on_huge_repos() {
        let ms = Duration::from_millis;
        let mut w = WtState::default();
        w.record_poll(ms(100));
        assert_eq!(w.interval, POLL);
        w.record_poll(ms(800));
        assert_eq!(w.interval, Duration::from_secs(6));
        w.record_poll(ms(800));
        w.record_poll(ms(800));
        assert_eq!(w.interval, MAX_INTERVAL, "capped at 15s");
        assert!(!w.paused);
        w.record_poll(ms(100));
        assert_eq!(w.interval, Duration::from_millis(7500), "recovers gradually");
        let mut w = WtState::default();
        for _ in 0..3 {
            w.record_poll(Duration::from_secs(3));
        }
        assert!(!w.paused, "three slow polls are tolerated");
        w.record_poll(Duration::from_secs(3));
        assert!(w.paused, "the fourth consecutive one pauses auto-refresh");
        let mut w = WtState::default();
        for _ in 0..3 {
            w.record_poll(Duration::from_secs(3));
        }
        w.record_poll(ms(10));
        w.record_poll(Duration::from_secs(3));
        assert!(!w.paused && w.slow_streak == 1, "streak must be consecutive");
    }

    #[test]
    fn stale_status_results_are_ignored() {
        let d = repo(false);
        let mut app = app_for(d.path());
        pump_until(&mut app, "commits", |a| a.target.is_some());
        app.request_status();
        let before = app.wt.summary.clone();
        app.wt_handle(app.wt.token - 1, Ok(Summary { files: 9, ..Default::default() }));
        assert!(app.wt.inflight_since.is_some(), "a stale answer must not end the current poll");
        assert_eq!(app.wt.summary, before);
    }

    fn rss_mb() -> f64 {
        let s = std::fs::read_to_string("/proc/self/statm").unwrap();
        s.split_whitespace().nth(1).unwrap().parse::<f64>().unwrap() * 4096.0 / 1048576.0
    }

    /// Soak: 5,000 selections / refreshes (+ 20 project switches). Run alone:
    /// `cargo test --release -p ct-app soak -- --ignored --test-threads=1 --nocapture`
    #[test]
    #[ignore]
    fn soak_five_thousand_actions_keep_rss_growth_under_20mb() {
        let a = repo(true);
        let b = repo(true);
        for i in 0..25 {
            std::fs::write(a.path().join("a"), format!("v{i}\n")).unwrap();
            git(a.path(), &["commit", "-qam", &format!("c{i}")]);
        }
        let mut app = app_for(a.path());
        pump_until(&mut app, "loaded", |x| x.diffset.ready().is_some() && x.commits.len() >= 20);
        let mut base = 0.0;
        for i in 0..5_200usize {
            if i == 200 {
                base = rss_mb();
            }
            match i % 5 {
                0 => {
                    let sha = app.commits[i / 5 % app.commits.len()].sha.clone();
                    app.select_commit(&sha);
                }
                1 => app.select_worktree(),
                2 => app.request_status(),
                3 => app.request_diff(),
                _ => {
                    app.request_log(false);
                    app.request_files();
                }
            }
            if i % 250 == 249 {
                let p = if (i / 250) % 2 == 0 { b.path() } else { a.path() };
                app.switch_project(p.to_path_buf());
                pump_until(&mut app, "reloaded", |x| x.diffset.ready().is_some() && !x.commits.is_empty());
            }
            app.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
        pump_until(&mut app, "settled", |x| !x.commits.is_empty());
        let grown = rss_mb() - base;
        println!("soak: rss after warm-up {base:.1} MB, end {:.1} MB, growth {grown:.1} MB", base + grown);
        assert!(grown < 20.0, "RSS grew {grown:.1} MB");
    }

    #[test]
    fn unfocused_window_does_no_polling_work_and_asks_for_no_repaints() {
        let d = repo(true);
        let mut app = app_for(d.path());
        pump_until(&mut app, "loaded", |a| a.repo.is_some() && a.wt.summary.is_some());
        let ctx = egui::Context::default();
        let frame = |focused: bool, app: &mut App| {
            let raw = egui::RawInput { focused, ..Default::default() };
            let out = ctx.run(raw, |ctx| {
                app.wt_tick(ctx);
                app.res_tick(ctx);
            });
            out.viewport_output.values().next().map(|v| v.repaint_delay)
        };
        let (token, last) = (app.wt.token, app.res.last);
        for _ in 0..3 {
            frame(false, &mut app); // egui's own first-frame passes
        }
        for _ in 0..5 {
            let delay = frame(false, &mut app);
            assert!(delay.is_none_or(|d| d >= Duration::from_secs(3600)), "unfocused frame requested a repaint: {delay:?}");
        }
        assert_eq!((app.wt.token, app.res.last), (token, last), "no status job, no resource sample while unfocused");
        let delay = frame(true, &mut app).unwrap();
        assert!(delay <= Duration::from_secs(15) && delay >= Duration::from_millis(50));
        assert_eq!(app.wt.token, token + 1, "focus regained triggers one immediate status poll");
        assert!(app.res.last.is_some());
    }
}
