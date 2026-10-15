//! Settings as a full page: replaces the three-pane area (header and status bar stay). Nothing
//! here owns state other than the chosen section; every control edits `App.settings` / the theme
//! live and persistence stays debounced through `settings_dirty_at`.

use crate::app::App;
use crate::settings::{Startup, ViewMode};
use crate::theme::{self, Hex};
use crate::widgets::{self, Btn, Level};
use egui::{vec2, Align, Color32, Layout, Rect, RichText, ScrollArea, Ui};
use std::time::Instant;

/// Widest the content column grows; wider windows get margins instead of stretched rows.
const COLUMN_MAX: f32 = 720.0;
/// Below this width the section list collapses into a dropdown above the content.
const NAV_BREAKPOINT: f32 = 760.0;
const NAV_WIDTH: f32 = 200.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Section {
    #[default]
    General,
    Appearance,
    Agents,
    Remotes,
    Resources,
    Shortcuts,
    About,
}

impl Section {
    pub const ALL: [Section; 7] = [Section::General, Section::Appearance, Section::Agents, Section::Remotes, Section::Resources, Section::Shortcuts, Section::About];

    pub fn label(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Appearance => "Appearance",
            Section::Agents => "Agents & Accounts",
            Section::Remotes => "Remotes",
            Section::Resources => "Resources & Limits",
            Section::Shortcuts => "Shortcuts",
            Section::About => "About",
        }
    }

    pub fn parse(s: &str) -> Option<Section> {
        let s = s.to_ascii_lowercase();
        Section::ALL.into_iter().find(|x| x.label().to_ascii_lowercase().starts_with(&s) && !s.is_empty())
    }
}

/// What a frame of the page changed; applied once after drawing.
#[derive(Default)]
struct Dirty {
    theme: bool,
    settings: bool,
    reset_colors: bool,
    limits: bool,
}

pub fn open_path(path: &std::path::Path) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let mut c = std::process::Command::new(opener);
    c.arg(path);
    crate::proc::spawn_reaped("ct-open-path", c, std::time::Duration::from_secs(15));
}

pub(crate) fn row(ui: &mut Ui, th: &theme::Theme, label: &str, add: impl FnOnce(&mut Ui)) {
    let narrow = ui.available_width() < 420.0;
    let body = |ui: &mut Ui| {
        let lw = if narrow { ui.available_width() } else { 220.0 };
        let (r, _) = ui.allocate_exact_size(vec2(lw, th.m().control_height), egui::Sense::hover());
        let g = widgets::fit(ui, label, widgets::ui_font(th), th.fg(), lw);
        ui.painter().galley(egui::pos2(r.min.x, r.center().y - g.size().y / 2.0), g, th.fg());
    };
    if narrow {
        ui.vertical(|ui| {
            body(ui);
            add(ui);
        });
    } else {
        ui.horizontal(|ui| {
            body(ui);
            add(ui);
        });
    }
}

pub(crate) fn hint(ui: &mut Ui, th: &theme::Theme, text: &str) {
    ui.label(RichText::new(text).font(widgets::small_font(th)).color(th.muted()));
}

impl App {
    pub fn settings_open_section(&mut self, s: Section) {
        self.settings_section = s;
        self.settings_open = true;
    }

    /// Left navigation (wide windows). Drawn as a side panel by the caller.
    pub fn settings_nav_panel(&mut self, ctx: &egui::Context) {
        let th = self.th.clone();
        egui::SidePanel::left("settings_nav").exact_width(NAV_WIDTH).resizable(false).frame(egui::Frame::new().fill(th.bg()).stroke(egui::Stroke::new(1.0, th.border())).inner_margin(egui::Margin::same(12))).show(ctx, |ui| {
            self.settings_back_button(ui);
            ui.add_space(12.0);
            for s in Section::ALL {
                let sel = self.settings_section == s;
                if Btn::new(s.label()).selected(sel).min_width(ui.available_width()).show(ui, &th).clicked() {
                    self.settings_section = s;
                }
            }
        });
    }

    fn settings_back_button(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        if Btn::new("← Back").show(ui, &th).on_hover_text("Back to the repository (Esc)").clicked() {
            self.settings_open = false;
        }
    }

    pub fn settings_wide(ctx: &egui::Context) -> bool {
        ctx.screen_rect().width() >= NAV_BREAKPOINT
    }

    /// The page body: centred column, scrollable. Call inside the central panel.
    pub fn settings_page(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let wide = Self::settings_wide(ui.ctx());
        let full = ui.max_rect();
        let w = (full.width() - 32.0).clamp(240.0, COLUMN_MAX);
        let col = Rect::from_min_size(egui::pos2(full.center().x - w / 2.0, full.min.y + 16.0), vec2(w, (full.height() - 16.0).max(80.0)));
        let mut dirty = Dirty::default();
        ui.scope_builder(egui::UiBuilder::new().max_rect(col), |ui| {
            ui.set_width(w);
            ui.spacing_mut().item_spacing = vec2(8.0, 12.0);
            let sv = ui.visuals_mut();
            sv.widgets.inactive.bg_fill = th.border();
            sv.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, th.muted());
            sv.widgets.hovered.bg_fill = th.border();
            sv.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, th.fg());
            sv.widgets.active.fg_stroke = egui::Stroke::new(1.0, th.accent());
            sv.selection.bg_fill = th.accent();
            if !wide {
                ui.horizontal(|ui| {
                    self.settings_back_button(ui);
                    let mut cur = self.settings_section;
                    egui::ComboBox::from_id_salt("settings_section").selected_text(cur.label()).width(ui.available_width().min(240.0)).show_ui(ui, |ui| {
                        for s in Section::ALL {
                            ui.selectable_value(&mut cur, s, s.label());
                        }
                    });
                    self.settings_section = cur;
                });
            }
            ScrollArea::vertical().id_salt(("settings_scroll", self.settings_section as u8)).auto_shrink([false, false]).show(ui, |ui| {
                ui.set_width(w);
                ui.label(RichText::new(self.settings_section.label()).font(widgets::heading_font(&th)).color(th.fg()));
                match self.settings_section {
                    Section::General => self.sec_general(ui, &mut dirty),
                    Section::Appearance => self.sec_appearance(ui, &mut dirty),
                    Section::Agents => self.sec_agents(ui, &mut dirty),
                    Section::Remotes => self.sec_remotes(ui),
                    Section::Resources => self.sec_resources(ui, &mut dirty),
                    Section::Shortcuts => self.sec_shortcuts(ui),
                    Section::About => self.sec_about(ui),
                }
                ui.add_space(24.0);
            });
        });
        self.settings_apply(ui.ctx(), dirty);
    }

    fn settings_apply(&mut self, ctx: &egui::Context, d: Dirty) {
        let (mut theme_changed, mut persist) = (d.theme, d.theme);
        if d.reset_colors {
            let mode = self.th.file.active;
            self.th.file = theme::ThemeFile { active: mode, ..Default::default() };
            theme_changed = true;
            persist = true;
        }
        if d.settings {
            self.settings = std::mem::take(&mut self.settings).sanitize();
            self.th.ui_font = self.settings.ui_font_size;
            self.th.code_font = self.settings.code_font_size;
            theme_changed = true;
            persist = true;
            self.diff_cache_invalidate();
        }
        if d.limits {
            self.settings = std::mem::take(&mut self.settings).sanitize();
            self.ag_apply_limits();
            persist = true;
        }
        if theme_changed {
            self.th.apply(ctx);
        }
        if persist {
            self.settings_dirty_at = Some(Instant::now());
        }
    }

    // ------------------------------------------------------------ General

    fn sec_general(&mut self, ui: &mut Ui, d: &mut Dirty) {
        let th = self.th.clone();
        row(ui, &th, "Default view", |ui| {
            let mut i = match self.settings.view_mode {
                ViewMode::Diff => 0,
                ViewMode::Blame => 1,
                ViewMode::Edit => 2,
            };
            if widgets::segmented(ui, &th, &["Diff", "Blame", "Edit"], &mut i) {
                self.set_view_pref([ViewMode::Diff, ViewMode::Blame, ViewMode::Edit][i]);
            }
        });
        hint(ui, &th, "The mode of the main pane; picking a file or commit never changes it.");
        row(ui, &th, "Diff layout", |ui| {
            let mut i = usize::from(self.settings.split_view);
            if widgets::segmented(ui, &th, &["Unified", "Side by side"], &mut i) {
                self.settings.split_view = i == 1;
                d.settings = true;
            }
        });
        row(ui, &th, "Tab width", |ui| {
            if ui.add(egui::DragValue::new(&mut self.settings.tab_width).range(1..=16)).changed() {
                d.settings = true;
            }
        });
        row(ui, &th, "Wrap long lines", |ui| {
            if ui.checkbox(&mut self.settings.wrap, "In the editor").changed() {
                d.settings = true;
            }
        });
        row(ui, &th, "Rail layout", |ui| {
            if Btn::new("Reset files / commits split").enabled(self.settings.files_split.is_some()).show(ui, &th).clicked() {
                self.settings.files_split = None;
                d.settings = true;
            }
        });
        row(ui, &th, "On startup", |ui| {
            let mut i = usize::from(self.settings.startup == Startup::Picker);
            if widgets::segmented(ui, &th, &["Reopen last project", "Project picker"], &mut i) {
                self.settings.startup = if i == 0 { Startup::Last } else { Startup::Picker };
                d.settings = true;
            }
        });
        hint(ui, &th, "Applies the next time CodeTrail starts without a path argument.");
    }

    // ------------------------------------------------------------ Appearance

    fn sec_appearance(&mut self, ui: &mut Ui, d: &mut Dirty) {
        let th = self.th.clone();
        let m = th.m().clone();
        row(ui, &th, "Theme", |ui| {
            let mut i = usize::from(!self.th.file.is_dark());
            if widgets::segmented(ui, &th, &["Dark", "Light"], &mut i) {
                self.th.file.active = if i == 0 { theme::Mode::Dark } else { theme::Mode::Light };
                d.theme = true;
            }
        });
        let z = self.settings.zoom;
        let mut nz = z;
        row(ui, &th, "Zoom", |ui| {
            ui.horizontal(|ui| {
                if Btn::new("−").enabled(z > 0.5).show(ui, &th).clicked() {
                    nz = z - 0.1;
                }
                ui.label(RichText::new(format!("{:.0}%", z * 100.0)).color(th.fg()));
                if Btn::new("+").enabled(z < 3.0).show(ui, &th).clicked() {
                    nz = z + 0.1;
                }
                if Btn::new("Reset").enabled(z != 1.0).show(ui, &th).clicked() {
                    nz = 1.0;
                }
            });
        });
        if nz != z {
            self.settings.zoom = crate::settings::clamp_zoom(nz);
            ui.ctx().set_zoom_factor(self.settings.zoom);
            self.settings_dirty_at = Some(Instant::now());
        }
        hint(ui, &th, "Scales text and the whole interface. Ctrl+Plus / Ctrl+Minus / Ctrl+0 work anywhere.");
        row(ui, &th, "UI font size", |ui| {
            if ui.add(egui::Slider::new(&mut self.settings.ui_font_size, 11.0..=18.0).step_by(1.0)).changed() {
                d.settings = true;
            }
        });
        row(ui, &th, "Code font size", |ui| {
            if ui.add(egui::Slider::new(&mut self.settings.code_font_size, 10.0..=20.0).step_by(1.0)).changed() {
                d.settings = true;
            }
        });
        if !self.fonts_note.is_empty() {
            hint(ui, &th, &self.fonts_note);
        }
        widgets::heading(ui, &th, "Colors");
        let edit = |ui: &mut Ui, label: &str, h: &mut Hex| -> bool {
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
            d.theme |= edit(ui, "Background", &mut p.bg.base);
            d.theme |= edit(ui, "Raised surface", &mut p.bg.raised);
            d.theme |= edit(ui, "Sunken surface", &mut p.bg.sunken);
            d.theme |= edit(ui, "Text", &mut p.fg.primary);
            d.theme |= edit(ui, "Muted text", &mut p.fg.muted);
            d.theme |= edit(ui, "Accent", &mut p.accent);
            d.theme |= edit(ui, "Border", &mut p.border);
            d.theme |= edit(ui, "Added line", &mut p.diff.add.bg);
            d.theme |= edit(ui, "Added word", &mut p.diff.add.word);
            d.theme |= edit(ui, "Removed line", &mut p.diff.del.bg);
            d.theme |= edit(ui, "Removed word", &mut p.diff.del.word);
            d.theme |= edit(ui, "AI badge", &mut p.ai_badge);
            d.theme |= edit(ui, "Warning", &mut p.warn);
        }
        let ratio = theme::contrast(self.th.fg(), self.th.bg());
        let (lvl, msg) = if ratio >= 4.5 { (Level::Info, format!("Text contrast {ratio:.1}:1")) } else { (Level::Warn, format!("Text contrast {ratio:.1}:1 is below the 4.5:1 minimum for readable text.")) };
        widgets::banner(ui, &th, lvl, &msg, None, false);
        self.preview_swatches(ui);
        ui.horizontal(|ui| {
            if Btn::new("Reset colors").show(ui, &th).clicked() {
                d.reset_colors = true;
            }
            hint(ui, &th, &format!("Saved to {}", theme::theme_path().display()));
        });
    }

    /// A small diff excerpt painted with the live palette.
    fn preview_swatches(&self, ui: &mut Ui) {
        let th = self.th.clone();
        let mono = widgets::mono_font(&th);
        let rows = [("  fn total(items: &[u32]) -> u32 {", None, th.fg()), ("-     items.iter().sum()", Some(th.c(th.p().diff.del.bg)), th.c(th.p().diff.del.fg)), ("+     items.iter().copied().sum()", Some(th.c(th.p().diff.add.bg)), th.c(th.p().diff.add.fg)), ("  }", None, th.muted())];
        egui::Frame::new().fill(th.sunken()).stroke(egui::Stroke::new(1.0, th.border())).corner_radius(th.radius()).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (txt, bg, fg) in rows {
                let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), th.code_font * 1.6), egui::Sense::hover());
                if let Some(b) = bg {
                    ui.painter().rect_filled(r, 0.0, b);
                }
                let g = widgets::fit(ui, txt, mono.clone(), fg, r.width() - 16.0);
                ui.painter().galley(egui::pos2(r.min.x + 8.0, r.center().y - g.size().y / 2.0), g, fg);
            }
        });
    }

    // ------------------------------------------------------------ Agents

    fn sec_agents(&mut self, ui: &mut Ui, d: &mut Dirty) {
        let th = self.th.clone();
        widgets::heading(ui, &th, "Default model");
        self.model_picker(ui, "settings");
        ui.add_space(8.0);
        self.accounts_section(ui);
        widgets::heading(ui, &th, "Ask AI");
        row(ui, &th, "Preview the prompt", |ui| {
            if ui.checkbox(&mut self.settings.ask_preview, "Before asking").changed() {
                d.settings = true;
            }
        });
        row(ui, &th, "Timeout (s)", |ui| {
            if ui.add(egui::DragValue::new(&mut self.settings.ask_timeout_secs).range(5..=3600)).changed() {
                d.settings = true;
            }
        });
    }

    // ------------------------------------------------------------ Resources

    fn sec_resources(&mut self, ui: &mut Ui, d: &mut Dirty) {
        let th = self.th.clone();
        widgets::heading(ui, &th, "Agent runs");
        row(ui, &th, "Concurrent runs", |ui| {
            let mut n = self.settings.agent.max_concurrent;
            if ui.add(egui::DragValue::new(&mut n).range(1..=8)).changed() {
                self.ag_set_max_concurrent(n);
            }
        });
        row(ui, &th, "Memory per run (MiB)", |ui| {
            if ui.add(egui::DragValue::new(&mut self.settings.agent.max_rss_mb).range(0..=262_144).speed(64)).changed() {
                d.limits = true;
            }
            hint(ui, &th, "0 = unlimited");
        });
        row(ui, &th, "Runtime per run (min)", |ui| {
            if ui.add(egui::DragValue::new(&mut self.settings.agent.max_runtime_min).range(0..=1440)).changed() {
                d.limits = true;
            }
            hint(ui, &th, "0 = unlimited");
        });
        row(ui, &th, "Total memory budget (MiB)", |ui| {
            if ui.add(egui::DragValue::new(&mut self.settings.agent.max_total_rss_mb).range(512..=262_144).speed(64)).changed() {
                d.limits = true;
            }
        });
        hint(ui, &th, "A run over its limit for three samples in a row is stopped; the budget stops the largest run.");
        widgets::heading(ui, &th, "Git");
        row(ui, &th, "Command timeout (s)", |ui| {
            if ui.add(egui::DragValue::new(&mut self.settings.git_timeout_secs).range(1..=600)).changed() {
                d.settings = true;
            }
        });
        row(ui, &th, "Parallel git processes", |ui| {
            ui.label(RichText::new(format!("{} (fixed)", ct_core::MAX_GIT_PROCS)).color(th.muted()));
        });
        widgets::heading(ui, &th, "Monitor");
        row(ui, &th, "Status bar", |ui| {
            if ui.checkbox(&mut self.settings.status_monitor, "Show memory and CPU").changed() {
                self.res.sample = None;
                d.settings = true;
            }
        });
        hint(ui, &th, "Sampled every 2 s while the window is focused; nothing while it is in the background.");
    }

    // ------------------------------------------------------------ Shortcuts

    fn sec_shortcuts(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let mut rows: Vec<(String, String)> = crate::ui::COMMANDS.iter().filter(|(_, _, k)| !k.is_empty()).map(|(_, n, k)| (n.to_string(), k.to_string())).collect();
        rows.push(("Settings".into(), "Ctrl+,".into()));
        rows.push(("Zoom in / out / reset".into(), "Ctrl+Plus / Ctrl+Minus / Ctrl+0".into()));
        rows.push(("Next / previous hunk".into(), "] / [".into()));
        rows.push(("Next / previous commit".into(), "J / K".into()));
        rows.push(("History: step entries (Blame, Follow)".into(), "↑ / ↓".into()));
        rows.push(("Back (settings, history view, popovers)".into(), "Esc".into()));
        hint(ui, &th, "On macOS Ctrl is Cmd. Shortcuts are fixed.");
        egui::Frame::new().fill(th.sunken()).stroke(egui::Stroke::new(1.0, th.border())).corner_radius(th.radius()).inner_margin(egui::Margin::symmetric(12, 4)).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (name, keys) in &rows {
                ui.horizontal(|ui| {
                    ui.set_min_height(th.m().control_height);
                    ui.label(RichText::new(name).color(th.fg()));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(keys).font(widgets::mono_font(&th)).color(th.muted()));
                    });
                });
            }
        });
    }

    // ------------------------------------------------------------ About

    fn sec_about(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let cfg = crate::settings::Settings::path();
        let data = ct_runs::default_data_dir();
        row(ui, &th, "Version", |ui| {
            ui.label(RichText::new(env!("CARGO_PKG_VERSION")).font(widgets::mono_font(&th)).color(th.fg()));
        });
        row(ui, &th, "Build", |ui| {
            ui.label(RichText::new(format!("{} · {}", env!("CT_GIT_SHA"), env!("CT_BUILD_DATE"))).font(widgets::mono_font(&th)).color(th.fg()));
        });
        for (label, path) in [("Config file", cfg.clone()), ("Theme file", theme::theme_path()), ("Data directory", data)] {
            let dir = if path.is_dir() { path.clone() } else { path.parent().map(std::path::Path::to_path_buf).unwrap_or_default() };
            ui.label(RichText::new(label).color(th.fg()));
            ui.horizontal(|ui| {
                let g = widgets::fit(ui, &path.display().to_string(), widgets::mono_font(&th), th.muted(), (ui.available_width() - 130.0).max(80.0));
                ui.label(g).on_hover_text(path.display().to_string());
                if Btn::new("Open folder").compact().show(ui, &th).clicked() {
                    open_path(&dir);
                }
            });
        }
        widgets::heading(ui, &th, "Licenses");
        hint(ui, &th, "CodeTrail is built from open-source crates; each keeps its own license (MIT, Apache-2.0, and similar).");
        if Btn::new("Open dependency list (Cargo.lock)").compact().show(ui, &th).clicked() {
            open_path(&std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::settings_window_tests::test_app;

    fn run(ctx: &egui::Context, app: &mut App, size: egui::Vec2) -> Vec<Rect> {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::pos2(0.0, 0.0), size)), ..Default::default() };
        let mut used = Vec::new();
        let _ = ctx.run(input, |ctx| {
            if App::settings_wide(ctx) {
                app.settings_nav_panel(ctx);
            }
            egui::CentralPanel::default().show(ctx, |ui| {
                app.settings_page(ui);
                used.push(ui.min_rect());
            });
        });
        used
    }

    #[test]
    fn section_parse_and_labels() {
        assert_eq!(Section::parse("appear"), Some(Section::Appearance));
        assert_eq!(Section::parse("agents"), Some(Section::Agents));
        assert_eq!(Section::parse(""), None);
        assert_eq!(Section::ALL.len(), 7);
        assert_eq!(Section::parse("remo"), Some(Section::Remotes));
    }

    #[test]
    fn page_does_not_overflow_or_grow_per_frame_in_any_section() {
        for size in [vec2(1440.0, 900.0), vec2(720.0, 760.0), vec2(640.0, 420.0)] {
            for s in Section::ALL {
                let ctx = egui::Context::default();
                let mut app = test_app(&ctx);
                app.settings_open_section(s);
                let mut rects = Vec::new();
                for _ in 0..12 {
                    rects.push(run(&ctx, &mut app, size)[0]);
                }
                assert_eq!(rects[5], rects[11], "{s:?} at {size:?}: layout must settle");
                let r = rects[11];
                assert!(r.width() <= size.x + 0.5 && r.height() <= size.y + 0.5, "{s:?} at {size:?}: used {r:?}");
            }
        }
    }

    #[test]
    fn back_keeps_state_and_remembers_the_section() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx);
        app.rail = crate::app::RailTab::Search;
        app.settings_open_section(Section::Resources);
        run(&ctx, &mut app, vec2(1000.0, 700.0));
        app.settings_open = false;
        assert_eq!(app.rail, crate::app::RailTab::Search);
        app.settings_open = true;
        assert_eq!(app.settings_section, Section::Resources);
    }

    #[test]
    fn every_page_setting_round_trips_through_config_toml() {
        let mut s = crate::settings::Settings::default();
        s.view_mode = ViewMode::Blame;
        s.split_view = true;
        s.tab_width = 2;
        s.wrap = true;
        s.startup = Startup::Picker;
        s.status_monitor = false;
        s.ui_font_size = 15.0;
        s.code_font_size = 12.0;
        s.ask_preview = false;
        s.ask_timeout_secs = 99;
        s.git_timeout_secs = 12;
        s.files_split = Some(0.4);
        s.agent.max_concurrent = 5;
        s.agent.max_rss_mb = 0;
        s.agent.max_runtime_min = 30;
        s.agent.max_total_rss_mb = 9000;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        s.save(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        for key in ["view_mode", "split_view", "tab_width", "wrap", "startup", "status_monitor", "ui_font_size", "code_font_size", "ask_preview", "ask_timeout_secs", "git_timeout_secs", "files_split", "max_concurrent", "max_rss_mb", "max_runtime_min", "max_total_rss_mb"] {
            assert!(text.contains(key), "config.toml keeps the key {key}");
        }
        assert_eq!(crate::settings::Settings::load(&p), (s, None));
    }

    #[test]
    fn run_limits_are_clamped() {
        let mut s = crate::settings::Settings::default();
        s.agent.max_total_rss_mb = 1;
        s.agent.max_runtime_min = 999_999;
        let s = s.sanitize();
        assert_eq!(s.agent.max_total_rss_mb, 512);
        assert_eq!(s.agent.max_runtime_min, 1440);
    }

    #[test]
    fn changing_limits_reaches_the_run_service_and_persists_debounced() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx);
        app.settings.agent.max_rss_mb = 1024;
        app.settings_apply(&ctx, Dirty { limits: true, ..Default::default() });
        assert!(app.settings_dirty_at.is_some(), "persisted through the debounce, not per frame");
        assert_eq!(app.settings.agent.max_rss_mb, 1024);
    }

    #[test]
    fn disabled_status_monitor_hides_the_sample() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx);
        app.settings.status_monitor = false;
        app.res.sample = Some(crate::resmon::Sample { app_rss_kb: 1, child_rss_kb: 0, children: 0, cpu_pct: 0.0, ticks: 0 });
        let mut shown = true;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let before = ui.min_rect();
                app.resource_label(ui, &app.th.clone(), widgets::small_font(&app.th));
                shown = ui.min_rect() != before;
            });
        });
        assert!(!shown);
    }
}
