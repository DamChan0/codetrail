//! Frame layout, top bar, left rail, status bar, popovers, palette, settings, shortcuts.

use crate::app::*;
use crate::widgets::{self, Btn, BtnKind, Icon, Level};
use crate::theme;
use egui::{pos2, vec2, Align, Color32, FontId, Key, Layout, Modifiers, Rect, RichText, ScrollArea, Ui};
use crate::jobs::JobKind;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    PickFile,
    SearchPanel,
    Palette,
    BaseMenu,
    ToggleEdit,
    Save,
    CopyRef,
    CopyPathLine,
    CopyAbsPath,
    AskAi,
    ToggleSplit,
    Settings,
    GotoLine,
    ToggleInspector,
    Refresh,
    OpenProject,
    PullRequests,
    ShowCommits,
    ShowChanges,
    ShowFiles,
    ShowRuns,
}

pub const COMMANDS: [(Cmd, &str, &str); 21] = [
    (Cmd::OpenProject, "Switch project", "Ctrl+O"),
    (Cmd::PickFile, "Go to file", "Ctrl+P"),
    (Cmd::SearchPanel, "Search in files", "Ctrl+Shift+F"),
    (Cmd::ShowCommits, "Show commits", "Ctrl+1"),
    (Cmd::ShowChanges, "Show local changes", "Ctrl+2"),
    (Cmd::ShowFiles, "Show file list", "Ctrl+4"),
    (Cmd::PullRequests, "Pull requests", "Ctrl+Shift+P"),
    (Cmd::ShowRuns, "Show agent runs", "Ctrl+6"),
    (Cmd::BaseMenu, "Change base", "Ctrl+B"),
    (Cmd::ToggleEdit, "Toggle editor", "Ctrl+E"),
    (Cmd::Save, "Save file", "Ctrl+S"),
    (Cmd::CopyRef, "Copy code reference", "Ctrl+Shift+C"),
    (Cmd::CopyPathLine, "Copy absolute path:line", "Ctrl+Alt+C"),
    (Cmd::CopyAbsPath, "Copy absolute file path", "Ctrl+Alt+Shift+C"),
    (Cmd::AskAi, "Ask AI about selection", "Ctrl+L"),
    (Cmd::ToggleSplit, "Toggle unified / side-by-side", ""),
    (Cmd::GotoLine, "Go to line", "Ctrl+G"),
    (Cmd::ToggleInspector, "Show or hide inspector", ""),
    (Cmd::Refresh, "Reload commits and diff", ""),
    (Cmd::Settings, "Settings", "Ctrl+,"),
    (Cmd::Palette, "Command palette", "Ctrl+K"),
];

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(spec) = self.remote.take() {
            self.rs.backend().close(&spec);
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        self.ag_close_requested(ctx);
        self.smoke_prepare();
        self.shortcuts(ctx);
        // egui's built-in Ctrl+Plus / Ctrl+Minus / Ctrl+0 change the zoom: keep the stored value in step.
        let z = ctx.zoom_factor();
        if (z - self.settings.zoom).abs() > 0.001 {
            self.settings.zoom = crate::settings::clamp_zoom(z);
            if (z - self.settings.zoom).abs() > 0.001 {
                ctx.set_zoom_factor(self.settings.zoom);
            }
            self.settings_dirty_at = Some(Instant::now());
        }
        self.debounce(ctx);
        self.wt_tick(ctx);
        self.prs_focus_tick(ctx);
        self.res_tick(ctx);
        self.tt_keys(ctx);
        if self.editor.is_some() && self.centre == Centre::Editor {
            self.check_editor_disk();
            widgets::repaint_if_focused(ctx, Duration::from_millis(1000));
        }
        if let Some(t) = self.settings_dirty_at {
            if t.elapsed() > Duration::from_millis(400) {
                self.settings_dirty_at = None;
                self.persist_settings();
            } else {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }
        if let Some((_, t)) = &self.status_msg {
            if t.elapsed() > Duration::from_secs(4) {
                self.status_msg = None;
            } else {
                ctx.request_repaint_after(Duration::from_millis(500));
            }
        }
        let th = self.th.clone();

        egui::TopBottomPanel::bottom("status").exact_height(th.m().control_height).frame(egui::Frame::new().fill(th.raised()).stroke(egui::Stroke::new(1.0, th.border())).inner_margin(egui::Margin::symmetric(12, 0))).show(ctx, |ui| self.status_bar(ui));

        egui::TopBottomPanel::top("top").exact_height(th.m().control_height + 16.0).frame(egui::Frame::new().fill(th.raised()).stroke(egui::Stroke::new(1.0, th.border())).inner_margin(egui::Margin::symmetric(12, 8))).show(ctx, |ui| self.top_bar(ui));

        if !self.banners.is_empty() {
            egui::TopBottomPanel::top("banners").frame(egui::Frame::new().fill(th.bg())).show(ctx, |ui| {
                let mut dismiss = None;
                for (i, (lvl, text)) in self.banners.iter().enumerate() {
                    if widgets::banner(ui, &th, *lvl, text, None, true) == widgets::BannerAction::Dismiss {
                        dismiss = Some(i);
                    }
                }
                if let Some(i) = dismiss {
                    self.banners.remove(i);
                }
            });
        }

        self.update_notice(ctx, &th);
        if !self.repo_arg.as_os_str().is_empty() || self.settings_open {
            egui::SidePanel::left("activity").exact_width(68.0).resizable(false).frame(egui::Frame::new().fill(th.raised()).stroke(egui::Stroke::new(1.0, th.border()))).show(ctx, |ui| self.activity_bar(ui));
        }

        if self.settings_open {
            if Self::settings_wide(ctx) {
                self.settings_nav_panel(ctx);
            }
            egui::CentralPanel::default().frame(egui::Frame::new().fill(th.sunken())).show(ctx, |ui| self.settings_page(ui));
        } else {
            self.main_panes(ctx, &th);
        }

        self.base_popover(ctx);
        self.palette_ui(ctx);
        self.ask_preview_window(ctx);
        self.agent_windows(ctx);
        self.project_popover(ctx);
        self.folder_browser(ctx);
        self.smoke_tick(ctx);
    }
}

impl App {
    /// The update notice under the top bar: available / downloading / installed / failed.
    fn update_notice(&mut self, ctx: &egui::Context, th: &crate::theme::Theme) {
        use crate::updater::UpdState;
        if self.upd_hidden {
            return;
        }
        let can_self = crate::updater::running_self_updatable();
        let (lvl, text, action) = match &self.upd {
            UpdState::Available(r) if can_self => (Level::Info, format!("CodeTrail {} is available (you have {}).", r.version, crate::updater::CURRENT), Some("Update now")),
            UpdState::Available(r) => (Level::Info, format!("CodeTrail {} is available. This copy is a system install: get the new package from the release page.", r.version), Some("Open release page")),
            UpdState::Installing(r) => (Level::Info, format!("Downloading CodeTrail {}…", r.version), None),
            UpdState::Installed(v) => (Level::Info, format!("CodeTrail {v} is installed. Restart to use it."), Some("Restart now")),
            UpdState::Failed(e) => (Level::Error, format!("Update failed: {e}"), Some("Retry check")),
            _ => return,
        };
        let mut act = widgets::BannerAction::None;
        egui::TopBottomPanel::top("update_notice").frame(egui::Frame::new().fill(th.bg()).inner_margin(egui::Margin::symmetric(8, 6))).show(ctx, |ui| {
            act = widgets::banner(ui, th, lvl, &text, action, true);
        });
        match act {
            widgets::BannerAction::Dismiss => self.upd_hidden = true,
            widgets::BannerAction::Action => match self.upd.clone() {
                UpdState::Available(r) if !can_self => crate::updater::open_url(&r.page),
                UpdState::Available(_) => self.upd_install(),
                UpdState::Installed(_) => self.upd_restart(ctx),
                UpdState::Failed(_) => self.upd_check(),
                _ => {}
            },
            widgets::BannerAction::None => {}
        }
    }

    fn main_panes(&mut self, ctx: &egui::Context, th: &crate::theme::Theme) {
        use crate::panels;
        let win_w = ctx.screen_rect().width();
        let docked = win_w >= panels::DOCK_MIN_WINDOW;
        // Entering a narrow window hides the inspector (it re-opens as a drawer via its button).
        let was_docked_id = egui::Id::new("was_docked");
        let was_docked = ctx.data(|d| d.get_temp::<bool>(was_docked_id));
        if was_docked != Some(docked) {
            ctx.data_mut(|d| d.insert_temp(was_docked_id, docked));
            if !docked && was_docked.is_none_or(|w| w) {
                self.insp_open = false;
            }
        }
        // The saved widths are only replaced while the user drags a splitter (pointer down).
        let dragging = ctx.input(|i| i.pointer.primary_down());
        let lay = panels::panel_layout(win_w, self.settings.rail_width, self.settings.inspector_width, self.insp_open);
        self.conn_banner(ctx, th);
        if self.rail == RailTab::Prs {
            egui::CentralPanel::default().frame(egui::Frame::new().fill(th.sunken())).show(ctx, |ui| self.prs_page(ui));
            return;
        }
        let (rw, rlo, rhi) = lay.rail;
        let r = egui::SidePanel::left("rail").resizable(true).default_width(rw).width_range(rlo..=rhi).frame(egui::Frame::new().fill(th.bg()).stroke(egui::Stroke::new(1.0, th.border()))).show(ctx, |ui| self.rail_ui(ui));
        let rail_seen = ctx.data(|d| d.get_temp::<f32>(egui::Id::new("rail_seen")));
        let now = r.response.rect.width();
        ctx.data_mut(|d| d.insert_temp(egui::Id::new("rail_seen"), now));
        if dragging && rail_seen.is_some_and(|p| (p - now).abs() > 0.5) {
            self.settings.rail_width = now;
            self.settings_dirty_at = Some(Instant::now());
        }

        if let Some((iw, lo, hi)) = lay.insp {
            let r = egui::SidePanel::right("inspector").resizable(true).default_width(iw).width_range(lo..=hi).frame(egui::Frame::new().fill(th.bg()).stroke(egui::Stroke::new(1.0, th.border()))).show(ctx, |ui| self.inspector_ui(ui));
            let seen = ctx.data(|d| d.get_temp::<f32>(egui::Id::new("insp_seen")));
            let now = r.response.rect.width();
            ctx.data_mut(|d| d.insert_temp(egui::Id::new("insp_seen"), now));
            if dragging && seen.is_some_and(|p| (p - now).abs() > 0.5) {
                self.settings.inspector_width = now;
                self.settings_dirty_at = Some(Instant::now());
            }
        }

        let area = ctx.available_rect();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(th.sunken())).show(ctx, |ui| self.centre_ui(ui));

        if let Some(dw) = lay.drawer {
            let h = area.height();
            let pos = egui::pos2(area.right() - dw, area.top());
            egui::Area::new(egui::Id::new("inspector_drawer")).order(egui::Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
                egui::Frame::new().fill(th.bg()).stroke(egui::Stroke::new(1.0, th.border())).show(ui, |ui| {
                    ui.set_width(dw - 2.0);
                    ui.set_height(h - 2.0);
                    ui.set_max_size(vec2(dw - 2.0, h - 2.0));
                    self.inspector_ui(ui);
                });
            });
        }
    }

    pub fn persist_settings(&mut self) {
        if self.smoke.is_some() {
            return;
        }
        if let Err(e) = self.settings.save(&crate::settings::Settings::path()) {
            self.flash(format!("Could not save settings: {e}"));
        }
        if let Err(e) = self.th.file.save(&theme::theme_path()) {
            self.flash(format!("Could not save theme: {e}"));
        }
    }

    fn debounce(&mut self, ctx: &egui::Context) {
        self.filter_sync();
        self.filter_follow(ctx);
        self.narrow_tick(ctx);
        if let Some(t) = self.commit_filter_at {
            if t.elapsed() > Duration::from_millis(300) {
                self.commit_filter_at = None;
                let now = (self.commit_filter.clone(), self.commit_filter_mode);
                if now != self.commit_filter_applied {
                    self.commit_filter_applied = now;
                    self.request_log(false);
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
        if let Some(t) = self.search.edited_at {
            if t.elapsed() > Duration::from_millis(250) {
                self.search.edited_at = None;
                self.start_search();
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
    }

    // ------------------------------------------------------------ shortcuts

    pub fn run_cmd(&mut self, c: Cmd) {
        match c {
            Cmd::PickFile => {
                self.palette = PaletteState { open: Some(PaletteMode::Files), focus: true, ..Default::default() };
            }
            Cmd::Palette => {
                self.palette = PaletteState { open: Some(PaletteMode::Commands), focus: true, ..Default::default() };
            }
            Cmd::SearchPanel => self.set_rail(RailTab::Search),
            Cmd::PullRequests => {
                if self.remote.is_none() {
                    self.set_rail(RailTab::Prs);
                }
            }
            Cmd::ShowCommits => self.set_rail(RailTab::Commits),
            Cmd::ShowChanges => self.set_rail(RailTab::Current),
            Cmd::ShowFiles => self.set_rail(RailTab::Files),
            Cmd::ShowRuns => {
                if self.remote.is_none() {
                    self.set_rail(RailTab::Runs);
                }
            }
            Cmd::BaseMenu => self.base_pop.open = !self.base_pop.open,
            Cmd::ToggleEdit => match self.settings.view_mode {
                crate::settings::ViewMode::Edit => self.set_view_pref(crate::settings::ViewMode::Diff),
                _ => {
                    if self.file_sel.is_some() || self.editor.is_some() {
                        self.set_view_pref(crate::settings::ViewMode::Edit);
                    } else {
                        self.flash("Pick a file first (Ctrl+P).");
                    }
                }
            },
            Cmd::Save => {
                if self.centre == Centre::Editor {
                    self.save_editor(false);
                }
            }
            Cmd::CopyRef => self.copy_ref(),
            Cmd::CopyPathLine => self.copy_path_line(),
            Cmd::CopyAbsPath => self.copy_file_path(true),
            Cmd::AskAi => self.start_ask(),
            Cmd::ToggleSplit => {
                self.settings.split_view = !self.settings.split_view;
                self.settings_dirty_at = Some(Instant::now());
            }
            Cmd::Settings => self.settings_open = !self.settings_open,
            Cmd::OpenProject => self.pj_toggle_popover(),
            Cmd::GotoLine => {
                if let Some(e) = &mut self.editor {
                    if self.centre == Centre::Editor {
                        e.goto_open = true;
                    }
                }
            }
            Cmd::ToggleInspector => self.insp_open = !self.insp_open,
            Cmd::Refresh => {
                self.request_status();
                self.request_log(false);
                self.request_diff();
                self.request_files();
            }
        }
    }

    pub(crate) fn shortcuts(&mut self, ctx: &egui::Context) {
        let mut cmds: Vec<Cmd> = Vec::new();
        // egui-winit turns Ctrl+C with Shift/Alt into a bare Copy event, never a C key: give those
        // chords their key back so they can be bound (and recorded on the Shortcuts page).
        ctx.input_mut(|i| {
            let m = i.modifiers;
            for e in i.events.iter_mut().filter(|e| matches!(e, egui::Event::Copy)) {
                if m.shift || m.alt {
                    *e = egui::Event::Key { key: Key::C, physical_key: None, pressed: true, repeat: false, modifiers: m };
                }
            }
        });
        if self.key_capture.is_some() {
            if self.settings_open && self.settings_section == crate::settingspage::Section::Shortcuts {
                // the Shortcuts page is recording a key: nothing fires
                return;
            }
            self.key_capture = None;
        }
        let typing = ctx.wants_keyboard_input();
        let table = crate::keymap::table(&self.settings.keys);
        ctx.input_mut(|i| {
            for (k, c) in &table {
                if i.consume_shortcut(k) {
                    cmds.push(*c);
                }
            }
        });
        for c in cmds {
            self.run_cmd(c);
        }
        if typing || self.palette.open.is_some() {
            return;
        }
        // Ctrl+C over the diff / blame view copies the selected code.
        if !self.settings_open && self.rail != RailTab::Prs && matches!(self.centre, Centre::Diff | Centre::Blame) && ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) && self.copy_code() {
            ctx.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Copy)));
        }
        let (next_h, prev_h, down, up, esc) = ctx.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::CloseBracket), i.consume_key(Modifiers::NONE, Key::OpenBracket), i.consume_key(Modifiers::NONE, Key::J), i.consume_key(Modifiers::NONE, Key::K), i.consume_key(Modifiers::NONE, Key::Escape)));
        if esc {
            self.base_pop.open = false;
            self.settings_open = false;
        }
        if next_h || prev_h {
            self.step_hunk(next_h);
        }
        if down || up {
            self.step_commit(down);
        }
    }

    fn step_commit(&mut self, forward: bool) {
        if self.commits.is_empty() {
            return;
        }
        let cur = match &self.target {
            Some(TargetSel::Commit(s)) => self.commits.iter().position(|c| &c.sha == s),
            _ => None,
        };
        let next = match (cur, forward) {
            (Some(i), true) => (i + 1).min(self.commits.len() - 1),
            (Some(i), false) => i.saturating_sub(1),
            (None, _) => 0,
        };
        let sha = self.commits[next].sha.clone();
        self.commit_scroll_to = Some(next);
        self.select_commit(&sha);
    }

    fn step_hunk(&mut self, forward: bool) {
        let Some(p) = self.prepared.ready() else { return };
        let rows = p.model.hunk_rows(self.settings.split_view);
        if rows.is_empty() {
            return;
        }
        let cur = self.diff_scroll_to.unwrap_or(self.diff_top_row);
        let target = if forward { rows.iter().copied().find(|r| *r > cur).unwrap_or(rows[0]) } else { rows.iter().rev().copied().find(|r| *r < cur).unwrap_or(*rows.last().unwrap()) };
        self.diff_scroll_to = Some(target);
        self.diff_top_row = target;
    }

    // ------------------------------------------------------------ top bar

    fn top_bar(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        let narrow = ui.ctx().screen_rect().width() < 1000.0;
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = m.space[1];
            self.project_header(ui, narrow);
            if !self.head_name.is_empty() {
                widgets::badge(ui, &th, &self.head_name, th.muted());
            }
            // Comparison controls belong to the code views only.
            if self.settings_open || self.rail == RailTab::Prs {
                return;
            }
            ui.add_space(m.space[1]);
            // Base chip: always visible in the code views, shows the comparison base in effect.
            let avail = ui.available_width();
            let base_label = self.base_label();
            let r = widgets::chip(ui, &th, "Base", &base_label, true, (avail * 0.4).max(160.0));
            if r.clicked() {
                self.base_pop.open = !self.base_pop.open;
            }
            self.base_chip_rect = Some(r.rect);
            if let (Some(c), true) = (&self.cur, ui.available_width() > 640.0) {
                let txt = format!("{} → {}", c.old_label, c.new_label);
                ui.label(RichText::new(txt).font(widgets::mono_font(&th)).color(th.muted()));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = m.space[1];
                // Diff layout only means something while a diff is on screen.
                if self.centre == Centre::Diff && self.prepared.ready().is_some() {
                    let mut v = usize::from(self.settings.split_view);
                    if widgets::segmented(ui, &th, &["Unified", "Side by side"], &mut v) {
                        self.settings.split_view = v == 1;
                        self.settings_dirty_at = Some(Instant::now());
                    }
                }
                if ui.ctx().screen_rect().width() >= 1000.0 && widgets::Btn::new("Ask AI").icon(Icon::Spark).kind(BtnKind::Primary).enabled(self.selection.is_some() && self.remote.is_none()).show(ui, &th).on_hover_text(if self.remote.is_some() { crate::app::REMOTE_NO_AGENT } else { "Ctrl+L" }).clicked() {
                    self.start_ask();
                }
                if widgets::Btn::new("Inspector").kind(BtnKind::Ghost).selected(self.insp_open).show(ui, &th).clicked() {
                    self.insp_open = !self.insp_open;
                }
            });
        });
    }

    // ------------------------------------------------------------ status bar

    fn status_bar(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let f = widgets::small_font(&th);
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            let msg = match (&self.status_msg, self.busy()) {
                (Some((m, _)), _) => m.clone(),
                (None, true) => "Working…".to_string(),
                _ => "Ready".to_string(),
            };
            ui.label(RichText::new(msg).font(f.clone()).color(th.fg()));
            if let Some(Sel { code_ref, .. }) = &self.selection {
                ui.label(RichText::new(code_ref.to_string()).font(widgets::mono_font(&th)).color(th.accent()));
            }
            if let Some(e) = &self.editor {
                if self.centre == Centre::Editor && e.dirty() {
                    ui.label(RichText::new("Unsaved changes").font(f.clone()).color(th.warn()));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                if let Some(s) = &self.store {
                    let st = s.stats();
                    let unit = if st.records == 1 { "record" } else { "records" };
                    let txt = if st.read_only { format!("{} {unit} · read-only", st.records) } else { format!("{} {unit}", st.records) };
                    ui.label(RichText::new(txt).font(f.clone()).color(th.muted()));
                } else if self.store_note.is_some() {
                    ui.label(RichText::new("No store").font(f.clone()).color(th.warn()));
                }
                if self.search.stats.is_some() {
                    ui.label(RichText::new(self.last_search_stats_line.clone()).font(f.clone()).color(th.muted()));
                }
                if let Some(e) = &self.editor {
                    if self.centre == Centre::Editor {
                        ui.label(RichText::new(format!("Ln {}", e.cursor_line)).font(f.clone()).color(th.muted()));
                    }
                }
                self.resource_label(ui, &th, f.clone());
            });
        });
    }

    // ------------------------------------------------------------ left rail

    /// Switches the left section with the side effects each one needs.
    pub fn set_rail(&mut self, tab: RailTab) {
        self.settings_open = false;
        self.rail = tab;
        let dirty = self.wt.summary.as_ref().map_or(0, |s| s.files);
        if tab == RailTab::Current && dirty > 0 && !matches!(self.target, Some(TargetSel::Worktree)) {
            self.select_worktree();
        }
        if tab == RailTab::Search {
            self.search.focus = true;
        }
        if tab == RailTab::Runs {
            self.ag_open_runs();
        } else if self.centre == Centre::Run {
            self.centre = Centre::Diff;
        }
        if tab == RailTab::Prs {
            self.prs_refresh_if_stale();
        }
    }

    /// The narrow bar at the far left: one icon + label per section, settings at the bottom.
    pub(crate) fn activity_bar(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let dirty = self.wt.summary.as_ref().map_or(0, |s| s.files);
        let runs = self.ag.runs.badge();
        let needs = self.prs.list.as_ref().map_or(0, |(_, v)| v.iter().filter(|p| p.phase() == crate::prs::Phase::NeedsWork).count());
        let remote = self.remote.is_some();
        let key = |c: Cmd| crate::keymap::binding(&self.settings.keys, c).map(|k| crate::keymap::format(&k));
        let keys = [key(Cmd::ShowCommits), key(Cmd::ShowChanges), key(Cmd::SearchPanel), key(Cmd::ShowFiles), key(Cmd::PullRequests), key(Cmd::ShowRuns)];
        let items: [(RailTab, Icon, &str, usize, Option<String>); 6] = [
            (RailTab::Commits, Icon::Commit, "Commits", 0, None),
            (RailTab::Current, Icon::Changes, "Changes", dirty, None),
            (RailTab::Search, Icon::Search, "Search", 0, None),
            (RailTab::Files, Icon::File, "Files", 0, None),
            (RailTab::Prs, Icon::PullRequest, "PRs", needs, remote.then(|| "Pull requests: open a local project".to_string())),
            (RailTab::Runs, Icon::Play, "Runs", runs, remote.then(|| format!("Runs: {}", crate::app::REMOTE_NO_AGENT))),
        ];
        ui.add_space(6.0);
        ui.spacing_mut().item_spacing.y = 2.0;
        let mut pick = None;
        for ((tab, icon, label, badge, off), k) in items.into_iter().zip(keys) {
            let sel = self.rail == tab && !self.settings_open;
            let tip = off.clone().unwrap_or_else(|| k.map_or(label.to_string(), |k| format!("{label} ({k})")));
            if activity_item(ui, &th, icon, label, sel, badge, off.is_some(), &tip).clicked() && off.is_none() {
                pick = Some(tab);
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
            ui.add_space(6.0);
            let tip = key(Cmd::Settings).map_or("Settings".to_string(), |k| format!("Settings ({k})"));
            if activity_item(ui, &th, Icon::Settings, "Settings", self.settings_open, 0, false, &tip).clicked() {
                self.settings_open = !self.settings_open;
            }
        });
        if let Some(t) = pick {
            self.set_rail(t);
        }
    }

    fn rail_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        let title = match self.rail {
            RailTab::Commits => "Commits",
            RailTab::Current => "Changes",
            RailTab::Search => "Search",
            RailTab::Files => "Files",
            RailTab::Runs => "Runs",
            RailTab::Prs => "Pull requests",
        };
        ui.add_space(m.space[2]);
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            ui.label(RichText::new(title).font(widgets::heading_font(&th)).color(th.fg()));
        });
        ui.add_space(m.space[1]);
        match self.rail {
            RailTab::Commits => self.commits_panel(ui),
            RailTab::Current => self.current_panel(ui),
            RailTab::Search => self.search_panel(ui),
            RailTab::Files => self.files_panel(ui),
            RailTab::Runs => self.runs_panel(ui),
            RailTab::Prs => {}
        }
    }

    fn commits_panel(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        // Commit / author search.
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            let w = ui.available_width() - m.space[2] - 168.0;
            let te = egui::TextEdit::singleline(&mut self.commit_filter).hint_text("Search commits").desired_width(w.max(80.0)).margin(vec2(8.0, 6.0));
            if ui.add_sized([w.max(80.0), m.control_height], te).changed() {
                self.commit_filter_at = Some(Instant::now());
            }
            let mut mode = self.commit_filter_mode;
            if widgets::segmented(ui, &th, &["Message", "Author"], &mut mode) {
                self.commit_filter_mode = mode;
                self.commit_filter_at = Some(Instant::now());
            }
        });
        ui.add_space(m.space[1]);
        self.narrow_chip(ui);
        let total = ui.max_rect().height();
        let nfiles = self.diffset.ready().map_or(0, |s| s.files.len());
        let files_h = crate::railsplit::files_height(self.settings.files_split, nfiles, total);
        let panel = egui::TopBottomPanel::bottom("rail_files").resizable(false).exact_height(files_h).frame(egui::Frame::new().fill(th.bg()).stroke(egui::Stroke::new(1.0, th.border()))).show_inside(ui, |ui| self.changed_files_ui(ui));
        // Splitter on the panel's top edge: drag to resize, double-click to reset.
        let top = panel.response.rect.top();
        let grab = Rect::from_min_max(pos2(panel.response.rect.left(), top - 4.0), pos2(panel.response.rect.right(), top + 4.0));
        let sp = ui.interact(grab, egui::Id::new("rail_files_split"), egui::Sense::click_and_drag()).on_hover_cursor(egui::CursorIcon::ResizeVertical).on_hover_text("Drag to resize · double-click to reset");
        if sp.hovered() || sp.dragged() {
            ui.painter().line_segment([pos2(grab.left(), top), pos2(grab.right(), top)], egui::Stroke::new(2.0, th.accent()));
        }
        if sp.double_clicked() {
            self.settings.files_split = None;
            self.settings_dirty_at = Some(Instant::now());
        } else if sp.dragged() {
            self.settings.files_split = Some(crate::railsplit::frac_for(files_h - sp.drag_delta().y, total));
            self.settings_dirty_at = Some(Instant::now());
        }

        // Commit list.
        let row_h = th.m().commit_row_height;
        if matches!(self.open_state, Loadable::Failed(_)) {
            widgets::empty_state(ui, &th, "Repository not opened", "See the message at the top.");
            return;
        }
        if self.open_state.is_loading() || (self.commits_loading && self.commits.is_empty()) {
            widgets::skeleton(ui, &th, 8, row_h);
            return;
        }
        if let Some(e) = self.commits_err.clone() {
            if self.commits.is_empty() {
                if widgets::banner(ui, &th, Level::Error, &format!("Could not read history: {e}"), Some("Retry"), false) == widgets::BannerAction::Action {
                    self.request_log(false);
                }
                return;
            }
        }
        if self.commits.is_empty() {
            let q = !self.commit_filter_applied.0.trim().is_empty() || self.narrow_applied.is_some();
            widgets::empty_state(ui, &th, if q { "No matching commits" } else { "No commits yet" }, if q { "Try another term, switch between Message and Author, or turn off narrowing by files." } else { "Make a commit and it will show up here." });
            return;
        }
        let n = self.commits.len();
        let mut area = ScrollArea::vertical().id_salt("commit_list").auto_shrink([false, false]);
        if let Some(i) = self.commit_scroll_to.take() {
            area = area.vertical_scroll_offset((i as f32 * row_h - 2.0 * row_h).max(0.0));
        }
        let mut pick: Option<String> = None;
        let mut need_more = false;
        let now = crate::timefmt::now();
        let sel_sha = match &self.target {
            Some(TargetSel::Commit(s)) => Some(s.clone()),
            _ => None,
        };
        area.show_rows(ui, row_h, n, |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            if range.end + 15 >= n {
                need_more = true;
            }
            for i in range {
                let c = &self.commits[i];
                let (resp, rect) = widgets::list_row(ui, &th, row_h, sel_sha.as_deref() == Some(c.sha.as_str()));
                let p = ui.painter();
                let l1 = Rect::from_min_max(pos2(rect.min.x, rect.min.y + 4.0), pos2(rect.max.x, rect.min.y + row_h * 0.52));
                let l2 = Rect::from_min_max(pos2(rect.min.x, rect.min.y + row_h * 0.52), pos2(rect.max.x, rect.max.y - 4.0));
                let mut x = l1.min.x;
                for r in c.refs.iter().take(2) {
                    let (label, col) = ref_style(&th, r);
                    x += widgets::paint_badge(p, &th, pos2(x, l1.center().y), &label, col) + 6.0;
                }
                widgets::paint_text_fit(ui, Rect::from_min_max(pos2(x, l1.min.y), l1.max), &c.subject, widgets::ui_font(&th), th.fg());
                let meta = format!("{}  ·  {}  ·  {}", crate::timefmt::short(&c.sha), c.author, crate::timefmt::rel(now, c.time));
                widgets::paint_text_fit(ui, l2, &meta, widgets::small_font(&th), th.muted());
                if c.parents.len() > 1 {
                    widgets::paint_text_right(ui, l2, "merge", widgets::small_font(&th), th.muted());
                }
                if resp.clicked() {
                    pick = Some(c.sha.clone());
                }
                resp.on_hover_text(crate::timefmt::abs(c.time));
            }
            if self.commits_loading {
                ui.add_space(4.0);
                ui.label(RichText::new("Loading more…").font(widgets::small_font(&th)).color(th.muted()));
            } else if self.commits_done {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(RichText::new("Start of history").font(widgets::small_font(&th)).color(th.muted()));
                });
            }
        });
        if need_more && !self.commits_done && !self.commits_loading {
            self.request_log(true);
        }
        if let Some(s) = pick {
            self.select_commit(&s);
        }
    }

    fn changed_files_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        ui.add_space(m.space[1]);
        match &self.diffset {
            Loadable::Idle => {
                widgets::empty_state(ui, &th, "No comparison yet", "Select a commit.");
            }
            Loadable::Loading => widgets::skeleton(ui, &th, 4, 28.0),
            Loadable::Failed(e) => {
                let e = e.clone();
                if widgets::banner(ui, &th, Level::Error, &format!("Could not diff: {e}"), Some("Retry"), false) == widgets::BannerAction::Action {
                    self.request_diff();
                }
            }
            Loadable::Ready(_) => self.files_ready_ui(ui),
        }
    }

    pub(crate) fn search_panel(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        let mut changed = false;
        let qid = egui::Id::new("search_query");
        // Up/Down step through the hits while the query field (or nothing) has focus; consumed
        // before the field is drawn so the text edit does not move its cursor (its focus
        // lock filter keeps vertical arrows, so egui does not move focus to a neighbouring widget either).
        let mut step: Option<bool> = None;
        if self.palette.open.is_none() && self.tt.is_none() && ui.memory(|mm| mm.focused().is_none_or(|f| f == qid)) {
            let (down, up) = ui.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::ArrowDown), i.consume_key(Modifiers::NONE, Key::ArrowUp)));
            step = if down { Some(true) } else if up { Some(false) } else { None };
        }
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            let w = ui.available_width() - m.space[2];
            let te = egui::TextEdit::singleline(&mut self.search.query).id(qid).hint_text("Search file contents").desired_width(w).margin(vec2(8.0, 6.0));
            let r = ui.add_sized([w, m.control_height], te);
            if r.has_focus() {
                ui.memory_mut(|mm| mm.set_focus_lock_filter(qid, egui::EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() }));
            }
            if self.search.focus {
                r.request_focus();
                self.search.focus = false;
            }
            if r.changed() {
                changed = true;
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                self.search.edited_at = None;
                self.start_search();
            }
        });
        ui.add_space(m.space[0]);
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            ui.spacing_mut().item_spacing.x = m.space[0];
            for (label, flag) in [("Aa", 0), (".*", 1)] {
                let on = if flag == 0 { self.search.case } else { self.search.regex };
                let tip = if flag == 0 { "Match case" } else { "Regular expression" };
                if Btn::new(label).compact().selected(on).kind(BtnKind::Ghost).show(ui, &th).on_hover_text(tip).clicked() {
                    if flag == 0 {
                        self.search.case = !self.search.case;
                    } else {
                        self.search.regex = !self.search.regex;
                    }
                    changed = true;
                }
            }
            let w = ui.available_width() - m.space[2];
            let te = egui::TextEdit::singleline(&mut self.search.globs).hint_text("Globs, e.g. src/** !*.lock").desired_width(w).margin(vec2(8.0, 4.0));
            if ui.add_sized([w, m.control_height_compact], te).changed() {
                changed = true;
            }
        });
        if changed {
            self.search.edited_at = Some(Instant::now());
        }
        ui.add_space(m.space[1]);
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            let txt = if self.search.running { format!("Searching… {} matches", self.search.total) } else if self.search.stats.is_some() { self.last_search_stats_line.clone() } else { String::new() };
            ui.label(RichText::new(txt).font(widgets::small_font(&th)).color(th.muted()));
            if self.search.running {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(m.space[2]);
                    if Btn::new("Cancel").compact().kind(BtnKind::Ghost).show(ui, &th).clicked() {
                        self.jobs.cancel(JobKind::Search);
                        self.search.running = false;
                        self.last_search_stats_line = format!("{} matches · cancelled", self.search.total);
                        self.search.stats = Some(Default::default());
                    }
                });
            }
        });
        if let Some(e) = self.search.error.clone() {
            widgets::banner(ui, &th, Level::Error, &format!("Search failed: {e}"), None, false);
            return;
        }
        if self.search.query.is_empty() {
            widgets::empty_state(ui, &th, "Search the working tree", "Type to search. Respects .gitignore.");
            return;
        }
        if self.search.groups.is_empty() {
            if self.search.running {
                widgets::skeleton(ui, &th, 6, 26.0);
            } else if self.search.ran.is_some() {
                widgets::empty_state(ui, &th, "No matches", "Check the spelling, case and glob filters.");
            }
            return;
        }
        if self.search.capped {
            widgets::banner(ui, &th, Level::Warn, &format!("Showing the first {MAX_SEARCH_HITS} matches. Narrow the query."), None, false);
        }
        let mut flat = flat_rows(&self.search.groups);
        let mut toggle: Option<usize> = None;
        let mut open: Option<(usize, usize)> = None;
        let row_h = 26.0;
        let mut area = ScrollArea::vertical().id_salt("search_list").auto_shrink([false, false]);
        let reveal = std::mem::take(&mut self.search_scroll_to).then_some(self.search.selected).flatten();
        if let Some((g, h)) = reveal.filter(|_| step.is_none()) {
            if let Some(i) = flat.iter().position(|r| *r == (g, Some(h))) {
                area = area.vertical_scroll_offset((i as f32 * row_h - ui.available_height() / 2.0).max(0.0));
            }
        }
        if let Some(down) = step {
            if let Some((g, h)) = step_hit(&self.search.groups, self.search.selected, down) {
                open = Some((g, h));
                self.search.selected = Some((g, h));
                if self.search.groups[g].collapsed {
                    self.search.groups[g].collapsed = false;
                    flat = flat_rows(&self.search.groups);
                }
                if let Some(i) = flat.iter().position(|r| *r == (g, Some(h))) {
                    area = area.vertical_scroll_offset((i as f32 * row_h - ui.available_height() / 2.0).max(0.0));
                }
            }
        }
        let sel = self.search.selected;
        area.show_rows(ui, row_h, flat.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in range {
                let (gi, hi) = flat[i];
                let g = &self.search.groups[gi];
                match hi {
                    None => {
                        let (resp, rect) = widgets::list_row(ui, &th, row_h, false);
                        let c = pos2(rect.min.x + 6.0, rect.center().y);
                        widgets::paint_icon(ui.painter(), c, if g.collapsed { Icon::ChevronRight } else { Icon::ChevronDown }, th.muted());
                        let cnt = format!("{}", g.hits.len());
                        let cw = widgets::paint_text_right(ui, rect, &cnt, widgets::small_font(&th), th.muted());
                        widgets::paint_text_fit(ui, Rect::from_min_max(pos2(rect.min.x + 18.0, rect.min.y), pos2(rect.max.x - cw - 8.0, rect.max.y)), &g.path, widgets::ui_font(&th), th.fg());
                        if resp.clicked() {
                            toggle = Some(gi);
                        }
                    }
                    Some(h) => {
                        let hit = &g.hits[h];
                        let (resp, rect) = widgets::list_row(ui, &th, row_h, sel == Some((gi, h)));
                        let ln = format!("{}", hit.line);
                        let lw = 36.0;
                        widgets::paint_text_right(ui, Rect::from_min_max(rect.min, pos2(rect.min.x + lw, rect.max.y)), &ln, widgets::mono_font(&th), th.muted());
                        let trimmed = hit.text.trim_start();
                        let cut = hit.text.len() - trimmed.len();
                        let text_rect = Rect::from_min_max(pos2(rect.min.x + lw + 8.0, rect.min.y), rect.max);
                        let font = widgets::mono_font(&th);
                        let gal = widgets::fit(ui, trimmed, font.clone(), th.fg(), text_rect.width());
                        let gp = pos2(text_rect.min.x, rect.center().y - gal.size().y / 2.0);
                        // match highlight (only when the match survives truncation: galley shows a prefix)
                        let shown_chars = gal.text().chars().count();
                        for (s, e) in hit.ranges.iter().map(|(s, e)| (*s as usize, *e as usize)) {
                            if s < cut || e <= s {
                                continue;
                            }
                            let (s, e) = (s - cut, e - cut);
                            if e > trimmed.len() || !trimmed.is_char_boundary(s) || !trimmed.is_char_boundary(e) {
                                continue;
                            }
                            let (cs, ce) = (trimmed[..s].chars().count(), trimmed[..e].chars().count());
                            if cs >= shown_chars {
                                continue;
                            }
                            let ce = ce.min(shown_chars);
                            let x0 = gal.pos_from_ccursor(egui::text::CCursor::new(cs)).min.x;
                            let x1 = gal.pos_from_ccursor(egui::text::CCursor::new(ce)).min.x;
                            ui.painter().rect_filled(Rect::from_min_max(pos2(gp.x + x0, rect.min.y + 4.0), pos2(gp.x + x1.max(x0 + 2.0), rect.max.y - 4.0)), 2.0, th.c(th.p().diff.add.word));
                        }
                        ui.painter().galley(gp, gal, th.fg());
                        if resp.clicked() {
                            open = Some((gi, h));
                        }
                    }
                }
            }
        });
        if let Some(g) = toggle {
            self.search.groups[g].collapsed = !self.search.groups[g].collapsed;
        }
        if let Some((g, h)) = open {
            self.search.selected = Some((g, h));
            let hit = self.search.groups[g].hits[h].clone();
            self.open_editor(&hit.path, Some(hit.line));
            if let (Some(e), Some(&(s, end))) = (self.editor.as_mut().filter(|e| e.path == hit.path), hit.ranges.first()) {
                e.mark = Some((hit.line, s as usize, end as usize));
            }
            if self.blame.path == hit.path && self.centre == Centre::Blame {
                // remote: blame view stands in for the editor; select the hit line
                let i = hit.line.saturating_sub(1) as usize;
                self.blame.selected = Some((i, i));
            }
            let r = crate::selection::code_ref_for_lines(&hit.path, hit.line, hit.line, &ct_core::RefAt::Worktree);
            self.set_selection(r, SelSource::Search, hit.text);
        }
    }

    fn files_panel(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            let w = ui.available_width() - m.space[2];
            let te = egui::TextEdit::singleline(&mut self.files.filter).hint_text("Filter files").desired_width(w).margin(vec2(8.0, 6.0));
            ui.add_sized([w, m.control_height], te);
        });
        ui.add_space(m.space[1]);
        let picked = self.file_list_ui(ui, "files_tab", false);
        if let Some(p) = picked {
            self.open_file_diff_or_editor(&p);
        }
    }

    /// Opens `path`: its diff when the current comparison touches it, otherwise the file itself in
    /// the stored view mode (no diff exists there, so Diff falls back to Blame with a note).
    pub fn open_file_diff_or_editor(&mut self, path: &str) {
        let in_diff = self.diffset.ready().is_some_and(|s| s.files.iter().any(|f| f.path == path));
        if in_diff {
            if self.centre == Centre::Run {
                self.centre = Centre::Diff;
            }
            self.open_file_diff(path);
        } else {
            self.open_outside_diff(path);
        }
    }

    /// Fuzzy/plain file list shared by the Files tab and the Ctrl+P picker. Returns a picked path.
    pub fn file_list_ui(&mut self, ui: &mut Ui, id: &str, keyboard: bool) -> Option<String> {
        let th = self.th.clone();
        match &self.files.all {
            Loadable::Idle | Loadable::Loading => {
                widgets::skeleton(ui, &th, 8, 26.0);
                return None;
            }
            Loadable::Failed(e) => {
                let e = e.clone();
                if widgets::banner(ui, &th, Level::Error, &format!("Could not list files: {e}"), Some("Retry"), false) == widgets::BannerAction::Action {
                    self.request_files();
                }
                return None;
            }
            Loadable::Ready(_) => {}
        }
        if self.files.filtered_for != self.files.filter {
            self.files.filtered_for = self.files.filter.clone();
            self.files.selected = 0;
            if let Loadable::Ready((all, idx)) = &self.files.all {
                self.files.results = if self.files.filter.trim().is_empty() { all.iter().take(500).map(|p| (p.clone(), 0, Vec::new())).collect() } else { idx.query(self.files.filter.trim(), 500) };
            }
        }
        if self.files.results.is_empty() {
            widgets::empty_state(ui, &th, "No files match", "Try fewer letters, or part of the folder name.");
            return None;
        }
        let n = self.files.results.len();
        let mut picked = None;
        if keyboard {
            let (down, up, enter) = ui.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::ArrowDown), i.consume_key(Modifiers::NONE, Key::ArrowUp), i.consume_key(Modifiers::NONE, Key::Enter)));
            if down {
                self.files.selected = (self.files.selected + 1).min(n - 1);
            }
            if up {
                self.files.selected = self.files.selected.saturating_sub(1);
            }
            if enter {
                picked = Some(self.files.results[self.files.selected].0.clone());
            }
        }
        let row_h = 28.0;
        let mut area = ScrollArea::vertical().id_salt(id).auto_shrink([false, false]);
        if keyboard {
            let top = self.files.selected as f32 * row_h;
            let _ = top;
            area = area.max_height(360.0);
        }
        let sel = if keyboard { Some(self.files.selected) } else { None };
        let cur = self.file_sel.clone();
        area.show_rows(ui, row_h, n, |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in range {
                let (path, _, hits) = &self.files.results[i];
                let is_sel = sel == Some(i) || (sel.is_none() && cur.as_deref() == Some(path.as_str()));
                let (resp, rect) = widgets::list_row(ui, &th, row_h, is_sel);
                if keyboard && sel == Some(i) {
                    resp.scroll_to_me(None);
                }
                let (dir, name) = match path.rfind('/') {
                    Some(p) => (&path[..p], &path[p + 1..]),
                    None => ("", path.as_str()),
                };
                let font = widgets::ui_font(&th);
                let name_start = path.chars().count() - name.chars().count();
                let g = fuzzy_galley(ui, name, &font, &th, hits, name_start);
                let gw = g.size().x.min(rect.width() * 0.6);
                ui.painter().with_clip_rect(rect).galley(pos2(rect.min.x, rect.center().y - g.size().y / 2.0), g, th.fg());
                if !dir.is_empty() {
                    widgets::paint_text_fit(ui, Rect::from_min_max(pos2(rect.min.x + gw + 8.0, rect.min.y), rect.max), dir, widgets::small_font(&th), th.muted());
                }
                if resp.clicked() {
                    picked = Some(path.clone());
                }
            }
        });
        picked
    }

    // ------------------------------------------------------------ popovers / dialogs

    fn base_popover(&mut self, ctx: &egui::Context) {
        if !self.base_pop.open {
            return;
        }
        let th = self.th.clone();
        let m = th.m().clone();
        let anchor = self.base_chip_rect.map(|r| r.left_bottom() + vec2(0.0, 6.0)).unwrap_or(pos2(200.0, 56.0));
        let mut close = false;
        let mut apply: Option<Result<BaseMode, (String, String)>> = None;
        let multi = self.is_merge_selected();
        let area = egui::Area::new(egui::Id::new("base_pop")).order(egui::Order::Foreground).fixed_pos(anchor).show(ctx, |ui| {
            egui::Frame::new().fill(th.raised()).stroke(egui::Stroke::new(1.0, th.border())).corner_radius(th.radius()).inner_margin(egui::Margin::same(12)).shadow(egui::Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(90) }).show(ui, |ui| {
                ui.set_width(340.0);
                ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                widgets::heading(ui, &th, "Compare against");
                let cur_mode = if self.range.is_some() { None } else { Some(self.base.clone()) };
                let opt = |ui: &mut Ui, label: &str, hint: &str, on: bool| -> bool {
                    let (resp, rect) = widgets::list_row(ui, &th, 32.0, on);
                    let w = widgets::paint_text_right(ui, rect, hint, widgets::small_font(&th), th.muted());
                    widgets::paint_text_fit(ui, Rect::from_min_max(rect.min, pos2(rect.max.x - w - 8.0, rect.max.y)), label, widgets::ui_font(&th), th.fg());
                    resp.clicked()
                };
                if opt(ui, "Parent", "default", cur_mode == Some(BaseMode::Parent)) {
                    apply = Some(Ok(BaseMode::Parent));
                }
                if multi {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Merge parent").font(widgets::small_font(&th)).color(th.muted()));
                        let mut i = usize::from(self.parent_n.max(1) - 1).min(1);
                        let ncount = self.selected_commit().map(|c| c.parents.len()).unwrap_or(2).min(2);
                        let labels = if ncount > 1 { vec!["1", "2"] } else { vec!["1"] };
                        if widgets::segmented(ui, &th, &labels, &mut i) {
                            self.parent_n = (i + 1) as u8;
                            apply = Some(Ok(BaseMode::Parent));
                        }
                    });
                }
                if opt(ui, "Previous selection", self.prev_sha.as_deref().map(crate::timefmt::short).unwrap_or("none"), cur_mode == Some(BaseMode::Previous)) && self.prev_sha.is_some() {
                    apply = Some(Ok(BaseMode::Previous));
                }
                if opt(ui, "Current changes", "uncommitted", cur_mode == Some(BaseMode::WorkingTree)) {
                    apply = Some(Ok(BaseMode::WorkingTree));
                }
                ui.separator();
                let ref_names: Vec<String> = self.refs.iter().map(|r| r.name.clone()).collect();
                ui.label(RichText::new("Any ref or merge-base").font(widgets::small_font(&th)).color(th.muted()));
                ui.horizontal(|ui| {
                    let w = 204.0;
                    ui.add_sized([w, m.control_height], egui::TextEdit::singleline(&mut self.base_pop.ref_input).hint_text("branch, tag or sha").margin(vec2(8.0, 6.0)));
                    let ok = !self.base_pop.ref_input.trim().is_empty();
                    if Btn::new("Use").enabled(ok).show(ui, &th).clicked() {
                        apply = Some(Ok(BaseMode::Ref(self.base_pop.ref_input.trim().to_string())));
                    }
                    if Btn::new("Merge-base").enabled(ok).show(ui, &th).clicked() {
                        apply = Some(Ok(BaseMode::MergeBase(self.base_pop.ref_input.trim().to_string())));
                    }
                });
                let q = self.base_pop.ref_input.trim().to_lowercase();
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                    for n in ref_names.iter().filter(|n| q.is_empty() || n.to_lowercase().contains(&q)).take(8) {
                        if Btn::new(n).compact().kind(BtnKind::Ghost).show(ui, &th).clicked() {
                            self.base_pop.ref_input = n.clone();
                        }
                    }
                });
                ui.separator();
                ui.label(RichText::new("Range").font(widgets::small_font(&th)).color(th.muted()));
                ui.horizontal(|ui| {
                    ui.add_sized([110.0, m.control_height], egui::TextEdit::singleline(&mut self.base_pop.range_a).hint_text("from").margin(vec2(8.0, 6.0)));
                    ui.label(RichText::new("..").color(th.muted()));
                    ui.add_sized([110.0, m.control_height], egui::TextEdit::singleline(&mut self.base_pop.range_b).hint_text("to").margin(vec2(8.0, 6.0)));
                    let ok = !self.base_pop.range_a.trim().is_empty() && !self.base_pop.range_b.trim().is_empty();
                    if Btn::new("Apply").enabled(ok).show(ui, &th).clicked() {
                        apply = Some(Err((self.base_pop.range_a.trim().to_string(), self.base_pop.range_b.trim().to_string())));
                    }
                });
            });
        });
        if ctx.input(|i| i.pointer.any_click()) {
            let over = ctx.input(|i| i.pointer.interact_pos()).is_some_and(|p| area.response.rect.contains(p) || self.base_chip_rect.is_some_and(|r| r.contains(p)));
            if !over {
                close = true;
            }
        }
        match apply {
            Some(Ok(b)) => {
                self.set_base(b);
                close = true;
            }
            Some(Err((a, b))) => {
                self.set_range(a, b);
                close = true;
            }
            None => {}
        }
        if close {
            self.base_pop.open = false;
        }
    }

    fn palette_ui(&mut self, ctx: &egui::Context) {
        let Some(mode) = self.palette.open else { return };
        let th = self.th.clone();
        let screen = ctx.screen_rect();
        let w = 520.0_f32.min(screen.width() - 32.0);
        let mut close = false;
        let mut run: Option<Cmd> = None;
        let mut pick: Option<String> = None;
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            close = true;
        }
        egui::Area::new(egui::Id::new("palette")).order(egui::Order::Foreground).fixed_pos(pos2(screen.center().x - w / 2.0, screen.top() + 72.0)).show(ctx, |ui| {
            egui::Frame::new().fill(th.raised()).stroke(egui::Stroke::new(1.0, th.border())).corner_radius(th.radius()).inner_margin(egui::Margin::same(12)).shadow(egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(110) }).show(ui, |ui| {
                ui.set_width(w - 24.0);
                match mode {
                    PaletteMode::Files => {
                        let te = egui::TextEdit::singleline(&mut self.files.filter).hint_text("Go to file").desired_width(f32::INFINITY).margin(vec2(8.0, 6.0));
                        let r = ui.add_sized([w - 24.0, th.m().control_height], te);
                        if self.palette.focus {
                            r.request_focus();
                            self.palette.focus = false;
                        }
                        ui.add_space(8.0);
                        pick = self.file_list_ui(ui, "palette_files", true);
                    }
                    PaletteMode::Commands => {
                        let te = egui::TextEdit::singleline(&mut self.palette.input).hint_text("Type a command").desired_width(f32::INFINITY).margin(vec2(8.0, 6.0));
                        let r = ui.add_sized([w - 24.0, th.m().control_height], te);
                        if self.palette.focus {
                            r.request_focus();
                            self.palette.focus = false;
                        }
                        ui.add_space(8.0);
                        let q = self.palette.input.to_lowercase();
                        let items: Vec<&(Cmd, &str, &str)> = COMMANDS.iter().filter(|c| q.is_empty() || c.1.to_lowercase().contains(&q)).collect();
                        if items.is_empty() {
                            widgets::empty_state(ui, &th, "No such command", "Try a word from the shortcut list.");
                        } else {
                            let (down, up, enter) = ui.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::ArrowDown), i.consume_key(Modifiers::NONE, Key::ArrowUp), i.consume_key(Modifiers::NONE, Key::Enter)));
                            if down {
                                self.palette.sel = (self.palette.sel + 1).min(items.len() - 1);
                            }
                            if up {
                                self.palette.sel = self.palette.sel.saturating_sub(1);
                            }
                            self.palette.sel = self.palette.sel.min(items.len() - 1);
                            if enter {
                                run = Some(items[self.palette.sel].0);
                            }
                            for (i, (c, label, _)) in items.iter().enumerate() {
                                let (resp, rect) = widgets::list_row(ui, &th, 30.0, i == self.palette.sel);
                                let key = crate::keymap::binding(&self.settings.keys, *c).map(|k| crate::keymap::format(&k)).unwrap_or_default();
                                let kw = if key.is_empty() { 0.0 } else { widgets::paint_text_right(ui, rect, &key, widgets::small_font(&th), th.muted()) };
                                widgets::paint_text_fit(ui, Rect::from_min_max(rect.min, pos2(rect.max.x - kw - 8.0, rect.max.y)), label, widgets::ui_font(&th), th.fg());
                                if resp.clicked() {
                                    run = Some(*c);
                                }
                            }
                        }
                    }
                }
            });
        });
        if let Some(p) = pick {
            close = true;
            self.open_file_diff_or_editor(&p);
        }
        if let Some(c) = run {
            close = true;
            self.palette.open = None;
            self.run_cmd(c);
        }
        if close {
            if self.palette.open == Some(PaletteMode::Files) {
                self.files.filter.clear();
            }
            self.palette.open = None;
        }
    }

    pub fn diff_cache_invalidate(&mut self) {
        if self.prepared.ready().is_some() {
            self.request_file_diff();
        }
    }
}

pub fn ref_style(th: &crate::theme::Theme, r: &str) -> (String, Color32) {
    if let Some(t) = r.strip_prefix("tag: ") {
        (t.to_string(), th.warn())
    } else if let Some(h) = r.strip_prefix("HEAD -> ") {
        (h.to_string(), th.accent())
    } else if r == "HEAD" {
        ("HEAD".into(), th.accent())
    } else if r.contains('/') {
        (r.to_string(), th.muted())
    } else {
        (r.to_string(), th.ai())
    }
}

pub fn status_style(th: &crate::theme::Theme, s: ct_core::Status) -> (&'static str, Color32) {
    use ct_core::Status::*;
    match s {
        Added => ("A", th.c(th.p().diff.add.fg)),
        Deleted => ("D", th.c(th.p().diff.del.fg)),
        Modified => ("M", th.warn()),
        Renamed => ("R", th.accent()),
        Copied => ("C", th.accent()),
        TypeChanged => ("T", th.muted()),
    }
}

fn fuzzy_galley(ui: &Ui, name: &str, font: &FontId, th: &crate::theme::Theme, hits: &[u32], name_start: usize) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::default();
    let hit_bytes: std::collections::HashSet<usize> = hits.iter().map(|h| *h as usize).collect();
    for (ci, ch) in name.chars().enumerate() {
        let on = hit_bytes.contains(&(name_start + ci));
        let col = if on { th.accent() } else { th.fg() };
        job.append(&ch.to_string(), 0.0, egui::TextFormat { font_id: font.clone(), color: col, ..Default::default() });
    }
    ui.fonts(|f| f.layout_job(job))
}

/// Search list rows for virtualised rendering: group header (`None`) + its hits unless collapsed.
fn flat_rows(groups: &[SearchGroup]) -> Vec<(usize, Option<usize>)> {
    let mut flat = Vec::new();
    for (gi, g) in groups.iter().enumerate() {
        flat.push((gi, None));
        if !g.collapsed {
            flat.extend((0..g.hits.len()).map(|h| (gi, Some(h))));
        }
    }
    flat
}

/// Next / previous hit across all groups (collapsed ones included); nothing selected starts at
/// the first / last hit; stops at the ends.
pub fn step_hit(groups: &[SearchGroup], cur: Option<(usize, usize)>, down: bool) -> Option<(usize, usize)> {
    let all: Vec<(usize, usize)> = groups.iter().enumerate().flat_map(|(g, x)| (0..x.hits.len()).map(move |h| (g, h))).collect();
    let i = match cur.and_then(|c| all.iter().position(|x| *x == c)) {
        None if down => 0,
        None => all.len().checked_sub(1)?,
        Some(i) if down => (i + 1).min(all.len() - 1),
        Some(i) => i.saturating_sub(1),
    };
    all.get(i).copied()
}

#[cfg(test)]
pub mod settings_window_tests {
    use super::*;
    use crate::agents_fake::FakeAgents;

    pub fn test_app(ctx: &egui::Context) -> App {
        let (th, _) = theme::ThemeFile::load(std::path::Path::new("/nonexistent/theme.toml"));
        App::new(ctx.clone(), std::path::PathBuf::from("/nonexistent"), th, crate::settings::Settings::default(), Vec::new(), String::new(), None, std::sync::Arc::new(FakeAgents::new()))
    }
}

/// One activity-bar entry: icon over a short label, optional count badge, disabled with a reason.
#[allow(clippy::too_many_arguments)]
fn activity_item(ui: &mut Ui, th: &crate::theme::Theme, icon: Icon, label: &str, selected: bool, badge: usize, off: bool, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 54.0), egui::Sense::click());
    let r = rect.shrink2(vec2(6.0, 1.0));
    let p = ui.painter();
    if selected {
        p.rect_filled(r, th.radius(), th.selection());
    } else if resp.hovered() && !off {
        p.rect_filled(r, th.radius(), th.hover());
    }
    let col = if off { crate::theme::mix(th.muted(), th.bg(), 0.4) } else if selected { th.fg() } else { th.muted() };
    let ic = pos2(r.center().x, r.min.y + 18.0);
    widgets::paint_icon_sized(p, ic, icon, col, 18.0);
    p.text(pos2(r.center().x, r.max.y - 11.0), egui::Align2::CENTER_CENTER, label, widgets::small_font(th), col);
    if badge > 0 {
        let t = if badge > 99 { "99+".to_string() } else { badge.to_string() };
        let g = p.layout_no_wrap(t, egui::FontId::proportional(10.0), th.bg());
        let w = (g.size().x + 8.0).max(16.0);
        let b = Rect::from_min_size(pos2(ic.x + 5.0, ic.y - 15.0), vec2(w, 16.0));
        p.rect_filled(b, 8.0, if icon == Icon::PullRequest { th.p().diff.del.fg.0 } else { th.accent() });
        p.galley(pos2(b.center().x - g.size().x / 2.0, b.center().y - g.size().y / 2.0), g, th.bg());
    }
    if resp.has_focus() {
        p.rect_stroke(r, th.radius(), egui::Stroke::new(th.m().focus_ring, th.accent()), egui::StrokeKind::Inside);
    }
    let resp = resp.on_hover_text(tip);
    if off {
        resp.on_hover_cursor(egui::CursorIcon::NotAllowed)
    } else {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }
}
