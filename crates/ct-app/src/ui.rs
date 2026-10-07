//! Frame layout, top bar, left rail, status bar, popovers, palette, settings, shortcuts.

use crate::app::*;
use crate::widgets::{self, Btn, BtnKind, Icon, Level};
use crate::theme::{self, Hex};
use egui::{pos2, vec2, Align, Color32, FontId, Key, Layout, Modifiers, Rect, RichText, ScrollArea, Ui};
use crate::jobs::JobKind;
use std::time::{Duration, Instant};

const CMD: Modifiers = Modifiers::COMMAND;
const CMD_SHIFT: Modifiers = Modifiers { command: true, shift: true, ..Modifiers::NONE };

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    PickFile,
    SearchPanel,
    Palette,
    BaseMenu,
    ToggleEdit,
    Save,
    CopyRef,
    AskAi,
    ToggleSplit,
    Settings,
    GotoLine,
    ToggleInspector,
    Refresh,
}

pub const COMMANDS: [(Cmd, &str, &str); 13] = [
    (Cmd::PickFile, "Go to file", "Ctrl+P"),
    (Cmd::SearchPanel, "Search in files", "Ctrl+Shift+F"),
    (Cmd::BaseMenu, "Change base", "Ctrl+B"),
    (Cmd::ToggleEdit, "Toggle editor", "Ctrl+E"),
    (Cmd::Save, "Save file", "Ctrl+S"),
    (Cmd::CopyRef, "Copy code reference", "Ctrl+Shift+C"),
    (Cmd::AskAi, "Ask AI about selection", "Ctrl+L"),
    (Cmd::ToggleSplit, "Toggle unified / side-by-side", ""),
    (Cmd::GotoLine, "Go to line", "Ctrl+G"),
    (Cmd::ToggleInspector, "Show or hide inspector", ""),
    (Cmd::Refresh, "Reload commits and diff", ""),
    (Cmd::Settings, "Settings", ""),
    (Cmd::Palette, "Command palette", "Ctrl+K"),
];

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        self.ag_close_requested(ctx);
        self.smoke_prepare();
        self.shortcuts(ctx);
        self.debounce(ctx);
        if self.editor.is_some() && self.centre == Centre::Editor {
            self.check_editor_disk();
            ctx.request_repaint_after(Duration::from_millis(1000));
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

        let narrow = ctx.screen_rect().width() < 1000.0;
        let init_id = egui::Id::new("narrow_init");
        if ctx.data(|d| d.get_temp::<bool>(init_id)).is_none() {
            ctx.data_mut(|d| d.insert_temp(init_id, true));
            if narrow {
                self.insp_open = false;
            }
        }
        let rail_w = self.settings.rail_width;
        if !(narrow && self.insp_open) {
            let max_w = if narrow { 300.0 } else { 520.0 };
            let r = egui::SidePanel::left("rail").resizable(true).default_width(rail_w.min(max_w)).width_range(280.0..=max_w).frame(egui::Frame::new().fill(th.bg()).stroke(egui::Stroke::new(1.0, th.border()))).show(ctx, |ui| self.rail_ui(ui));
            if !narrow && (r.response.rect.width() - rail_w).abs() > 1.0 {
                self.settings.rail_width = r.response.rect.width();
                self.settings_dirty_at = Some(Instant::now());
            }
        }

        if self.insp_open {
            let insp_w = self.settings.inspector_width;
            let (lo, hi) = if narrow { (280.0, ctx.screen_rect().width() - 360.0) } else { (280.0, 560.0) };
            let r = egui::SidePanel::right("inspector").resizable(true).default_width(insp_w.clamp(lo, hi.max(lo))).width_range(lo..=hi.max(lo)).frame(egui::Frame::new().fill(th.bg()).stroke(egui::Stroke::new(1.0, th.border()))).show(ctx, |ui| self.inspector_ui(ui));
            if !narrow && (r.response.rect.width() - insp_w).abs() > 1.0 {
                self.settings.inspector_width = r.response.rect.width();
                self.settings_dirty_at = Some(Instant::now());
            }
        }

        egui::CentralPanel::default().frame(egui::Frame::new().fill(th.sunken())).show(ctx, |ui| self.centre_ui(ui));

        self.base_popover(ctx);
        self.palette_ui(ctx);
        self.ask_preview_window(ctx);
        self.settings_window(ctx);
        self.agent_windows(ctx);
        self.smoke_tick(ctx);
    }
}

impl App {
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
            Cmd::SearchPanel => {
                self.rail = RailTab::Search;
                self.search.focus = true;
            }
            Cmd::BaseMenu => self.base_pop.open = !self.base_pop.open,
            Cmd::ToggleEdit => match self.centre {
                Centre::Editor => self.centre = Centre::Diff,
                _ => {
                    if let Some(p) = self.file_sel.clone().or_else(|| self.editor.as_ref().map(|e| e.path.clone())) {
                        self.open_editor(&p, None);
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
            Cmd::AskAi => self.start_ask(),
            Cmd::ToggleSplit => {
                self.settings.split_view = !self.settings.split_view;
                self.settings_dirty_at = Some(Instant::now());
            }
            Cmd::Settings => self.settings_open = !self.settings_open,
            Cmd::GotoLine => {
                if let Some(e) = &mut self.editor {
                    if self.centre == Centre::Editor {
                        e.goto_open = true;
                    }
                }
            }
            Cmd::ToggleInspector => self.insp_open = !self.insp_open,
            Cmd::Refresh => {
                self.request_log(false);
                self.request_diff();
                self.request_files();
            }
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let mut cmds: Vec<Cmd> = Vec::new();
        let typing = ctx.wants_keyboard_input();
        ctx.input_mut(|i| {
            let mut hit = |m: Modifiers, k: Key, c: Cmd| {
                if i.consume_key(m, k) {
                    cmds.push(c);
                }
            };
            hit(CMD_SHIFT, Key::F, Cmd::SearchPanel);
            hit(CMD_SHIFT, Key::C, Cmd::CopyRef);
            hit(CMD, Key::P, Cmd::PickFile);
            hit(CMD, Key::K, Cmd::Palette);
            hit(CMD, Key::B, Cmd::BaseMenu);
            hit(CMD, Key::E, Cmd::ToggleEdit);
            hit(CMD, Key::S, Cmd::Save);
            hit(CMD, Key::L, Cmd::AskAi);
            hit(CMD, Key::G, Cmd::GotoLine);
        });
        for c in cmds {
            self.run_cmd(c);
        }
        if typing || self.palette.open.is_some() {
            return;
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
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = m.space[1];
            let repo_name = self.repo.as_ref().map(|r| short_repo_name(&r.root)).unwrap_or_else(|| short_repo_name(&self.repo_arg));
            ui.label(RichText::new(repo_name).font(widgets::heading_font(&th)).color(th.fg()));
            if !self.head_name.is_empty() {
                widgets::badge(ui, &th, &self.head_name, th.muted());
            }
            ui.add_space(m.space[1]);
            // Base chip: always visible, shows the comparison base in effect.
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
                if widgets::icon_button(ui, &th, Icon::Settings, "Settings").clicked() {
                    self.settings_open = !self.settings_open;
                }
                let mut v = usize::from(self.settings.split_view);
                if widgets::segmented(ui, &th, &["Unified", "Side by side"], &mut v) {
                    self.settings.split_view = v == 1;
                    self.settings_dirty_at = Some(Instant::now());
                }
                if ui.ctx().screen_rect().width() >= 1000.0 && widgets::Btn::new("Ask AI").icon(Icon::Spark).kind(BtnKind::Primary).enabled(self.selection.is_some()).show(ui, &th).on_hover_text("Ctrl+L").clicked() {
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
                ui.label(RichText::new(self.fonts_note.clone()).font(f.clone()).color(th.muted()));
            });
        });
    }

    // ------------------------------------------------------------ left rail

    fn rail_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        ui.add_space(m.space[1]);
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            let mut i = match self.rail {
                RailTab::Commits => 0,
                RailTab::Search => 1,
                RailTab::Files => 2,
                RailTab::Runs => 3,
            };
            let badge = self.ag.runs.badge();
            let runs_label = if badge > 0 { format!("Runs {badge}") } else { "Runs".to_string() };
            if widgets::segmented_gated(ui, &th, &["Commits", "Search", "Files", &runs_label], &[], &mut i, 8.0) {
                self.rail = [RailTab::Commits, RailTab::Search, RailTab::Files, RailTab::Runs][i];
                if self.rail == RailTab::Search {
                    self.search.focus = true;
                }
                if self.rail == RailTab::Runs {
                    self.ag_open_runs();
                } else if self.centre == Centre::Run {
                    self.centre = Centre::Diff;
                }
            }
        });
        ui.add_space(m.space[1]);
        match self.rail {
            RailTab::Commits => self.commits_panel(ui),
            RailTab::Search => self.search_panel(ui),
            RailTab::Files => self.files_panel(ui),
            RailTab::Runs => self.runs_panel(ui),
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
        // Working tree row.
        let wt_sel = matches!(self.target, Some(TargetSel::Worktree));
        let (resp, rect) = widgets::list_row(ui, &th, 32.0, wt_sel);
        widgets::paint_text_fit(ui, rect, "Working tree changes", widgets::ui_font(&th), th.fg());
        if resp.clicked() {
            self.select_worktree();
        }

        let files_h = if self.cur.is_some() { 220.0 } else { 120.0 };
        egui::TopBottomPanel::bottom("rail_files").resizable(true).default_height(files_h).height_range(96.0..=420.0).frame(egui::Frame::new().fill(th.bg()).stroke(egui::Stroke::new(1.0, th.border()))).show_inside(ui, |ui| self.changed_files_ui(ui));

        // Commit list.
        let row_h = th.m().commit_row_height;
        if let Loadable::Failed(e) = &self.open_state {
            widgets::banner(ui, &th, Level::Error, &format!("Cannot open repository: {e}"), None, false);
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
            let q = !self.commit_filter_applied.0.trim().is_empty();
            widgets::empty_state(ui, &th, if q { "No matching commits" } else { "No commits yet" }, if q { "Try another term, or switch between Message and Author." } else { "Make a commit and it will show up here." });
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
            Loadable::Ready(set) => {
                let (add, del): (u32, u32) = set.files.iter().fold((0, 0), |a, f| (a.0 + f.add, a.1 + f.del));
                ui.horizontal(|ui| {
                    ui.add_space(m.space[2]);
                    ui.label(RichText::new(format!("{} {}", set.files.len(), if set.files.len() == 1 { "file" } else { "files" })).font(widgets::ui_font(&th)).color(th.fg()));
                    ui.label(RichText::new(format!("+{add}")).font(widgets::mono_font(&th)).color(th.c(th.p().diff.add.fg)));
                    ui.label(RichText::new(format!("−{del}")).font(widgets::mono_font(&th)).color(th.c(th.p().diff.del.fg)));
                });
                ui.add_space(m.space[0]);
                if set.files.is_empty() {
                    widgets::empty_state(ui, &th, "No changes", "This comparison has no differences.");
                    return;
                }
                let mut pick: Option<String> = None;
                let row_h = 28.0;
                let sel = self.file_sel.clone();
                ScrollArea::vertical().id_salt("files_changed").auto_shrink([false, false]).show_rows(ui, row_h, set.files.len(), |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for i in range {
                        let f = &set.files[i];
                        let (resp, rect) = widgets::list_row(ui, &th, row_h, sel.as_deref() == Some(f.path.as_str()));
                        let (letter, col) = status_style(&th, f.status);
                        let lr = Rect::from_min_size(pos2(rect.min.x, rect.min.y), vec2(14.0, row_h));
                        ui.painter().text(lr.center(), egui::Align2::CENTER_CENTER, letter, widgets::mono_font(&th), col);
                        let stat = if f.binary { "bin".to_string() } else { format!("+{} −{}", f.add, f.del) };
                        let sw = widgets::paint_text_right(ui, rect, &stat, widgets::small_font(&th), th.muted());
                        widgets::paint_text_fit(ui, Rect::from_min_max(pos2(rect.min.x + 22.0, rect.min.y), pos2(rect.max.x - sw - 8.0, rect.max.y)), &f.path, widgets::ui_font(&th), th.fg());
                        if resp.clicked() {
                            pick = Some(f.path.clone());
                        }
                        resp.on_hover_text(&f.path);
                    }
                });
                if let Some(p) = pick {
                    self.centre = Centre::Diff;
                    self.open_file_diff(&p);
                }
            }
        }
    }

    fn search_panel(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            let w = ui.available_width() - m.space[2];
            let te = egui::TextEdit::singleline(&mut self.search.query).hint_text("Search file contents").desired_width(w).margin(vec2(8.0, 6.0));
            let r = ui.add_sized([w, m.control_height], te);
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
        // Flatten for virtualised rendering: group header + its hits.
        let mut flat: Vec<(usize, Option<usize>)> = Vec::new();
        for (gi, g) in self.search.groups.iter().enumerate() {
            flat.push((gi, None));
            if !g.collapsed {
                flat.extend((0..g.hits.len()).map(|h| (gi, Some(h))));
            }
        }
        let mut toggle: Option<usize> = None;
        let mut open: Option<(usize, usize)> = None;
        let sel = self.search.selected;
        let row_h = 26.0;
        ScrollArea::vertical().id_salt("search_list").auto_shrink([false, false]).show_rows(ui, row_h, flat.len(), |ui, range| {
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

    /// Opens `path`: diff when the current comparison touches it, otherwise the editor.
    pub fn open_file_diff_or_editor(&mut self, path: &str) {
        let in_diff = self.diffset.ready().is_some_and(|s| s.files.iter().any(|f| f.path == path));
        if in_diff {
            self.centre = Centre::Diff;
            self.open_file_diff(path);
        } else {
            self.open_editor(path, None);
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
                if opt(ui, "Working tree", "uncommitted", cur_mode == Some(BaseMode::WorkingTree)) {
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
                            for (i, (c, label, key)) in items.iter().enumerate() {
                                let (resp, rect) = widgets::list_row(ui, &th, 30.0, i == self.palette.sel);
                                let kw = if key.is_empty() { 0.0 } else { widgets::paint_text_right(ui, rect, key, widgets::small_font(&th), th.muted()) };
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

    // ------------------------------------------------------------ settings

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let th = self.th.clone();
        let m = th.m().clone();
        let mut open = true;
        let mut changed_theme = false;
        let mut changed_settings = false;
        let mut reset = false;
        egui::Window::new("Settings").open(&mut open).collapsible(false).resizable(true).default_width(380.0).default_pos(pos2(ctx.screen_rect().right() - 420.0, 64.0)).frame(egui::Frame::new().fill(th.raised()).stroke(egui::Stroke::new(1.0, th.border())).corner_radius(th.radius()).inner_margin(egui::Margin::same(16)).shadow(egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(110) })).show(ctx, |ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 10.0);
            let sv = ui.visuals_mut();
            sv.widgets.inactive.bg_fill = th.border();
            sv.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, th.muted());
            sv.widgets.hovered.bg_fill = th.border();
            sv.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, th.fg());
            sv.widgets.active.fg_stroke = egui::Stroke::new(1.0, th.accent());
            sv.selection.bg_fill = th.accent();
            let win_h = (ctx.screen_rect().height() - 220.0).clamp(240.0, 720.0);
            ScrollArea::vertical().max_height(win_h).min_scrolled_height(win_h).auto_shrink([false, false]).show(ui, |ui| {
                widgets::heading(ui, &th, "Appearance");
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Theme").color(th.muted()));
                    let mut i = usize::from(!self.th.file.is_dark());
                    if widgets::segmented(ui, &th, &["Dark", "Light"], &mut i) {
                        self.th.file.active = if i == 0 { theme::Mode::Dark } else { theme::Mode::Light };
                        changed_theme = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("UI font size").color(th.muted()));
                    if ui.add(egui::Slider::new(&mut self.settings.ui_font_size, 11.0..=18.0).step_by(1.0)).changed() {
                        changed_settings = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Code font size").color(th.muted()));
                    if ui.add(egui::Slider::new(&mut self.settings.code_font_size, 10.0..=20.0).step_by(1.0)).changed() {
                        changed_settings = true;
                    }
                });
                ui.add_space(4.0);
                widgets::heading(ui, &th, "Colors");
                let mut edit = |ui: &mut Ui, label: &str, h: &mut Hex| -> bool {
                    let mut c = h.0;
                    let mut ch = false;
                    ui.horizontal(|ui| {
                        ui.set_min_height(m.control_height);
                        if ui.color_edit_button_srgba(&mut c).changed() {
                            *h = Hex(Color32::from_rgb(c.r(), c.g(), c.b()));
                            ch = true;
                        }
                        ui.label(RichText::new(label).color(th.fg()));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(h.to_hex()).font(widgets::mono_font(&th)).color(th.muted()));
                        });
                    });
                    ch
                };
                {
                    let p = self.th.file.palette_mut();
                    changed_theme |= edit(ui, "Background", &mut p.bg.base);
                    changed_theme |= edit(ui, "Raised surface", &mut p.bg.raised);
                    changed_theme |= edit(ui, "Sunken surface", &mut p.bg.sunken);
                    changed_theme |= edit(ui, "Text", &mut p.fg.primary);
                    changed_theme |= edit(ui, "Muted text", &mut p.fg.muted);
                    changed_theme |= edit(ui, "Accent", &mut p.accent);
                    changed_theme |= edit(ui, "Border", &mut p.border);
                    changed_theme |= edit(ui, "Added line", &mut p.diff.add.bg);
                    changed_theme |= edit(ui, "Added word", &mut p.diff.add.word);
                    changed_theme |= edit(ui, "Removed line", &mut p.diff.del.bg);
                    changed_theme |= edit(ui, "Removed word", &mut p.diff.del.word);
                    changed_theme |= edit(ui, "AI badge", &mut p.ai_badge);
                    changed_theme |= edit(ui, "Warning", &mut p.warn);
                }
                let ratio = theme::contrast(self.th.fg(), self.th.bg());
                let (lvl, msg) = if ratio >= 4.5 { (Level::Info, format!("Text contrast {ratio:.1}:1")) } else { (Level::Warn, format!("Text contrast {ratio:.1}:1 is below the 4.5:1 minimum for readable text.")) };
                widgets::banner(ui, &th, lvl, &msg, None, false);
                ui.add_space(4.0);
                self.accounts_section(ui);
                ui.add_space(4.0);
                widgets::heading(ui, &th, "Behaviour");
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut self.settings.ask_preview, "Preview the prompt before asking AI").changed() {
                        changed_settings = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Ask AI timeout (s)").color(th.muted()));
                    if ui.add(egui::DragValue::new(&mut self.settings.ask_timeout_secs).range(10..=1800)).changed() {
                        changed_settings = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Tab width").color(th.muted()));
                    if ui.add(egui::DragValue::new(&mut self.settings.tab_width).range(1..=8)).changed() {
                        changed_settings = true;
                    }
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if Btn::new("Reset colors").show(ui, &th).clicked() {
                        reset = true;
                    }
                    let p = theme::theme_path();
                    ui.label(RichText::new(format!("Saved to {}", p.display())).font(widgets::small_font(&th)).color(th.muted()));
                });
            });
        });
        if reset {
            let mode = self.th.file.active;
            self.th.file = theme::ThemeFile { active: mode, ..Default::default() };
            changed_theme = true;
        }
        if changed_settings {
            self.settings = std::mem::take(&mut self.settings).sanitize();
            self.th.ui_font = self.settings.ui_font_size;
            self.th.code_font = self.settings.code_font_size;
            changed_theme = true;
            self.diff_cache_invalidate();
        }
        if changed_theme {
            self.th.apply(ctx);
            self.settings_dirty_at = Some(Instant::now());
        }
        if !open {
            self.settings_open = false;
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
