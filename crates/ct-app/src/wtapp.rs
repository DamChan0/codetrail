//! Live working-tree status: polled while the window is focused, refreshed on focus gain and
//! after editor saves / run applies. Results carry a token so only the latest one counts.

use crate::app::{App, Msg, TargetSel};
use crate::jobs::JobKind;
use crate::worktree::{self, Summary};
use std::time::{Duration, Instant};

pub const POLL: Duration = Duration::from_secs(3);

pub struct WtState {
    pub summary: Option<Summary>,
    pub token: u64,
    pub last: Instant,
    pub was_focused: bool,
    /// `cmp_gen` of a refresh that must keep the visible file/scroll (0 = none).
    pub quiet_gen: u64,
}

impl Default for WtState {
    fn default() -> Self {
        WtState { summary: None, token: 0, last: Instant::now(), was_focused: false, quiet_gen: 0 }
    }
}

impl App {
    pub fn request_status(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        self.wt.token += 1;
        self.wt.last = Instant::now();
        let (token, head, timeout) = (self.wt.token, self.head.clone(), self.git_timeout());
        self.jobs.spawn(JobKind::Status, move |c| {
            let repo = repo.with_timeout(timeout).with_cancel(c.cancel.clone());
            c.finish(Msg::Status { token, res: worktree::summary(&repo, &head) });
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
        let due = !self.wt.was_focused || self.wt.last.elapsed() >= POLL;
        self.wt.was_focused = true;
        if due {
            self.request_status();
        }
        ctx.request_repaint_after(POLL.saturating_sub(self.wt.last.elapsed()).max(Duration::from_millis(50)));
    }

    pub fn wt_handle(&mut self, token: u64, res: Result<Summary, String>) {
        if token != self.wt.token {
            return;
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
        app.wt.summary.as_mut().unwrap().files = 99; // marker: same sig, so a fresh result must not refresh
        app.request_status();
        pump_until(&mut app, "status", |a| a.wt.summary.as_ref().unwrap().files == 4);
        assert_eq!(app.cmp_gen, gen);
    }

    #[test]
    fn stale_status_results_are_ignored() {
        let d = repo(false);
        let mut app = app_for(d.path());
        pump_until(&mut app, "commits", |a| a.target.is_some());
        app.request_status();
        let before = app.wt.summary.clone();
        app.wt_handle(app.wt.token - 1, Ok(Summary { files: 9, ..Default::default() }));
        assert_eq!(app.wt.summary, before);
    }
}
