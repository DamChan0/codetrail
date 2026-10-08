//! Project switcher: header chip + recent-projects popover, in-app folder browser, and the
//! full per-project state reset on switch. Filesystem listing runs on a worker (PLAN §9.2).

use crate::agentapp::now_ms;
use crate::app::{App, Loadable, Msg};
use crate::jobs::JobKind;
use crate::projects::*;
use crate::widgets::{self, Btn, BtnKind, Icon, Level};
use egui::{pos2, vec2, Align, Layout, Rect, RichText, ScrollArea, Ui};
use std::path::{Path, PathBuf};

pub struct Browser {
    pub cwd: PathBuf,
    pub input: String,
    pub listing: Loadable<Listing>,
    /// Inline validation text for the typed path; the previous listing stays visible.
    pub error: Option<String>,
    pub token: u64,
    pub focus: bool,
}

#[derive(Default)]
pub struct ProjectState {
    pub popover: bool,
    pub chip_rect: Option<Rect>,
    pub browser: Option<Browser>,
    /// Why the last switch was refused (shown in the browser).
    pub open_error: Option<String>,
}

impl App {
    pub fn pj_toggle_popover(&mut self) {
        self.pj.popover = !self.pj.popover;
    }

    pub fn pj_open_browser(&mut self) {
        self.pj.popover = false;
        let start = browser_start(&self.settings.recent, &home_dir());
        self.pj.browser = Some(Browser { cwd: start.clone(), input: abbrev_home(&start, &home_dir()), listing: Loadable::Loading, error: None, token: 0, focus: true });
        self.pj.open_error = None;
        self.pj_browse(start);
    }

    /// Lists `path` on a worker; stale answers (older token) are ignored.
    pub fn pj_browse(&mut self, path: PathBuf) {
        let Some(b) = &mut self.pj.browser else { return };
        b.token += 1;
        let token = b.token;
        if b.listing.ready().is_none() {
            b.listing = Loadable::Loading;
        }
        self.jobs.spawn(JobKind::Browse, move |c| {
            c.finish(Msg::Browse { token, res: list_dir(&path) });
        });
    }

    pub fn pj_handle(&mut self, token: u64, res: Result<Listing, String>) {
        let Some(b) = &mut self.pj.browser else { return };
        if token != b.token {
            return;
        }
        match res {
            Ok(l) => {
                b.cwd = l.dir.clone();
                b.input = abbrev_home(&l.dir, &home_dir());
                b.error = None;
                b.listing = Loadable::Ready(l);
            }
            Err(e) => {
                b.error = Some(e);
                if matches!(b.listing, Loadable::Loading) {
                    b.listing = Loadable::Idle;
                }
            }
        }
    }

    /// Opens `path` as the current project. Only git repositories are accepted.
    pub fn pj_open(&mut self, path: PathBuf) -> Result<(), String> {
        match check_dir(&path)? {
            DirKind::Plain => return Err(format!("{} is not a git repository (no .git inside).", path.display())),
            DirKind::Git => {}
        }
        self.pj.browser = None;
        self.pj.popover = false;
        self.switch_project(path);
        Ok(())
    }

    /// Replaces every piece of per-project state by building a fresh `App` for `path`. The old
    /// `App`'s `Jobs` is dropped: its cancel flags are raised and its result channel is closed, so
    /// no stale Log/Diff/Search/Blame/Files message can reach the new project. Agent state
    /// (accounts, models, runs service and its bridge) is process-wide and carried over, so runs
    /// of the previous repo keep running.
    pub fn switch_project(&mut self, path: PathBuf) {
        let mut fresh = App::new(self.ctx.clone(), path, self.th.file.clone(), self.settings.clone(), std::mem::take(&mut self.banners), std::mem::take(&mut self.fonts_note), self.smoke.take(), self.ag.svc.clone());
        std::mem::swap(&mut fresh.ag, &mut self.ag);
        fresh.rail = self.rail;
        fresh.insp_open = self.insp_open;
        fresh.settings_open = self.settings_open;
        fresh.ag.new_run = None;
        fresh.ag.confirm = None;
        fresh.ag.runs.selected = None;
        *self = fresh;
    }

    pub fn pj_record_open(&mut self, root: &Path) {
        touch(&mut self.settings.recent, root, now_ms());
        self.settings_dirty_at = Some(std::time::Instant::now());
    }

    pub fn pj_forget(&mut self, path: &str) {
        remove(&mut self.settings.recent, path);
        self.settings_dirty_at = Some(std::time::Instant::now());
    }

    pub fn current_root(&self) -> Option<PathBuf> {
        self.repo.as_ref().map(|r| r.root.clone()).or_else(|| Some(self.repo_arg.clone()).filter(|p| !p.as_os_str().is_empty()))
    }

    // ------------------------------------------------------------ header

    pub fn project_header(&mut self, ui: &mut Ui, narrow: bool) {
        let th = self.th.clone();
        let home = home_dir();
        let root = self.current_root();
        let name = root.as_deref().map(crate::app::short_repo_name).unwrap_or_else(|| "No project".to_string());
        let full = root.as_deref().map(|r| abbrev_home(r, &home));
        let avail = ui.available_width();
        let r = widgets::chip(ui, &th, if narrow { "" } else { "Project" }, &name, true, (avail * 0.4).clamp(120.0, 260.0));
        let r = match &root {
            Some(p) => r.on_hover_text(format!("{}\nClick to switch project (Ctrl+O)", p.display())),
            None => r.on_hover_text("Open a project (Ctrl+O)"),
        };
        if r.clicked() {
            self.pj_toggle_popover();
        }
        self.pj.chip_rect = Some(r.rect);
        if let Some(full) = full {
            let room = ui.available_width() - if narrow { 360.0 } else { 520.0 };
            if room >= 70.0 {
                let chars = (room / 7.0) as usize;
                let g = widgets::fit(ui, &shorten_path(&full, chars), widgets::small_font(&th), th.muted(), room);
                ui.label(g).on_hover_text(root.as_deref().map(|p| p.display().to_string()).unwrap_or_default());
            }
        }
    }

    // ------------------------------------------------------------ popover

    pub fn project_popover(&mut self, ctx: &egui::Context) {
        if !self.pj.popover {
            return;
        }
        let th = self.th.clone();
        let home = home_dir();
        let anchor = self.pj.chip_rect.map(|r| r.left_bottom() + vec2(0.0, 6.0)).unwrap_or(pos2(12.0, 56.0));
        let cur = self.current_root().map(|p| p.to_string_lossy().into_owned());
        let width = 380.0_f32.min(ctx.screen_rect().width() - 24.0);
        let (mut open_path, mut forget, mut browse) = (None::<String>, None::<String>, false);
        let area = egui::Area::new(egui::Id::new("project_pop")).order(egui::Order::Foreground).fixed_pos(anchor).show(ctx, |ui| {
            egui::Frame::new().fill(th.raised()).stroke(egui::Stroke::new(1.0, th.border())).corner_radius(th.radius()).inner_margin(egui::Margin::same(12)).shadow(egui::Shadow { offset: [0, 4], blur: 16, spread: 0, color: egui::Color32::from_black_alpha(90) }).show(ui, |ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                widgets::heading(ui, &th, "Projects");
                if self.settings.recent.is_empty() {
                    ui.label(RichText::new("No recent projects yet.").color(th.muted()));
                }
                ScrollArea::vertical().max_height(300.0).auto_shrink([true, true]).show(ui, |ui| {
                    for rp in &self.settings.recent {
                        let (resp, rect) = widgets::list_row(ui, &th, 44.0, cur.as_deref() == Some(rp.path.as_str()));
                        let x = Rect::from_center_size(pos2(rect.max.x - 12.0, rect.center().y), vec2(24.0, 24.0));
                        let x_resp = ui.interact(x, ui.id().with(("forget", &rp.path)), egui::Sense::click());
                        let top = Rect::from_min_max(rect.min + vec2(0.0, 5.0), pos2(rect.max.x - 32.0, rect.min.y + 24.0));
                        let bot = Rect::from_min_max(pos2(rect.min.x, rect.min.y + 24.0), pos2(rect.max.x - 32.0, rect.max.y - 4.0));
                        let p = Path::new(&rp.path);
                        widgets::paint_text_fit(ui, top, &crate::app::short_repo_name(p), widgets::ui_font(&th), th.fg());
                        widgets::paint_text_fit(ui, bot, &abbrev_home(p, &home), widgets::small_font(&th), th.muted());
                        let col = if x_resp.hovered() { th.fg() } else { th.muted() };
                        widgets::paint_icon(ui.painter(), x.center(), Icon::Close, col);
                        if x_resp.on_hover_text("Remove from recent projects").clicked() {
                            forget = Some(rp.path.clone());
                        } else if resp.on_hover_text(&rp.path).clicked() {
                            open_path = Some(rp.path.clone());
                        }
                    }
                });
                ui.separator();
                if Btn::new("Open folder…").icon(Icon::File).show(ui, &th).clicked() {
                    browse = true;
                }
            });
        });
        if let Some(p) = forget {
            self.pj_forget(&p);
        }
        if browse {
            self.pj_open_browser();
        } else if let Some(p) = open_path {
            if let Err(e) = self.pj_open(PathBuf::from(&p)) {
                self.flash(e);
                self.pj_forget(&p);
            }
        } else if ctx.input(|i| i.key_pressed(egui::Key::Escape)) || (ctx.input(|i| i.pointer.any_click()) && !area.response.contains_pointer() && !self.pj.chip_rect.is_some_and(|r| ctx.input(|i| i.pointer.interact_pos().is_some_and(|p| r.contains(p))))) {
            self.pj.popover = false;
        }
    }

    // ------------------------------------------------------------ folder browser

    pub fn folder_browser(&mut self, ctx: &egui::Context) {
        let Some(mut b) = self.pj.browser.take() else { return };
        let th = self.th.clone();
        let home = home_dir();
        let (mut go, mut open, mut cancel) = (None::<PathBuf>, None::<PathBuf>, false);
        let err = self.pj.open_error.clone();
        crate::agentui::dialog(&th, ctx, "Open folder", 560.0, |ui| {
            // Breadcrumbs: every ancestor is a button.
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
                for (i, (label, p)) in breadcrumbs(&b.cwd).into_iter().enumerate() {
                    if i > 0 {
                        ui.label(RichText::new("›").color(th.muted()));
                    }
                    if Btn::new(&label).compact().show(ui, &th).clicked() {
                        go = Some(p);
                    }
                }
            });
            ui.horizontal(|ui| {
                let up = b.cwd.parent().map(Path::to_path_buf);
                if Btn::new("Up").compact().enabled(up.is_some()).show(ui, &th).clicked() {
                    go = up;
                }
                if Btn::new("Home").compact().show(ui, &th).clicked() {
                    go = Some(home.clone());
                }
                let w = (ui.available_width() - 8.0).max(120.0);
                let te = egui::TextEdit::singleline(&mut b.input).hint_text("Type a path, e.g. ~/projects/app").desired_width(w).margin(vec2(8.0, 6.0)).font(widgets::mono_font(&th));
                let r = ui.add(te);
                if b.focus {
                    r.request_focus();
                    b.focus = false;
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    go = Some(expand_tilde(&b.input, &home));
                }
            });
            // Validation line.
            let (text, col) = match (&b.error, &b.listing) {
                (Some(e), _) => (e.clone(), widgets::level_color(&th, Level::Error)),
                (None, Loadable::Ready(l)) if l.kind == DirKind::Git => ("Git repository: ready to open.".to_string(), th.p().diff.add.fg.0),
                (None, Loadable::Ready(_)) => ("Not a git repository. Open a folder that contains .git.".to_string(), th.warn()),
                (None, Loadable::Loading) => ("Reading folder…".to_string(), th.muted()),
                _ => (String::new(), th.muted()),
            };
            ui.label(RichText::new(if text.is_empty() { " " } else { text.as_str() }).font(widgets::small_font(&th)).color(col));
            if let Some(e) = &err {
                widgets::banner(ui, &th, Level::Error, e, None, false);
            }
            // Listing.
            egui::Frame::new().fill(th.sunken()).stroke(egui::Stroke::new(1.0, th.border())).corner_radius(th.radius()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ScrollArea::vertical().id_salt("folder_list").max_height(240.0).min_scrolled_height(240.0).auto_shrink([false, false]).show(ui, |ui| match &b.listing {
                    Loadable::Ready(l) if l.entries.is_empty() => widgets::empty_state(ui, &th, "No sub-folders", "Use Open to pick this folder, or go up."),
                    Loadable::Ready(l) => {
                        for e in &l.entries {
                            let (resp, rect) = widgets::list_row(ui, &th, 28.0, false);
                            let mut right = rect.max.x;
                            if e.is_git {
                                let g = widgets::galley(ui, "git", widgets::small_font(&th), th.accent());
                                right -= g.size().x + 14.0;
                                widgets::paint_badge(ui.painter(), &th, pos2(right, rect.center().y), "git", th.accent());
                            }
                            widgets::paint_text_fit(ui, Rect::from_min_max(rect.min, pos2(right - 8.0, rect.max.y)), &e.name, widgets::ui_font(&th), th.fg());
                            if resp.double_clicked() {
                                go = Some(l.dir.join(&e.name));
                            } else if resp.clicked() {
                                b.input = abbrev_home(&l.dir.join(&e.name), &home);
                                go = Some(l.dir.join(&e.name));
                            }
                        }
                    }
                    Loadable::Loading => widgets::skeleton(ui, &th, 6, 28.0),
                    _ => {}
                });
            });
            let can_open = b.error.is_none() && matches!(&b.listing, Loadable::Ready(l) if l.kind == DirKind::Git);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if Btn::new("Open").kind(BtnKind::Primary).enabled(can_open).show(ui, &th).clicked() {
                    open = Some(b.cwd.clone());
                }
                if Btn::new("Cancel").show(ui, &th).clicked() {
                    cancel = true;
                }
            });
        });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        if cancel {
            return;
        }
        self.pj.browser = Some(b);
        if let Some(p) = open {
            if let Err(e) = self.pj_open(p) {
                self.pj.open_error = Some(e);
            }
        } else if let Some(p) = go {
            self.pj.open_error = None;
            self.pj_browse(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents_fake::FakeAgents;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn git(dir: &Path, args: &[&str]) {
        let o = std::process::Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }

    fn repo(parent: &Path, name: &str, file: &str) -> PathBuf {
        let d = parent.join(name);
        std::fs::create_dir(&d).unwrap();
        git(&d, &["init", "-q"]);
        std::fs::write(d.join(file), "x\n").unwrap();
        git(&d, &["add", "."]);
        git(&d, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", &format!("init {name}")]);
        d
    }

    fn app_for(path: PathBuf) -> App {
        let ctx = egui::Context::default();
        let (th, _) = crate::theme::ThemeFile::load(Path::new("/nonexistent/theme.toml"));
        App::new(ctx, path, th, crate::settings::Settings::default(), Vec::new(), String::new(), None, Arc::new(FakeAgents::new()))
    }

    fn pump_until(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
        let t = Instant::now();
        while !done(app) {
            assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn switching_projects_resets_per_project_state_and_keeps_agent_state() {
        let d = tempfile::tempdir().unwrap();
        let a = repo(d.path(), "alpha", "a.txt");
        let b = repo(d.path(), "beta", "b.txt");
        let mut app = app_for(a.clone());
        pump_until(&mut app, "alpha commits", |x| x.repo.is_some() && !x.commits.is_empty());
        assert_eq!(app.settings.recent[0].path, app.repo.as_ref().unwrap().root.to_string_lossy());
        app.search.query = "needle".into();
        app.commit_filter = "init".into();
        app.ag.runs.selected = Some("r9".into());
        let svc = app.ag.svc.clone();

        app.switch_project(b.clone());
        assert!(app.repo.is_none() && app.commits.is_empty() && app.target.is_none() && app.selection.is_none());
        assert!(app.search.query.is_empty() && app.commit_filter.is_empty() && app.ag.runs.selected.is_none());
        assert!(Arc::ptr_eq(&svc, &app.ag.svc), "agent service is process-wide");
        pump_until(&mut app, "beta commits", |x| x.repo.is_some() && !x.commits.is_empty());
        assert_eq!(app.repo.as_ref().unwrap().root, b.canonicalize().unwrap());
        assert!(app.commits.iter().all(|c| c.subject.contains("beta")), "no commit of alpha leaks in");
        assert_eq!(app.settings.recent.len(), 2, "both projects remembered, newest first");
        assert!(app.settings.recent[0].path.ends_with("beta"));
    }

    #[test]
    fn only_git_repositories_can_be_opened() {
        let d = tempfile::tempdir().unwrap();
        let a = repo(d.path(), "alpha", "a.txt");
        let plain = d.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        let mut app = app_for(a);
        let e = app.pj_open(plain.clone()).unwrap_err();
        assert!(e.contains("not a git repository"), "{e}");
        assert!(app.pj_open(d.path().join("missing")).unwrap_err().contains("does not exist"));
        assert!(app.repo_arg.ends_with("alpha"), "a refused open keeps the current project");
    }

    #[test]
    fn stale_browse_answers_are_ignored() {
        let d = tempfile::tempdir().unwrap();
        let a = repo(d.path(), "alpha", "a.txt");
        let mut app = app_for(a);
        app.pj_open_browser();
        let first = app.pj.browser.as_ref().unwrap().token;
        app.pj_browse(d.path().to_path_buf());
        let stale = list_dir(Path::new("/")).unwrap();
        app.pj_handle(first, Ok(stale));
        let b = app.pj.browser.as_ref().unwrap();
        assert_ne!(b.cwd, Path::new("/"), "older token must not move the browser");
    }
}
