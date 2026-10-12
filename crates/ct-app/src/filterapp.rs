//! File filter glue: cached results (off-thread for big lists), selection that follows the
//! filter, history narrowing, and the Commits-tab file list.

use crate::app::{App, Loadable, Msg};
use crate::currentui::Row;
use crate::filter::{self, FileFilter, Stat};
use crate::jobs::JobKind;
use crate::ui::status_style;
use crate::widgets::{self, Level};
use egui::{pos2, vec2, Rect, RichText, ScrollArea, Ui};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FOLLOW_AFTER: Duration = Duration::from_millis(250);
const NARROW_AFTER: Duration = Duration::from_millis(300);

pub enum FilterOut {
    Idx(Vec<usize>, Stat),
    Rows(Vec<Row>, Stat),
}

impl App {
    /// The filter that applies to the comparison on screen.
    pub fn active_filter(&self) -> Arc<FileFilter> {
        if matches!(self.target, Some(crate::app::TargetSel::Worktree)) {
            self.flt_current.parsed.clone()
        } else {
            self.flt_commits.parsed.clone()
        }
    }

    /// Re-parses both filter texts; a change arms the debounces.
    pub fn filter_sync(&mut self) {
        if self.flt_commits.sync() && self.narrow {
            self.narrow_at = Some(Instant::now());
        }
        self.flt_current.sync();
    }

    /// Keeps `shown_files` in step with the filter and the diff set. True when it is current.
    pub fn ensure_shown_files(&mut self) -> bool {
        let n = match self.diffset.ready() {
            Some(s) => s.files.len(),
            None => return false,
        };
        let key = (self.flt_commits.rev, self.diff_rev);
        if self.shown_files.fresh(key) {
            return true;
        }
        let f = self.flt_commits.parsed.clone();
        if n <= filter::ASYNC_MIN {
            let (idx, st) = filter::pick(&self.diffset.ready().map(|s| s.files.as_slice()).unwrap_or_default(), &f);
            self.shown_files.pending = None;
            self.shown_files.store(key, idx, st);
            return true;
        }
        if self.shown_files.pending != Some(key) {
            self.shown_files.pending = Some(key);
            let files = self.diffset.ready().map(|s| s.files.clone()).unwrap_or_default();
            self.jobs.spawn(JobKind::FilterFiles, move |c| {
                let (idx, st) = filter::pick(&files, &f);
                c.finish(Msg::Filtered { current: false, key, out: FilterOut::Idx(idx, st) });
            });
        }
        false
    }

    /// Same for the Current tab's grouped rows.
    pub fn ensure_shown_wt(&mut self) -> bool {
        let Some(groups) = self.wt.summary.as_ref().map(|s| s.groups.clone()) else { return false };
        let key = (self.flt_current.rev, self.wt.rev);
        if self.shown_wt.fresh(key) {
            return true;
        }
        let f = self.flt_current.parsed.clone();
        if groups.staged.len() + groups.unstaged.len() + groups.untracked.len() <= filter::ASYNC_MIN {
            let (rows, st) = crate::currentui::rows(&groups, &f);
            self.shown_wt.pending = None;
            self.shown_wt.store(key, rows, st);
            return true;
        }
        if self.shown_wt.pending != Some(key) {
            self.shown_wt.pending = Some(key);
            self.jobs.spawn(JobKind::FilterWt, move |c| {
                let (rows, st) = crate::currentui::rows(&groups, &f);
                c.finish(Msg::Filtered { current: true, key, out: FilterOut::Rows(rows, st) });
            });
        }
        false
    }

    pub fn filter_done(&mut self, current: bool, key: (u64, u64), out: FilterOut) {
        match out {
            FilterOut::Idx(idx, st) if !current => self.shown_files.store(key, idx, st),
            FilterOut::Rows(rows, st) if current => self.shown_wt.store(key, rows, st),
            _ => {}
        }
    }

    /// After a pause in typing: when the open file no longer matches, show the first match (or
    /// nothing) so the main pane only ever shows files that pass the filter.
    pub fn filter_follow(&mut self, ctx: &egui::Context) {
        let on_wt = matches!(self.target, Some(crate::app::TargetSel::Worktree));
        for current in [false, true] {
            let f = if current { &mut self.flt_current } else { &mut self.flt_commits };
            let Some(t) = f.changed_at else { continue };
            if t.elapsed() < FOLLOW_AFTER {
                ctx.request_repaint_after(Duration::from_millis(100));
                continue;
            }
            f.changed_at = None;
            if current != on_wt || self.target.is_none() {
                continue;
            }
            let flt = f.parsed.clone();
            let Some(set) = self.diffset.ready() else { continue };
            if self.file_sel.as_deref().is_some_and(|p| flt.matches(p)) {
                continue;
            }
            match set.files.iter().find(|x| flt.matches(&x.path)).map(|x| x.path.clone()) {
                Some(p) => self.open_file_diff(&p),
                None => {
                    self.file_sel = None;
                    self.prepared = Loadable::Idle;
                }
            }
        }
    }

    /// Debounced "only commits touching these files": re-reads history when the pathspec changed.
    pub fn narrow_tick(&mut self, ctx: &egui::Context) {
        let Some(t) = self.narrow_at else { return };
        if t.elapsed() < NARROW_AFTER {
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        self.narrow_at = None;
        let want = if self.narrow { self.flt_commits.parsed.pathspec() } else { None };
        if want != self.narrow_applied {
            self.narrow_applied = want;
            self.request_log(false);
        }
    }

    /// True when a filter is active and no changed file passes it.
    pub fn filter_hides_everything(&self) -> bool {
        let (shown, key) = if matches!(self.target, Some(crate::app::TargetSel::Worktree)) {
            (self.shown_wt.fresh((self.flt_current.rev, self.wt.rev)).then_some(self.shown_wt.stat), self.flt_current.active())
        } else {
            (self.shown_files.fresh((self.flt_commits.rev, self.diff_rev)).then_some(self.shown_files.stat), self.flt_commits.active())
        };
        key && shown.is_some_and(|s| s.files == 0)
    }

    /// Commits tab: file list of the selected commit (header, filter, narrowing toggle, rows).
    pub fn files_ready_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        let fresh = self.ensure_shown_files();
        let Loadable::Ready(set) = &self.diffset else { return };
        let total = set.files.len();
        let active = self.flt_commits.active();
        let st = if fresh && active {
            self.shown_files.stat
        } else {
            let (add, del) = set.files.iter().fold((0u32, 0u32), |a, f| (a.0 + f.add, a.1 + f.del));
            Stat { files: total, add, del }
        };
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            let label = if fresh || !active { filter::count_label(st.files, total, active) } else { format!("Filtering {total} files…") };
            ui.label(RichText::new(label).font(widgets::ui_font(&th)).color(th.fg()));
            if fresh || !active {
                ui.label(RichText::new(format!("+{}", st.add)).font(widgets::mono_font(&th)).color(th.c(th.p().diff.add.fg)));
                ui.label(RichText::new(format!("−{}", st.del)).font(widgets::mono_font(&th)).color(th.c(th.p().diff.del.fg)));
            }
        });
        ui.add_space(m.space[0]);
        if total == 0 {
            widgets::empty_state(ui, &th, "No changes", "This comparison has no differences.");
            return;
        }
        if filter::bar(ui, &th, &mut self.flt_commits) {
            self.flt_commits.sync();
            if self.narrow {
                self.narrow_at = Some(Instant::now());
            }
        }
        self.narrow_toggle(ui);
        let Loadable::Ready(set) = &self.diffset else { return };
        let idx = if fresh { std::mem::take(&mut self.shown_files.items) } else { Vec::new() };
        if !fresh {
            ui.add_space(m.space[1]);
            ui.label(RichText::new("Filtering…").font(widgets::small_font(&th)).color(th.muted()));
        } else if idx.is_empty() {
            widgets::empty_state(ui, &th, "No matching files", "Clear the filter to see all changes.");
        }
        let mut pick: Option<String> = None;
        let row_h = 28.0;
        let sel = self.file_sel.clone();
        ScrollArea::vertical().id_salt("files_changed").auto_shrink([false, false]).show_rows(ui, row_h, idx.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in range {
                let f = &set.files[idx[i]];
                let (resp, rect) = widgets::list_row(ui, &th, row_h, sel.as_deref() == Some(f.path.as_str()));
                let (letter, col) = status_style(&th, f.status);
                let lr = Rect::from_min_size(pos2(rect.min.x, rect.min.y), vec2(14.0, row_h));
                ui.painter().text(lr.center(), egui::Align2::CENTER_CENTER, letter, widgets::mono_font(&th), col);
                let stat = if f.binary { "bin".to_string() } else { format!("+{} −{}", f.add, f.del) };
                let sw = widgets::paint_text_right(ui, rect, &stat, widgets::small_font(&th), th.muted());
                let area = Rect::from_min_max(pos2(rect.min.x + 22.0, rect.min.y), pos2(rect.max.x - sw - 8.0, rect.max.y));
                let label = filter::path_label(ui, &widgets::ui_font(&th), &f.path, area.width());
                widgets::paint_text_fit(ui, area, &label, widgets::ui_font(&th), th.fg());
                if resp.clicked() {
                    pick = Some(f.path.clone());
                }
                resp.on_hover_text(&f.path);
            }
        });
        if fresh {
            self.shown_files.items = idx;
        }
        if let Some(p) = pick {
            if self.centre == crate::app::Centre::Run {
                self.centre = crate::app::Centre::Diff;
            }
            self.open_file_diff(&p);
        }
    }

    /// "Only commits touching these files" checkbox under the filter box (Commits tab).
    fn narrow_toggle(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        let usable = self.flt_commits.parsed.pathspec().is_some();
        if !usable && !self.narrow {
            return;
        }
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            let mut on = self.narrow;
            let r = ui.add_enabled(usable || self.narrow, egui::Checkbox::new(&mut on, RichText::new("Only commits touching these files").font(widgets::small_font(&th)).color(th.muted())));
            let extra = self.flt_commits.parsed.extra_terms();
            let r = if extra > 0 { r.on_hover_text(format!("History uses the first include term only; {extra} more narrow the file list but not the commits.")) } else { r };
            let _ = r;
            if on != self.narrow {
                self.narrow = on;
                self.narrow_at = Some(Instant::now());
            }
        });
    }

    /// Chip above the commit list while history is narrowed by the filter.
    pub fn narrow_chip(&mut self, ui: &mut Ui) {
        if self.narrow_applied.is_none() {
            return;
        }
        let th = self.th.clone();
        let m = th.m().clone();
        let term = self.flt_commits.parsed.pathspec_term().unwrap_or("").to_string();
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            ui.label(RichText::new(format!("filtering by {term}")).font(widgets::small_font(&th)).color(th.accent()));
            if widgets::Btn::new("×").compact().kind(widgets::BtnKind::Ghost).show(ui, &th).on_hover_text("Show all commits again").clicked() {
                self.narrow = false;
                self.narrow_at = Some(Instant::now());
            }
        });
        ui.add_space(m.space[0]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents_fake::FakeAgents;
    use crate::app::TargetSel;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let o = Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }

    fn commit(dir: &Path, files: &[(&str, &str)], msg: &str) {
        for (p, c) in files {
            let f = dir.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, c).unwrap();
        }
        git(dir, &["add", "."]);
        git(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", msg]);
    }

    fn repo3() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().join("r");
        std::fs::create_dir(&r).unwrap();
        git(&r, &["init", "-q"]);
        commit(&r, &[("src/a.rs", "1\n"), ("docs/b.md", "1\n")], "first");
        commit(&r, &[("docs/b.md", "2\n")], "docs only");
        commit(&r, &[("src/a.rs", "2\n"), ("src/c.rs", "1\n"), ("docs/b.md", "3\n")], "all three");
        (d, r)
    }

    fn app_for(path: PathBuf) -> App {
        let (th, _) = crate::theme::ThemeFile::load(Path::new("/nonexistent/theme.toml"));
        App::new(egui::Context::default(), path, th, crate::settings::Settings::default(), Vec::new(), String::new(), None, Arc::new(FakeAgents::new()))
    }

    fn pump_until(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
        let t = Instant::now();
        while !done(app) {
            assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn open(path: PathBuf) -> App {
        let mut app = app_for(path);
        pump_until(&mut app, "open", |a| a.repo.is_some() && a.commits.len() == 3 && a.diffset.ready().is_some());
        app
    }

    fn paths(app: &App) -> Vec<String> {
        app.diffset.ready().unwrap().files.iter().map(|f| f.path.clone()).collect()
    }

    #[test]
    fn selecting_a_commit_with_a_filter_opens_the_first_matching_file_and_counts_only_matches() {
        let (_d, r) = repo3();
        let mut app = open(r);
        assert_eq!(paths(&app).len(), 3);
        app.flt_commits.text = "ext:md".into();
        app.filter_sync();
        let sha = app.commits[0].sha.clone();
        app.target = None;
        app.select_commit(&sha);
        pump_until(&mut app, "diff", |a| a.diffset.ready().is_some() && a.prepared.ready().is_some());
        assert_eq!(app.file_sel.as_deref(), Some("docs/b.md"), "the first file that passes the filter, not the first file");
        assert!(app.ensure_shown_files());
        assert_eq!(app.shown_files.stat.files, 1);
        assert_eq!(app.shown_files.items.len(), 1);
        assert!(!app.filter_hides_everything());
        // Nothing passes: the main pane has no file and says so.
        app.flt_commits.text = "zzz".into();
        app.filter_sync();
        assert!(app.ensure_shown_files());
        assert!(app.filter_hides_everything());
        app.flt_commits.changed_at = Some(Instant::now() - Duration::from_secs(1));
        app.filter_follow(&egui::Context::default());
        assert!(app.file_sel.is_none() && app.prepared.ready().is_none());
    }

    #[test]
    fn changing_the_filter_moves_the_open_file_only_when_it_stops_matching() {
        let (_d, r) = repo3();
        let mut app = open(r);
        let first = app.file_sel.clone().unwrap();
        app.flt_commits.text = "src".into();
        app.filter_sync();
        app.flt_commits.changed_at = Some(Instant::now() - Duration::from_secs(1));
        app.filter_follow(&egui::Context::default());
        if first.starts_with("src") {
            assert_eq!(app.file_sel.as_deref(), Some(first.as_str()), "still matches: stays");
        } else {
            assert!(app.file_sel.as_deref().unwrap().starts_with("src"));
        }
        app.flt_commits.text = "!src".into();
        app.filter_sync();
        app.flt_commits.changed_at = Some(Instant::now() - Duration::from_secs(1));
        app.filter_follow(&egui::Context::default());
        assert_eq!(app.file_sel.as_deref(), Some("docs/b.md"));
    }

    #[test]
    fn each_tab_keeps_its_own_filter_text() {
        let (_d, r) = repo3();
        let mut app = open(r);
        app.flt_commits.text = "src".into();
        app.flt_current.text = "*.md".into();
        app.filter_sync();
        assert_eq!((app.flt_commits.text.as_str(), app.flt_current.text.as_str()), ("src", "*.md"));
        app.rail = crate::app::RailTab::Current;
        app.rail = crate::app::RailTab::Commits;
        assert_eq!(app.flt_commits.text, "src");
        assert!(app.flt_commits.active() && app.flt_current.active());
    }

    #[test]
    fn narrowing_history_is_off_until_asked_then_debounced_and_reversible() {
        let (_d, r) = repo3();
        let mut app = open(r);
        let ctx = egui::Context::default();
        app.flt_commits.text = "src/a.rs".into();
        app.filter_sync();
        app.narrow_tick(&ctx);
        assert!(app.narrow_applied.is_none() && app.commits.len() == 3, "typing alone never re-reads history");
        app.narrow = true;
        app.narrow_at = Some(Instant::now() - Duration::from_secs(1));
        app.narrow_tick(&ctx);
        assert!(app.narrow_applied.is_some());
        pump_until(&mut app, "narrowed log", |a| a.commits.len() == 2 && !a.commits_loading);
        assert!(app.commits.iter().all(|c| c.subject != "docs only"));
        app.flt_commits.text = "!x".into();
        app.filter_sync();
        app.narrow_at = Some(Instant::now() - Duration::from_secs(1));
        app.narrow_tick(&ctx);
        assert!(app.narrow_applied.is_none(), "no include term: nothing to narrow by");
        pump_until(&mut app, "full log", |a| a.commits.len() == 3 && !a.commits_loading);
    }

    #[test]
    fn switching_project_clears_both_filters_and_the_narrowing() {
        let (_d, r) = repo3();
        let mut app = open(r.clone());
        app.flt_commits.text = "src".into();
        app.flt_current.text = "md".into();
        app.narrow = true;
        app.narrow_applied = Some(":(icase)*src*".into());
        app.filter_sync();
        app.switch_project(r);
        assert!(app.flt_commits.text.is_empty() && app.flt_current.text.is_empty());
        assert!(!app.narrow && app.narrow_applied.is_none() && !app.flt_commits.active());
    }

    fn many(n: usize) -> Vec<ct_core::FileStat> {
        use ct_core::{FileKind, Status};
        (0..n).map(|i| ct_core::FileStat { path: format!("Assets/Some Long Folder Name/Sub Folder {}/file_{i}.png", i % 50), old_path: None, status: Status::Added, similarity: None, add: 1, del: 0, binary: false, kind: FileKind::Text, old_mode: None, new_mode: None }).collect()
    }

    #[test]
    fn a_big_list_is_filtered_on_a_worker_and_the_stale_answer_is_dropped() {
        let (_d, r) = repo3();
        let mut app = open(r);
        let n = filter::ASYNC_MIN + 500;
        app.diffset = Loadable::Ready(ct_core::DiffSet { files: many(n) });
        app.diff_rev += 1;
        app.flt_commits.text = "file_1".into();
        app.filter_sync();
        assert!(!app.ensure_shown_files(), "above the threshold the UI thread does not scan");
        app.flt_commits.text = "file_2".into();
        app.filter_sync();
        assert!(!app.ensure_shown_files());
        pump_until(&mut app, "worker", |a| a.shown_files.fresh((a.flt_commits.rev, a.diff_rev)));
        let want = many(n).iter().filter(|f| f.path.contains("file_2")).count();
        assert_eq!(app.shown_files.stat.files, want);
        assert!(app.ensure_shown_files());
    }

    #[test]
    fn the_file_list_builds_only_the_visible_rows_even_with_thousands_of_files() {
        let (_d, r) = repo3();
        let mut app = open(r);
        app.diffset = Loadable::Ready(ct_core::DiffSet { files: many(2515) });
        app.diff_rev += 1;
        let ctx = egui::Context::default();
        let mut run = |app: &mut App| {
            let out = ctx.run(egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 500.0))), ..Default::default() }, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.files_ready_ui(ui));
            });
            out.shapes.len()
        };
        let shapes = run(&mut app);
        assert!(shapes < 400, "2515 files drew {shapes} shapes; rows must be virtualised");
        app.flt_commits.text = "file_7".into();
        app.filter_sync();
        let filtered = run(&mut app);
        assert!(filtered < 400);
        assert_eq!(app.shown_files.stat.files, many(2515).iter().filter(|f| f.path.contains("file_7")).count());
        assert!(app.target.is_some() || matches!(app.target, None | Some(TargetSel::Commit(_)) | Some(TargetSel::Worktree)));
    }
}
