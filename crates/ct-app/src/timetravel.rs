//! "Time travel in place": the main pane shows a file as it was at a commit from the line
//! history, while the left rail (tab, commit, selected file) is left exactly as it was.

use crate::app::*;
use crate::highlight;
use crate::jobs::JobKind;
use crate::widgets::{self, Btn, BtnKind};
use ct_core::{BlameLine, Treeish};
use egui::{Key, Ui};
use std::sync::Arc;

/// Historical blames kept for instant stepping (LRU, oldest dropped first).
pub const CACHE_CAP: usize = 12;

pub type TtResult = Result<Arc<BlameData>, String>;

#[derive(Default)]
pub struct Cache {
    items: Vec<(String, String, TtResult)>,
}

impl Cache {
    pub fn get(&mut self, sha: &str, path: &str) -> Option<TtResult> {
        let i = self.items.iter().position(|(s, p, _)| s == sha && p == path)?;
        let it = self.items.remove(i);
        let r = it.2.clone();
        self.items.push(it);
        Some(r)
    }

    pub fn put(&mut self, sha: String, path: String, r: TtResult) {
        self.items.retain(|(s, p, _)| !(s == &sha && p == &path));
        self.items.push((sha, path, r));
        if self.items.len() > CACHE_CAP {
            let _ = self.items.remove(0);
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

/// Lines of an untracked file shaped like blame output with an empty (all-zero) gutter.
pub fn plain_blame(path: &str, lines: Vec<String>) -> Vec<BlameLine> {
    lines
        .into_iter()
        .enumerate()
        .map(|(i, text)| BlameLine { line_no: i as u32 + 1, sha: "0".repeat(40), author: String::new(), time: 0, summary: String::new(), orig_line: i as u32 + 1, orig_path: path.to_string(), text })
        .collect()
}

pub struct TimeTravel {
    saved_blame: BlameState,
    saved_centre: Centre,
    pub sha: String,
    pub path: String,
    pub subject: String,
    pub time: i64,
    /// Tracked line range at this revision (1-based, inclusive); `None` = lines gone / unknown.
    pub lines: Option<(u32, u32)>,
    /// Position in `history.data`, when the revision came from the list.
    pub idx: Option<usize>,
    pub missing: bool,
    /// Scroll the inspector list to the active entry on the next frame.
    pub list_scroll: bool,
}

fn is_missing(e: &str) -> bool {
    let e = e.to_lowercase();
    e.contains("no such path") || e.contains("does not exist")
}

impl App {
    pub fn tt_active(&self) -> bool {
        self.tt.is_some()
    }

    /// Click on history entry `idx` (or keyboard step).
    pub fn tt_pick(&mut self, idx: usize) {
        let Loadable::Ready(v) = &self.history.data else { return };
        let Some(r) = v.get(idx) else { return };
        let (sha, path, lines, subject, time) = (r.commit.sha.clone(), r.path.clone(), r.lines, r.commit.subject.clone(), r.commit.time);
        self.tt_goto(sha, path, lines, subject, time, Some(idx));
    }

    /// Up/Down in the history: +1 = older entry.
    pub fn tt_step(&mut self, delta: i32) {
        let Some(t) = &self.tt else { return };
        let len = match &self.history.data {
            Loadable::Ready(v) => v.len() as i32,
            _ => return,
        };
        let cur = t.idx.map_or(if delta > 0 { -1 } else { len }, |i| i as i32);
        let next = (cur + delta).clamp(0, len - 1);
        if next != cur {
            self.tt_pick(next as usize);
        }
    }

    pub fn tt_goto(&mut self, sha: String, path: String, lines: Option<(u32, u32)>, subject: String, time: i64, idx: Option<usize>) {
        let (saved_blame, saved_centre) = match self.tt.take() {
            Some(t) => (t.saved_blame, t.saved_centre),
            None => (std::mem::take(&mut self.blame), self.centre),
        };
        self.centre = Centre::Blame;
        self.blame = BlameState { path: path.clone(), rev: Some(sha.clone()), data: Loadable::Loading, selected: None, scroll_to: None, scroll_y: 0.0, apply_y: None };
        self.tt = Some(TimeTravel { saved_blame, saved_centre, sha: sha.clone(), path: path.clone(), subject, time, lines, idx, missing: false, list_scroll: true });
        match self.tt_cache.get(&sha, &path) {
            Some(r) => self.tt_apply(r),
            None => self.tt_fetch(sha, path),
        }
    }

    fn tt_fetch(&mut self, sha: String, path: String) {
        let Some(repo) = self.repo.clone() else { return };
        let timeout = self.git_timeout();
        self.jobs.spawn(JobKind::TimeTravel, move |c| {
            let repo = repo.with_timeout(timeout).with_cancel(c.cancel.clone());
            let res = repo.blame(&Treeish::Commit(sha.clone()), &path, None).map_err(|e| e.to_string()).map(|lines| {
                let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
                let spans = highlight::highlight(highlight::lang_for_path(&path), &texts);
                Arc::new(BlameData { lines, spans })
            });
            c.finish(Msg::TtBlame { sha, path, res });
        });
    }

    pub fn tt_loaded(&mut self, sha: String, path: String, res: TtResult) {
        self.tt_cache.put(sha.clone(), path.clone(), res.clone());
        if self.tt.as_ref().is_some_and(|t| t.sha == sha && t.path == path) {
            self.tt_apply(res);
        }
    }

    fn tt_apply(&mut self, res: TtResult) {
        let Some(t) = &mut self.tt else { return };
        match res {
            Ok(d) => {
                t.missing = false;
                let n = d.lines.len();
                self.blame.selected = t.lines.filter(|_| n > 0).map(|(s, e)| (((s as usize).saturating_sub(1)).min(n - 1), ((e as usize).saturating_sub(1)).min(n - 1)));
                self.blame.scroll_to = t.lines.map(|(s, _)| s as usize);
                self.blame.data = Loadable::Ready(d);
            }
            Err(e) if is_missing(&e) => {
                t.missing = true;
                self.blame.data = Loadable::Failed(e);
            }
            Err(e) => {
                t.missing = false;
                self.blame.data = Loadable::Failed(e);
            }
        }
    }

    /// Back to the view that was open before time travel started (mode, file, scroll).
    pub fn tt_back(&mut self) {
        let Some(t) = self.tt.take() else { return };
        self.blame = t.saved_blame;
        self.blame.apply_y = Some(self.blame.scroll_y);
        self.centre = t.saved_centre;
    }

    /// Leave time travel because the user changed file / mode: same restore, nothing else.
    pub fn tt_discard(&mut self) {
        self.tt_back();
    }

    /// The old navigating behaviour, behind the banner's "Open commit".
    pub fn tt_open_commit(&mut self) {
        let Some(sha) = self.tt.as_ref().map(|t| t.sha.clone()) else { return };
        self.tt_back();
        self.jump_to_commit(&sha);
    }

    /// Blame gutter click while time travelling: move to that line's originating commit, in place.
    pub fn tt_from_blame_line(&mut self, i: usize) {
        let Loadable::Ready(d) = &self.blame.data else { return };
        let Some(l) = d.lines.get(i) else { return };
        if l.sha.bytes().all(|b| b == b'0') {
            return;
        }
        let (sha, path, line, subject, time) = (l.sha.clone(), l.orig_path.clone(), l.orig_line, l.summary.clone(), l.time);
        let idx = match &self.history.data {
            Loadable::Ready(v) => v.iter().position(|r| r.commit.sha == sha),
            _ => None,
        };
        self.tt_goto(sha, path, Some((line, line)), subject, time, idx);
    }

    /// Up/Down step through the history, Esc goes back. Ignored while a text field has focus.
    pub fn tt_keys(&mut self, ctx: &egui::Context) {
        if self.tt.is_none() || ctx.wants_keyboard_input() {
            return;
        }
        let (down, up, esc) = ctx.input_mut(|i| (i.consume_key(egui::Modifiers::NONE, Key::ArrowDown), i.consume_key(egui::Modifiers::NONE, Key::ArrowUp), i.consume_key(egui::Modifiers::NONE, Key::Escape)));
        if esc {
            self.tt_back();
        } else if down {
            self.tt_step(1);
        } else if up {
            self.tt_step(-1);
        }
    }

    /// Banner + missing-file state above the historical blame.
    pub fn tt_banner(&mut self, ui: &mut Ui) {
        let Some(t) = &self.tt else { return };
        let th = self.th.clone();
        let m = th.m().clone();
        let text = format!("Viewing {} @ {} {} · {}", t.path, crate::timefmt::short(&t.sha), t.subject, crate::timefmt::abs_date(t.time));
        let hint = if t.lines.is_none() { "  (the selected lines were removed in this commit)" } else { "" };
        let mut action = 0;
        egui::Frame::new().fill(crate::theme::mix(th.bg(), th.accent(), 0.14)).stroke(egui::Stroke::new(1.0, crate::theme::mix(th.bg(), th.accent(), 0.5))).inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = m.space[1];
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if Btn::new("Back to current").kind(BtnKind::Primary).compact().show(ui, &th).clicked() {
                        action = 1;
                    }
                    if Btn::new("Open commit").compact().show(ui, &th).clicked() {
                        action = 2;
                    }
                    let w = ui.available_width().max(60.0);
                    let g = widgets::fit(ui, &format!("{text}{hint}"), widgets::ui_font(&th), th.fg(), w);
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| ui.label(g));
                });
            });
        });
        match action {
            1 => self.tt_back(),
            2 => self.tt_open_commit(),
            _ => {}
        }
    }

    /// Main pane when the file does not exist at the shown revision.
    pub fn tt_missing_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let Some(t) = &self.tt else { return };
        widgets::empty_state(ui, &th, &format!("File did not exist at {}", crate::timefmt::short(&t.sha)), &format!("{} is not present in that revision.", t.path));
        let idx = t.idx;
        let len = match &self.history.data {
            Loadable::Ready(v) => v.len(),
            _ => 0,
        };
        let mut go: Option<usize> = None;
        ui.vertical_centered(|ui| {
            if let Some(i) = idx {
                if i + 1 < len && Btn::new("Nearest older entry").show(ui, &th).clicked() {
                    go = Some(i + 1);
                }
                if i > 0 && Btn::new("Nearest newer entry").show(ui, &th).clicked() {
                    go = Some(i - 1);
                }
            }
        });
        if let Some(i) = go {
            self.tt_pick(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents_fake::FakeAgents;
    use crate::app::RailTab;
    use crate::settings::ViewMode;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn git(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git").current_dir(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap();
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
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

    fn write(d: &Path, f: &str, body: &str) {
        std::fs::write(d.join(f), body).unwrap();
    }

    struct Fx {
        d: tempfile::TempDir,
        c1: String,
        c2: String,
        c4: String,
    }

    /// f.txt: created (c1), line 3 edited (c2), renamed to g.txt (c3), two lines inserted above
    /// and the same line edited again (c4: it now sits on line 5). other.txt exists from c1.
    fn fixture() -> Fx {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        git(p, &["init", "-q"]);
        write(p, "f.txt", "a\nb\nc\nd\ne\n");
        write(p, "other.txt", "o1\no2\n");
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "create"]);
        let c1 = git(p, &["rev-parse", "HEAD"]);
        write(p, "f.txt", "a\nb\nC-one\nd\ne\n");
        git(p, &["commit", "-qam", "edit line three"]);
        let c2 = git(p, &["rev-parse", "HEAD"]);
        git(p, &["mv", "f.txt", "g.txt"]);
        git(p, &["commit", "-qm", "rename"]);
        write(p, "g.txt", "x\ny\na\nb\nC-two\nd\ne\n");
        git(p, &["commit", "-qam", "insert and edit again"]);
        let c4 = git(p, &["rev-parse", "HEAD"]);
        Fx { d, c1, c2, c4 }
    }

    fn opened(fx: &Fx) -> App {
        let mut app = app_for(fx.d.path());
        pump_until(&mut app, "head diff", |a| a.diffset.ready().is_some() && a.target.is_some() && !a.commits.is_empty());
        pump_until(&mut app, "file diff", |a| a.prepared.ready().is_some());
        app
    }

    fn with_history(fx: &Fx) -> App {
        let mut app = opened(fx);
        app.request_history("g.txt", 5, 5);
        pump_until(&mut app, "history", |a| a.history.data.ready().is_some());
        app
    }

    fn tt_ready(app: &mut App) {
        pump_until(app, "historical blame", |a| a.blame.data.ready().is_some());
    }

    #[test]
    fn history_entries_carry_the_path_and_line_range_at_that_revision() {
        let fx = fixture();
        let app = with_history(&fx);
        let v = app.history.data.ready().unwrap();
        let got: Vec<_> = v.iter().map(|r| (r.commit.sha.clone(), r.path.clone(), r.lines)).collect();
        assert_eq!(got, [(fx.c4.clone(), "g.txt".into(), Some((5, 5))), (fx.c2.clone(), "f.txt".into(), Some((3, 3))), (fx.c1.clone(), "f.txt".into(), Some((3, 3)))]);
    }

    #[test]
    fn history_click_keeps_the_rail_and_selected_file_and_sets_pane_state() {
        let fx = fixture();
        let mut app = with_history(&fx);
        let (rail, target, file, prev) = (app.rail, app.target.clone(), app.file_sel.clone(), app.centre);
        app.tt_pick(1);
        tt_ready(&mut app);
        assert_eq!((app.rail, app.target.clone(), app.file_sel.clone()), (rail, target, file), "rail selection untouched");
        assert_eq!(app.centre, Centre::Blame);
        assert_eq!((app.blame.path.as_str(), app.blame.rev.as_deref()), ("f.txt", Some(fx.c2.as_str())), "path as it was named then");
        assert_eq!(app.blame.selected, Some((2, 2)), "line mapped to its number at that revision");
        assert_eq!(app.blame.scroll_to, Some(3));
        let t = app.tt.as_ref().unwrap();
        assert_eq!((t.sha.as_str(), t.subject.as_str(), t.idx), (fx.c2.as_str(), "edit line three", Some(1)));
        let text = &app.blame.data.ready().unwrap().lines[2].text;
        assert_eq!(text, "C-one", "content is the file at that revision");
        app.tt_back();
        assert_eq!((app.centre, app.tt.is_none()), (prev, true), "Back restores the previous mode");
        assert_eq!(app.blame.path, "", "previous blame state restored");
    }

    #[test]
    fn stepping_uses_the_cache_and_does_not_start_new_work() {
        let fx = fixture();
        let mut app = with_history(&fx);
        app.tt_pick(0);
        tt_ready(&mut app);
        app.tt_step(1);
        tt_ready(&mut app);
        app.tt_step(1);
        tt_ready(&mut app);
        assert_eq!(app.tt.as_ref().unwrap().idx, Some(2));
        assert_eq!(app.tt_cache.len(), 3);
        app.tt_step(-1);
        assert_eq!(app.tt.as_ref().unwrap().idx, Some(1));
        assert!(app.blame.data.ready().is_some(), "cached entry is shown in the same frame");
        app.tt_step(-1);
        app.tt_step(-1);
        assert_eq!(app.tt.as_ref().unwrap().idx, Some(0), "clamped at the newest entry");
    }

    #[test]
    fn blame_gutter_click_moves_in_place_and_open_commit_navigates() {
        let fx = fixture();
        let mut app = with_history(&fx);
        let (rail, file) = (app.rail, app.file_sel.clone());
        app.tt_pick(1);
        tt_ready(&mut app);
        // line index 0 ("a") originates in c1
        app.tt_from_blame_line(0);
        tt_ready(&mut app);
        assert_eq!(app.tt.as_ref().unwrap().sha, fx.c1);
        assert_eq!((app.rail, app.file_sel.clone()), (rail, file));
        app.tt_open_commit();
        assert!(app.tt.is_none());
        assert_eq!(app.rail, RailTab::Commits);
        assert_eq!(app.target, Some(TargetSel::Commit(fx.c1.clone())));
    }

    #[test]
    fn missing_file_state_and_nearest_entry() {
        let fx = fixture();
        let mut app = with_history(&fx);
        app.tt_goto(fx.c1.clone(), "nope.txt".into(), None, "create".into(), 0, Some(2));
        pump_until(&mut app, "missing", |a| a.tt.as_ref().is_some_and(|t| t.missing));
        assert!(matches!(app.blame.data, Loadable::Failed(_)));
        app.tt_pick(1);
        tt_ready(&mut app);
        assert!(!app.tt.as_ref().unwrap().missing);
    }

    #[test]
    fn lru_cache_is_bounded() {
        let mut c = Cache::default();
        for i in 0..(CACHE_CAP + 5) {
            c.put(format!("{i}"), "p".into(), Err("x".into()));
        }
        assert_eq!(c.len(), CACHE_CAP);
        assert!(c.get("0", "p").is_none());
        assert!(c.get(&format!("{}", CACHE_CAP + 4), "p").is_some());
    }

    // ---------------------------------------------------------------- sticky mode

    fn settle(app: &mut App) {
        pump_until(app, "settled", |a| !a.busy());
    }

    #[test]
    fn mode_is_sticky_across_file_commit_and_tab_changes() {
        let fx = fixture();
        let mut app = opened(&fx);
        app.set_view_pref(ViewMode::Blame);
        settle(&mut app);
        assert_eq!((app.centre, app.blame.path.as_str()), (Centre::Blame, "g.txt"));
        app.open_file_diff("g.txt");
        settle(&mut app);
        // another commit
        app.select_commit(&fx.c1);
        settle(&mut app);
        let first = app.file_sel.clone().unwrap();
        assert_eq!(app.centre, Centre::Blame);
        assert_eq!(app.blame.path, first, "blame follows the selected file");
        app.open_file_diff("other.txt");
        settle(&mut app);
        assert_eq!((app.centre, app.blame.path.as_str()), (Centre::Blame, "other.txt"));
        app.rail = RailTab::Files;
        app.rail = RailTab::Commits;
        assert_eq!(app.settings.view_mode, ViewMode::Blame);
        app.set_view_pref(ViewMode::Diff);
        assert_eq!(app.centre, Centre::Diff);
        app.open_file_diff("f.txt");
        assert_eq!(app.centre, Centre::Diff, "never auto-switches");
    }

    #[test]
    fn edit_falls_back_on_read_only_targets_without_changing_the_preference() {
        let fx = fixture();
        let mut app = opened(&fx);
        app.set_view_pref(ViewMode::Edit);
        settle(&mut app);
        assert_eq!(app.centre, Centre::Editor, "HEAD commit is editable");
        app.select_commit(&fx.c1);
        settle(&mut app);
        assert_eq!(app.centre, Centre::Blame, "historical revision: read-only fallback");
        assert_eq!(app.settings.view_mode, ViewMode::Edit, "stored preference untouched");
        assert!(app.view_note.is_some());
        app.select_commit(&fx.c4);
        settle(&mut app);
        assert_eq!(app.centre, Centre::Editor, "returns to Edit when editable again");
        assert!(app.view_note.is_none());
    }

    #[test]
    fn deleted_files_show_the_diff_and_keep_the_preference() {
        let fx = fixture();
        let p = fx.d.path();
        git(p, &["rm", "-q", "other.txt"]);
        git(p, &["commit", "-qm", "drop other"]);
        let mut app = app_for(p);
        pump_until(&mut app, "open", |a| a.diffset.ready().is_some() && a.target.is_some());
        app.set_view_pref(ViewMode::Edit);
        settle(&mut app);
        assert_eq!(app.file_sel.as_deref(), Some("other.txt"));
        assert_eq!(app.centre, Centre::Diff);
        assert_eq!(app.settings.view_mode, ViewMode::Edit);
        assert_eq!(app.view_note, Some("deleted file"));
    }

    #[test]
    fn blame_on_an_untracked_file_shows_plain_text_with_an_empty_gutter() {
        let fx = fixture();
        write(fx.d.path(), "fresh.txt", "one\ntwo\n");
        let mut app = app_for(fx.d.path());
        pump_until(&mut app, "open", |a| a.diffset.ready().is_some() && a.target == Some(TargetSel::Worktree));
        app.set_view_pref(ViewMode::Blame);
        app.open_file_diff("fresh.txt");
        pump_until(&mut app, "plain blame", |a| a.blame.data.ready().is_some() && a.blame.path == "fresh.txt");
        let d = app.blame.data.ready().unwrap();
        assert_eq!(d.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["one", "two"]);
        assert!(d.lines.iter().all(|l| l.sha.bytes().all(|b| b == b'0')));
        assert_eq!(app.centre, Centre::Blame);
    }

    #[test]
    fn unsaved_edits_are_not_replaced_by_selecting_another_file() {
        let fx = fixture();
        let mut app = opened(&fx);
        app.set_view_pref(ViewMode::Edit);
        pump_until(&mut app, "editor", |a| a.editor.as_ref().is_some_and(|e| e.load.ready().is_some()));
        let open = app.editor.as_ref().unwrap().path.clone();
        app.editor.as_mut().unwrap().text.push_str("unsaved");
        let other = if open == "other.txt" { "g.txt" } else { "other.txt" };
        app.open_file_diff(other);
        assert_eq!(app.editor.as_ref().unwrap().path, open);
        assert!(app.editor.as_ref().unwrap().dirty());
        assert_eq!(app.centre, Centre::Editor);
    }
}
