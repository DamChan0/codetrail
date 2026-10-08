//! Agent UI: model picker, Runs rail + run detail, Accounts (Settings), New-run / confirm / quit dialogs.
//! Pure drawing + intent dispatch; state lives in `agentapp::AgentState`.

use crate::agentapp::{now_ms, Confirm};
use crate::agents::*;
use crate::agentvm::*;
use crate::app::*;
use crate::widgets::{self, Btn, BtnKind, Icon, Level};
use egui::{pos2, vec2, Align, Color32, Layout, Rect, RichText, ScrollArea, Ui};
use std::time::Duration;

fn ok_color(th: &crate::theme::Theme) -> Color32 {
    th.p().diff.add.fg.0
}
fn err_color(th: &crate::theme::Theme) -> Color32 {
    th.p().diff.del.fg.0
}

fn state_color(th: &crate::theme::Theme, s: &RunState) -> Color32 {
    match s {
        RunState::Queued => th.muted(),
        RunState::Running => th.accent(),
        RunState::Succeeded => ok_color(th),
        RunState::Failed(_) => err_color(th),
        RunState::Aborted | RunState::Interrupted => th.warn(),
    }
}

fn model_label(m: &ModelSel) -> String {
    format!("{} · {}", m.backend.label(), m.id)
}

impl App {
    // ------------------------------------------------------------ model picker

    /// Compact Backend / Model / Thinking control, shared by the Ask tab and the new-run dialog.
    pub fn model_picker(&mut self, ui: &mut Ui, id: &str) {
        let th = self.th.clone();
        let m = th.m().clone();
        self.ag_ensure_models();
        let cur = self.settings.agent.backend;
        let labels: Vec<&str> = Backend::ALL.iter().map(|b| b.label()).collect();
        let gates: Vec<Option<String>> = Backend::ALL.iter().map(|b| self.ag.gate(*b).err()).collect();
        let mut i = Backend::ALL.iter().position(|b| *b == cur).unwrap_or(0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Agent").color(th.muted()));
            if widgets::segmented_gated(ui, &th, &labels, &gates, &mut i, m.button_pad_x) {
                self.ag_set_backend(Backend::ALL[i]);
            }
        });
        let caps = self.ag.caps(cur);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Model").color(th.muted()));
            match &self.ag.models {
                Loadable::Idle | Loadable::Loading => {
                    ui.label(RichText::new("Loading models…").color(th.muted()));
                }
                Loadable::Failed(e) => {
                    ui.label(RichText::new(format!("Could not load models: {e}")).color(err_color(&th)));
                    if Btn::new("Retry").compact().show(ui, &th).clicked() {
                        self.ag.model_cache.remove(&cur);
                        self.ag_load_models(cur);
                    }
                }
                Loadable::Ready(models) if models.is_empty() => {
                    ui.label(RichText::new("No models — sign in first").color(th.muted()));
                }
                Loadable::Ready(models) => {
                    let models = models.clone();
                    let sel = selected_info(&self.settings.agent, &models).cloned();
                    let text = sel.as_ref().map(|s| s.name.clone()).or_else(|| self.settings.agent.model.clone()).unwrap_or_else(|| "Choose a model".into());
                    let mut pick: Option<ModelInfo> = None;
                    egui::ComboBox::from_id_salt(("model", id)).width(180.0).selected_text(text).show_ui(ui, |ui| {
                        for mi in &models {
                            let label = if mi.provider.is_empty() { mi.name.clone() } else { format!("{} · {}", mi.name, mi.provider) };
                            let on = sel.as_ref().is_some_and(|s| s.id == mi.id && s.provider == mi.provider);
                            if ui.selectable_label(on, label).clicked() {
                                pick = Some(mi.clone());
                            }
                        }
                    });
                    if let Some(p) = pick {
                        self.ag_pick_model(&p);
                    }
                    if caps.thinking_level && sel.as_ref().map_or(true, |s| s.reasoning) {
                        ui.end_row();
                        ui.label(RichText::new("Thinking").color(th.muted()));
                        let cur_t = self.settings.agent.thinking.clone().unwrap_or_else(|| "default".into());
                        let mut pick_t: Option<Option<String>> = None;
                        egui::ComboBox::from_id_salt(("think", id)).width(96.0).selected_text(cur_t).show_ui(ui, |ui| {
                            if ui.selectable_label(self.settings.agent.thinking.is_none(), "default").clicked() {
                                pick_t = Some(None);
                            }
                            for t in THINKING_LEVELS {
                                if ui.selectable_label(self.settings.agent.thinking.as_deref() == Some(t), t).clicked() {
                                    pick_t = Some(Some(t.to_string()));
                                }
                            }
                        });
                        if let Some(t) = pick_t {
                            self.settings.agent.thinking = t;
                            self.settings_dirty_at = Some(std::time::Instant::now());
                        }
                    }
                }
            }
        });
        if let Err(why) = self.ag.gate(cur) {
            ui.label(RichText::new(why).font(widgets::small_font(&th)).color(th.muted()));
        }
    }

    // ------------------------------------------------------------ Runs rail

    pub fn runs_panel(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        self.ag_open_runs();
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            let can = self.repo.is_some() && self.ag.runs_svc.ready().is_some();
            if Btn::new("New run").icon(Icon::Spark).kind(BtnKind::Primary).enabled(can).show(ui, &th).clicked() {
                self.ag_new_run_dialog();
            }
        });
        ui.add_space(m.space[1]);
        match &self.ag.runs_svc {
            Loadable::Idle | Loadable::Loading => {
                widgets::skeleton(ui, &th, 4, 48.0);
                return;
            }
            Loadable::Failed(e) => {
                let e = e.clone();
                ui.horizontal(|ui| {
                    ui.add_space(m.space[2]);
                    ui.vertical(|ui| {
                        widgets::banner(ui, &th, Level::Error, &format!("Runs are unavailable: {e}"), None, false);
                    });
                });
                return;
            }
            Loadable::Ready(_) => {}
        }
        if let Some((l, t)) = self.ag.run_banner.clone() {
            if widgets::banner(ui, &th, l, &t, None, true) == widgets::BannerAction::Dismiss {
                self.ag.run_banner = None;
            }
        }
        let root = self.repo.as_ref().map(|r| r.root.clone());
        let here = |r: &RunInfo| root.as_ref().map_or(true, |p| &r.spec.repo == p);
        let hidden = self.ag.runs.infos.iter().filter(|r| !here(r)).count();
        if !self.ag.runs.infos.iter().any(|r| here(r)) {
            widgets::empty_state(ui, &th, "No runs yet", "A run lets an agent work on its own copy of this repo while you keep reading.");
            if hidden > 0 {
                ui.vertical_centered(|ui| ui.label(RichText::new(format!("{hidden} run(s) belong to other projects.")).font(widgets::small_font(&th)).color(th.muted())));
            }
            return;
        }
        let now = now_ms();
        let mut clicked: Option<String> = None;
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for r in self.ag.runs.infos.iter().filter(|r| here(r)) {
                let sel = self.ag.runs.selected.as_deref() == Some(r.id.as_str()) && self.centre == Centre::Run;
                let (resp, rect) = widgets::list_row(ui, &th, 52.0, sel);
                let col = state_color(&th, &r.state);
                let unseen = self.ag.runs.unseen.contains(&r.id);
                let top = Rect::from_min_max(rect.min + vec2(0.0, 4.0), pos2(rect.max.x, rect.min.y + 24.0));
                let chip_w = widgets::paint_text_right(ui, top, r.state.label(), widgets::small_font(&th), col);
                let title = Rect::from_min_max(top.min, pos2(top.max.x - chip_w - 8.0, top.max.y));
                widgets::paint_text_fit(ui, title, first_line(&r.spec.prompt), widgets::ui_font(&th), th.fg());
                let sub = Rect::from_min_max(pos2(rect.min.x, rect.min.y + 26.0), pos2(rect.max.x, rect.max.y - 4.0));
                let files = if r.files_changed == 1 { "1 file".to_string() } else { format!("{} files", r.files_changed) };
                let line = format!("{} · {} · {}", r.spec.model.id, elapsed_text(r, now), files);
                widgets::paint_text_fit(ui, sub, &line, widgets::small_font(&th), th.muted());
                if unseen {
                    ui.painter().circle_filled(pos2(rect.min.x - 6.0, rect.center().y), 3.0, th.accent());
                }
                if resp.on_hover_text(&r.spec.prompt).clicked() {
                    clicked = Some(r.id.clone());
                }
            }
        });
        if let Some(id) = clicked {
            self.ag_select_run(&id);
        }
        if self.ag.runs.active() > 0 {
            widgets::repaint_if_focused(ui.ctx(), Duration::from_secs(1));
        }
    }

    // ------------------------------------------------------------ run detail (centre)

    pub fn run_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        let Some(info) = self.ag.runs.selected.as_deref().and_then(|id| self.ag.runs.get(id)).cloned() else {
            widgets::empty_state(ui, &th, "No run selected", "Pick a run on the left, or start a new one.");
            return;
        };
        self.ag.runs.select(&info.id);
        let now = now_ms();
        let caps = self.ag.caps(info.spec.model.backend);
        egui::Frame::new().fill(th.raised()).stroke(egui::Stroke::new(1.0, th.border())).inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            ui.horizontal_wrapped(|ui| {
                widgets::badge(ui, &th, info.state.label(), state_color(&th, &info.state));
                ui.label(RichText::new(model_label(&info.spec.model)).color(th.muted()));
                ui.label(RichText::new(format!("{} · base {}", elapsed_text(&info, now), crate::timefmt::short(&info.base_sha))).color(th.muted()));
                let files = if info.files_changed == 1 { "1 file changed".to_string() } else { format!("{} files changed", info.files_changed) };
                ui.label(RichText::new(files).color(th.muted()));
            });
            ui.label(RichText::new(info.spec.prompt.trim()).color(th.fg()));
            if let RunState::Failed(e) = &info.state {
                widgets::banner(ui, &th, Level::Error, e, None, false);
            }
            ui.horizontal(|ui| {
                if info.state.is_active() {
                    if Btn::new("Abort").enabled(caps.abort).show(ui, &th).clicked() {
                        self.ag_abort(&info.id);
                    }
                }
                let done = info.state == RunState::Succeeded;
                if done {
                    if Btn::new("Review").kind(BtnKind::Primary).show(ui, &th).on_hover_text("Open the changes in the diff view").clicked() {
                        self.ag_review(&info.id);
                    }
                    if Btn::new("Apply").show(ui, &th).on_hover_text("Merge this run into your current branch").clicked() {
                        self.ag.confirm = Some(Confirm::Apply(info.id.clone()));
                    }
                }
                if !info.state.is_active() && Btn::new("Discard").show(ui, &th).clicked() {
                    self.ag.confirm = Some(Confirm::Discard(info.id.clone()));
                }
            });
            if let Some((l, t)) = self.ag.run_banner.clone() {
                if widgets::banner(ui, &th, l, &t, None, true) == widgets::BannerAction::Dismiss {
                    self.ag.run_banner = None;
                }
            }
        });
        // Steer / follow-up input.
        let steer_h = if info.state.is_active() { m.control_height + 20.0 } else { 0.0 };
        let stream_h = (ui.available_height() - steer_h).max(60.0);
        let stream = self.ag.runs.streams.get(&info.id);
        ScrollArea::vertical().max_height(stream_h).auto_shrink([false, false]).stick_to_bottom(info.state.is_active()).show(ui, |ui| {
            ui.add_space(8.0);
            let Some(s) = stream else {
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.label(RichText::new(if info.state.is_active() { "Waiting for the agent…" } else { "The live output of this run was not kept. Use Review to see its changes." }).color(th.muted()));
                });
                return;
            };
            if s.truncated {
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.label(RichText::new("Earlier output was trimmed.").font(widgets::small_font(&th)).color(th.muted()));
                });
            }
            for item in &s.items {
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.vertical(|ui| {
                        ui.set_max_width((ui.available_width() - 16.0).max(80.0));
                        match item {
                            StreamItem::Text(t) => {
                                ui.add(egui::Label::new(RichText::new(t.trim_end()).color(th.fg())).wrap().selectable(true));
                            }
                            StreamItem::Note(n) => {
                                ui.label(RichText::new(n).font(widgets::small_font(&th)).color(th.muted()));
                            }
                            StreamItem::Tool(t) => {
                                let mark = match t.ok {
                                    None => "…",
                                    Some(true) => "ok",
                                    Some(false) => "failed",
                                };
                                let col = if t.ok == Some(false) { err_color(&th) } else { th.muted() };
                                let detail = t.path.clone().filter(|p| !p.is_empty()).unwrap_or_else(|| t.summary.clone());
                                let line = format!("{}  {}  {}", t.name, detail, mark);
                                let g = widgets::fit(ui, &line, widgets::mono_font(&th), col, ui.available_width());
                                ui.label(g).on_hover_text(format!("{} {}", t.name, t.summary));
                            }
                        }
                    });
                });
                ui.add_space(4.0);
            }
        });
        if info.state.is_active() {
            ui.separator();
            let can = caps.steer || caps.follow_up;
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                let w = (ui.available_width() - 100.0).max(100.0);
                let hint = if can { "Steer the agent…" } else { "This backend cannot be steered while running." };
                let te = egui::TextEdit::singleline(&mut self.ag.steer_input).hint_text(hint).desired_width(w).margin(vec2(8.0, 6.0));
                let r = ui.add_enabled(can, te);
                let send = Btn::new("Send").enabled(can && !self.ag.steer_input.trim().is_empty()).show(ui, &th).clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.ag.steer_input.trim().is_empty() && can);
                if send {
                    let t = std::mem::take(&mut self.ag.steer_input);
                    self.ag_steer(&info.id, t.trim());
                }
            });
            widgets::repaint_if_focused(ui.ctx(), Duration::from_secs(1));
        }
    }

    // ------------------------------------------------------------ Accounts (Settings)

    pub fn accounts_section(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        self.ag_ensure_accounts();
        if self.smoke.as_ref().is_some_and(|s| s.scene == "accounts" && !s.requested) {
            ui.scroll_to_cursor(Some(Align::TOP));
        }
        widgets::heading(ui, &th, "Accounts");
        ui.label(RichText::new("Sign in once; CodeTrail never stores API keys.").font(widgets::small_font(&th)).color(th.muted()));
        if let Some(e) = self.ag.account_error.clone() {
            widgets::banner(ui, &th, Level::Error, &e, None, false);
        }
        let accounts: Vec<Account> = match &self.ag.accounts {
            Loadable::Ready(a) => a.clone(),
            Loadable::Failed(e) => {
                if widgets::banner(ui, &th, Level::Error, &format!("Could not read accounts: {e}"), Some("Retry"), false) == widgets::BannerAction::Action {
                    self.ag_refresh_accounts();
                }
                Vec::new()
            }
            _ => {
                widgets::skeleton(ui, &th, 3, 28.0);
                Vec::new()
            }
        };
        for a in &accounts {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(&a.label).color(th.fg()));
                let (txt, col) = match &a.state {
                    AccountState::LoggedIn { method } => (format!("Logged in · {method}"), ok_color(&th)),
                    AccountState::LoggedOut => ("Logged out".to_string(), th.muted()),
                    AccountState::Unavailable { .. } => ("Unavailable".to_string(), th.warn()),
                };
                widgets::badge(ui, &th, &txt, col);
            });
            if let AccountState::Unavailable { reason } = &a.state {
                ui.label(RichText::new(reason).font(widgets::small_font(&th)).color(th.muted()));
            }
            let logging = self.ag.login.as_ref().is_some_and(|(u, _)| u.account == a.id);
            match (&a.login, &a.state) {
                (LoginKind::InApp, AccountState::LoggedIn { .. }) => {
                    if Btn::new("Log out").compact().show(ui, &th).clicked() {
                        self.ag_logout(&a.id);
                    }
                }
                (LoginKind::InApp, AccountState::LoggedOut) if !logging => {
                    if Btn::new("Log in").compact().show(ui, &th).clicked() {
                        self.ag_login(&a.id);
                    }
                }
                (LoginKind::ExternalCli { command }, _) => {
                    let open = self.ag.how_open.as_deref() == Some(a.id.as_str());
                    if Btn::new("How to sign in").compact().selected(open).show(ui, &th).clicked() {
                        self.ag.how_open = if open { None } else { Some(a.id.clone()) };
                    }
                    if open {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(command).font(widgets::mono_font(&th)).color(th.fg()));
                            if widgets::icon_button(ui, &th, Icon::Copy, "Copy command").clicked() {
                                ui.ctx().copy_text(command.clone());
                            }
                        });
                        ui.label(RichText::new("Run this in a terminal. CodeTrail never handles these credentials.").font(widgets::small_font(&th)).color(th.muted()));
                    }
                }
                _ => {}
            }
            if logging {
                self.login_ui(ui);
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Agent runtime").color(th.fg()));
            let (txt, col) = if self.ag.runtime_progress.is_some() {
                ("Installing…".to_string(), th.accent())
            } else if self.ag.runtime.installed {
                (format!("Installed · pi {}", self.ag.runtime.pi.clone().unwrap_or_default()), ok_color(&th))
            } else {
                ("Not installed".to_string(), th.warn())
            };
            widgets::badge(ui, &th, &txt, col);
        });
        if let Some(p) = &self.ag.runtime_progress {
            ui.label(RichText::new(p).font(widgets::small_font(&th)).color(th.muted()));
        }
        if let Some(Err(e)) = &self.ag.runtime_result {
            widgets::banner(ui, &th, Level::Error, &format!("Runtime setup failed: {e}"), None, false);
        }
        if !self.ag.runtime.installed || self.ag.runtime_result.as_ref().is_some_and(|r| r.is_err()) {
            if Btn::new("Install agent runtime").enabled(self.ag.runtime_progress.is_none()).show(ui, &th).clicked() {
                self.ag_runtime_setup();
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Max concurrent runs").color(th.muted()));
            let mut n = self.settings.agent.max_concurrent;
            if ui.add(egui::DragValue::new(&mut n).range(1..=8)).changed() {
                self.ag_set_max_concurrent(n);
            }
        });
    }

    fn login_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let Some((lu, _)) = self.ag.login.clone() else { return };
        ui.label(RichText::new(lu.status_text()).color(th.fg()));
        if let Some(url) = &lu.url {
            ui.horizontal(|ui| {
                let g = widgets::fit(ui, url, widgets::mono_font(&th), th.muted(), (ui.available_width() - 40.0).max(80.0));
                ui.label(g).on_hover_text(url);
                if widgets::icon_button(ui, &th, Icon::Copy, "Copy link").clicked() {
                    ui.ctx().copy_text(url.clone());
                }
            });
        }
        if lu.wants_code() {
            ui.horizontal(|ui| {
                if let Some((u, _)) = &mut self.ag.login {
                    let te = egui::TextEdit::singleline(&mut u.code_input).hint_text("Paste the code").desired_width(180.0).margin(vec2(8.0, 6.0));
                    let r = ui.add(te);
                    let go = Btn::new("Submit").compact().enabled(!u.code_input.trim().is_empty()).show(ui, &th).clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if go {
                        self.ag_login_submit();
                    }
                }
            });
        }
        ui.horizontal(|ui| {
            if lu.finished() {
                if Btn::new("Dismiss").compact().show(ui, &th).clicked() {
                    self.ag.login = None;
                }
            } else if Btn::new("Cancel").compact().show(ui, &th).clicked() {
                self.ag_login_cancel();
            }
        });
    }

    // ------------------------------------------------------------ windows

    pub fn agent_windows(&mut self, ctx: &egui::Context) {
        self.new_run_window(ctx);
        self.confirm_window(ctx);
        self.quit_window(ctx);
        self.ag_quit_tick(ctx);
    }

    fn new_run_window(&mut self, ctx: &egui::Context) {
        let Some(mut nr) = self.ag.new_run.take() else { return };
        let th = self.th.clone();
        let (mut submit, mut cancel) = (false, false);
        dialog(&th, ctx, "New run", 500.0, |ui| {
            widgets::banner(ui, &th, Level::Warn, "Runs use an isolated git worktree. That is not a security sandbox: the agent can run commands with your permissions.", None, false);
            ui.label(RichText::new("Task").color(th.muted()));
            let r = ui.add(egui::TextEdit::multiline(&mut nr.prompt).hint_text("What should the agent do?").desired_rows(4).desired_width(ui.available_width()).margin(vec2(8.0, 6.0)));
            if nr.focus {
                r.request_focus();
                nr.focus = false;
            }
            self.model_picker(ui, "run");
            ui.horizontal(|ui| {
                ui.label(RichText::new("Base").color(th.muted()));
                ui.add(egui::TextEdit::singleline(&mut nr.base_ref).desired_width(180.0).margin(vec2(8.0, 6.0)));
            });
            ui.checkbox(&mut nr.isolate, "Isolate in a separate worktree (recommended)");
            let ready = selection(&self.settings.agent).is_some() && self.ag.gate(self.settings.agent.backend).is_ok() && !nr.prompt.trim().is_empty() && !nr.base_ref.trim().is_empty();
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if Btn::new("Start run").kind(BtnKind::Primary).enabled(ready).show(ui, &th).clicked() {
                        submit = true;
                    }
                    if Btn::new("Cancel").show(ui, &th).clicked() {
                        cancel = true;
                    }
                });
            });
        });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        self.ag.new_run = Some(nr);
        if submit {
            self.ag_submit_run();
        } else if cancel {
            self.ag.new_run = None;
        }
    }

    fn confirm_window(&mut self, ctx: &egui::Context) {
        let Some(c) = self.ag.confirm.clone() else { return };
        let th = self.th.clone();
        let (id, apply) = match &c {
            Confirm::Apply(i) => (i.clone(), true),
            Confirm::Discard(i) => (i.clone(), false),
        };
        let n = self.ag.runs.get(&id).map(|r| r.files_changed).unwrap_or(0);
        let files = if n == 1 { "1 changed file".to_string() } else { format!("{n} changed files") };
        let (mut yes, mut no) = (false, false);
        dialog(&th, ctx, if apply { "Apply run" } else { "Discard run" }, 420.0, |ui| {
            let text = if apply { format!("Merge this run into your current branch? It changes {files}. If the merge conflicts it is aborted and nothing changes.") } else { format!("Discard this run and its worktree? Its {files} will be deleted and cannot be recovered.") };
            ui.label(RichText::new(text).color(th.fg()));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if Btn::new(if apply { "Apply" } else { "Discard" }).kind(BtnKind::Primary).show(ui, &th).clicked() {
                    yes = true;
                }
                if Btn::new("Cancel").show(ui, &th).clicked() {
                    no = true;
                }
            });
        });
        if yes {
            self.ag.confirm = None;
            if apply {
                self.ag_apply(&id);
            } else {
                self.ag_discard(&id);
            }
        } else if no || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.ag.confirm = None;
        }
    }

    fn quit_window(&mut self, ctx: &egui::Context) {
        if self.ag.quit == QuitPhase::Idle {
            return;
        }
        let th = self.th.clone();
        let n = self.ag.runs.active();
        let (mut yes, mut no) = (false, false);
        let aborting = self.ag.quit == QuitPhase::Aborting;
        dialog(&th, ctx, "Quit CodeTrail", 420.0, |ui| {
            if aborting {
                ui.label(RichText::new("Stopping runs and saving their state…").color(th.fg()));
                return;
            }
            let runs = if n == 1 { "1 run is".to_string() } else { format!("{n} runs are") };
            ui.label(RichText::new(format!("{runs} still active. Quitting stops them; their worktrees and partial results are kept.")).color(th.fg()));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if Btn::new("Stop runs and quit").kind(BtnKind::Primary).show(ui, &th).clicked() {
                    yes = true;
                }
                if Btn::new("Keep working").show(ui, &th).clicked() {
                    no = true;
                }
            });
        });
        if yes {
            self.ag_confirm_quit();
        } else if no {
            self.ag.quit = QuitPhase::Idle;
        }
    }
}

pub fn dialog<R>(th: &crate::theme::Theme, ctx: &egui::Context, title: &str, width: f32, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    let mut out = None;
    let w = width.min(ctx.screen_rect().width() - 32.0);
    egui::Window::new(title).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0)).fixed_size(vec2(w, 0.0)).frame(egui::Frame::new().fill(th.raised()).stroke(egui::Stroke::new(1.0, th.border())).corner_radius(th.radius()).inner_margin(egui::Margin::same(16)).shadow(egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(110) })).show(ctx, |ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 10.0);
        ui.set_width(w - 32.0);
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
        out = Some(add(ui));
    });
    out
}
