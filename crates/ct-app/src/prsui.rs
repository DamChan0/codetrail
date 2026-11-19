//! Pull requests page: a summary strip of bento tiles (one per phase, click to filter), the board
//! of stacks (lane per branch, row per depth) and a detail split on the right with every
//! reviewer's state, the CI checks and the merge state of the selected PR.

use crate::app::App;
use crate::jobs::JobKind;
use crate::prs::{Ci, Phase, Pr, Review, Scope};
use crate::theme::{mix, Theme};
use crate::widgets::{self, Btn, BtnKind, Icon, Level};
use egui::{pos2, vec2, Align, Color32, Key, Layout, Modifiers, Rect, RichText, ScrollArea, Sense, Stroke, Ui};
use std::time::{Duration, Instant};

/// Refresh interval while the page is open.
pub const REFRESH: Duration = Duration::from_secs(60);
/// Card widths below these switch to a shorter card with less on it.
const FULL_W: f32 = 250.0;
const COMPACT_W: f32 = 160.0;
const CARD_MIN_W: f32 = 96.0;

/// Card height for a card `w` wide: full (chips, branch), compact (title + one chip), mini.
pub fn card_h(w: f32) -> f32 {
    if w >= FULL_W {
        104.0
    } else if w >= COMPACT_W {
        76.0
    } else {
        50.0
    }
}
const GAP: f32 = 12.0;
/// Space between a PR and the one stacked on it: room for the arrow.
const ROW_GAP: f32 = 44.0;
const LANE_GAP: f32 = 32.0;
const DETAIL_W: f32 = 380.0;

#[derive(Default)]
pub struct PrsState {
    pub scope: Scope,
    pub list: Option<(String, Vec<Pr>)>,
    /// How many PRs match in total (more than loaded when over [`crate::prs::MAX_PRS`]).
    pub total: usize,
    pub loading: bool,
    pub error: Option<String>,
    pub auth_prompt: bool,
    pub at: Option<Instant>,
    pub selected: Option<u64>,
    pub filter: Option<Phase>,
}

pub fn phase_color(th: &Theme, p: Phase) -> Color32 {
    match p {
        Phase::NeedsWork => th.p().diff.del.fg.0,
        Phase::InReview => th.accent(),
        Phase::Ready => th.p().diff.add.fg.0,
        Phase::Queued => mix(th.p().diff.add.fg.0, th.accent(), 0.5),
        Phase::Draft => th.muted(),
    }
}

pub fn ci_color(th: &Theme, c: Ci) -> Color32 {
    match c {
        Ci::Fail => th.p().diff.del.fg.0,
        Ci::Running => th.warn(),
        Ci::Pass => th.p().diff.add.fg.0,
        Ci::Skipped | Ci::None => th.muted(),
    }
}

pub fn review_color(th: &Theme, r: Review) -> Color32 {
    match r {
        Review::Approved => th.p().diff.add.fg.0,
        Review::ChangesRequested => th.p().diff.del.fg.0,
        Review::Commented => th.accent(),
        Review::Pending | Review::Dismissed => th.muted(),
    }
}

/// Whether the authenticated viewer has an active approval on this PR.
pub fn has_my_approval(p: &Pr) -> bool {
    p.reviewers.iter().any(|r| r.mine && r.state == Review::Approved && !r.re_requested)
}

/// Short review summary for a card: the most important fact first.
pub fn review_summary(p: &Pr) -> (String, Review) {
    let changes = p.reviewers.iter().filter(|r| r.state == Review::ChangesRequested && !r.re_requested).count();
    let waiting = p.reviewers.iter().filter(|r| r.state == Review::Pending || r.re_requested).count();
    let ok = p.approvals();
    if changes > 0 {
        (format!("{changes} requested changes"), Review::ChangesRequested)
    } else if ok > 0 && waiting > 0 {
        (format!("{ok} approved, {waiting} waiting"), Review::Approved)
    } else if ok > 0 {
        (format!("{ok} approved"), Review::Approved)
    } else if waiting > 0 {
        (format!("{waiting} waiting"), Review::Pending)
    } else {
        ("No reviewers".into(), Review::Dismissed)
    }
}

/// Lane (column) and row of every card of a stack: a chain stays in one lane, row = depth;
/// a second child starts a new lane.
pub fn layout_stack(stack: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut lane = 0;
    let mut prev_depth = None;
    stack
        .iter()
        .map(|&(_, d)| {
            if prev_depth.is_some_and(|p| d <= p) {
                lane += 1;
            }
            prev_depth = Some(d);
            (lane, d)
        })
        .collect()
}

impl App {
    pub fn prs_refresh_if_stale(&mut self) {
        if self.smoke.as_ref().is_some_and(|s| s.scene == "prs") {
            return;
        }
        if self.remote.is_some() || self.prs.loading || self.repo.is_none() {
            return;
        }
        if self.prs.at.is_none_or(|t| t.elapsed() >= REFRESH) {
            self.prs_fetch();
        }
    }

    pub fn prs_fetch(&mut self) {
        let Some(root) = self.repo.as_ref().filter(|r| !r.is_remote()).map(|r| r.root.clone()) else { return };
        let scope = self.prs.scope;
        self.prs.loading = true;
        self.prs.at = Some(Instant::now());
        self.jobs.spawn(JobKind::Prs, move |c| {
            let res = crate::prs::fetch(&root, scope, &c.cancel);
            c.finish(crate::app::Msg::Prs { scope, res });
        });
    }

    pub fn prs_loaded(&mut self, scope: crate::prs::Scope, res: Result<(String, Vec<Pr>, usize), String>) {
        if self.smoke.as_ref().is_some_and(|s| s.scene == "prs") || scope != self.prs.scope {
            return;
        }
        self.prs.loading = false;
        self.prs.at = Some(Instant::now());
        match res {
            Ok((slug, prs, total)) => {
                self.prs.error = None;
                self.prs.auth_prompt = false;
                if self.prs.selected.is_some_and(|n| !prs.iter().any(|p| p.number == n)) {
                    self.prs.selected = None;
                }
                self.prs.total = total;
                self.prs.list = Some((slug, prs));
            }
            Err(e) => {
                self.prs.auth_prompt = crate::prs::needs_gh_login(&e);
                self.prs.error = Some(e);
            }
        }
    }

    pub fn prs_page(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        if self.remote.is_some() {
            widgets::empty_state(ui, &th, "Pull requests need a local project", "gh runs on this machine against the clone's GitHub remote.");
            return;
        }
        self.prs_refresh_if_stale();
        ui.ctx().request_repaint_after(Duration::from_secs(5));
        self.prs_header(ui, &th);
        self.prs_auth_dialog(ui.ctx());
        if let Some(e) = self.prs.error.clone() {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width() - 16.0);
                    if widgets::banner(ui, &th, Level::Error, &format!("Could not load pull requests: {e}"), Some("Retry"), false) == widgets::BannerAction::Action {
                        self.prs_fetch();
                    }
                });
            });
        }
        let Some((_, prs)) = self.prs.list.clone() else {
            if self.prs.loading {
                ui.add_space(16.0);
                widgets::skeleton(ui, &th, 6, 40.0);
            }
            return;
        };
        self.prs_keys(ui, &prs);
        ui.add_space(12.0);
        self.prs_summary(ui, &th, &prs);
        ui.add_space(12.0);
        let sel = self.prs.selected.and_then(|n| prs.iter().find(|p| p.number == n)).cloned();
        egui::SidePanel::right("pr_detail").resizable(true).default_width(DETAIL_W).width_range(300.0..=(ui.available_width() * 0.75).max(320.0)).frame(egui::Frame::new().fill(th.bg()).stroke(Stroke::new(1.0, th.border()))).show_inside(ui, |ui| match &sel {
            Some(p) => self.pr_detail(ui, &th, p),
            None => widgets::empty_state(ui, &th, "Select a pull request", "Click a card, or use the arrow keys. Reviewers, checks and merge state show here."),
        });
        egui::CentralPanel::default().frame(egui::Frame::new().fill(th.sunken()).inner_margin(egui::Margin { left: 16, right: 12, top: 0, bottom: 12 })).show_inside(ui, |ui| {
            if prs.is_empty() {
                let what = match self.prs.scope {
                    Scope::Mine => "You have no open pull requests in this repository.",
                    Scope::ReviewRequested => "Nobody is waiting for your review here.",
                };
                widgets::empty_state(ui, &th, "No pull requests", what);
                return;
            }
            self.prs_board(ui, &th, &prs);
        });
    }

    fn prs_auth_dialog(&mut self, ctx: &egui::Context) {
        if !self.prs.auth_prompt { return; }
        let mut open = true;
        let mut login = false;
        let mut settings = false;
        let mut retry = false;
        egui::Window::new("GitHub CLI sign-in required").collapsible(false).resizable(false).open(&mut open).show(ctx, |ui| {
            ui.label("CodeTrail uses `gh` to load pull requests, but GitHub reports missing authentication or permissions.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if Btn::new("Set up / refresh GitHub access").kind(BtnKind::Primary).show(ui, &self.th).clicked() { login = true; }
                if Btn::new("Settings").show(ui, &self.th).clicked() { settings = true; }
                if Btn::new("Retry").show(ui, &self.th).clicked() { retry = true; }
            });
        });
        self.prs.auth_prompt = open && !retry;
        if login {
            match crate::settingspage::start_gh_setup() {
                Ok(()) => self.flash("Complete GitHub CLI setup in the terminal, then retry."),
                Err(e) => self.flash(e),
            }
        }
        if settings {
            self.prs.auth_prompt = false;
            self.settings_open = true;
            self.settings_section = crate::settingspage::Section::GitHub;
        }
        if retry { self.prs_fetch(); }
    }

    fn prs_header(&mut self, ui: &mut Ui, th: &Theme) {
        egui::Frame::new().fill(th.raised()).stroke(Stroke::new(1.0, th.border())).inner_margin(egui::Margin::symmetric(16, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                ui.label(RichText::new("Pull requests").font(widgets::heading_font(th)).color(th.fg()));
                if let Some((slug, _)) = &self.prs.list {
                    ui.label(RichText::new(slug).font(widgets::mono_font(th)).color(th.muted()));
                }
                let mut i = usize::from(self.prs.scope == Scope::ReviewRequested);
                if widgets::segmented(ui, th, &["Mine", "Review requested"], &mut i) {
                    self.prs.scope = if i == 1 { Scope::ReviewRequested } else { Scope::Mine };
                    self.prs.list = None;
                    self.prs.selected = None;
                    self.prs.filter = None;
                    self.prs.error = None;
                    self.prs.auth_prompt = false;
                    self.prs_fetch();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if Btn::new("Refresh").icon(Icon::Refresh).compact().enabled(!self.prs.loading).show(ui, th).clicked() {
                        self.prs_fetch();
                    }
                    let shown = self.prs.list.as_ref().map_or(0, |l| l.1.len());
                    let note = if self.prs.loading {
                        "Updating…".to_string()
                    } else if self.prs.total > shown {
                        format!("{shown} most recently updated of {}", self.prs.total)
                    } else {
                        self.prs.at.map(|t| format!("Updated {} ago, every minute", short_dur(t.elapsed()))).unwrap_or_default()
                    };
                    let nw = widgets::galley(ui, &note, widgets::small_font(th), th.muted()).size().x;
                    if ui.available_width() > nw + 8.0 {
                        ui.label(RichText::new(note).font(widgets::small_font(th)).color(th.muted()));
                    }
                });
            });
        });
    }

    /// Bento strip: "Needs work" is the wide tile and names the PRs; the others are counts.
    fn prs_summary(&mut self, ui: &mut Ui, th: &Theme, prs: &[Pr]) {
        let count = |ph: Phase| prs.iter().filter(|p| p.phase() == ph).count();
        let w = ui.available_width() - 32.0;
        let small = ((w - GAP * 4.0) / 6.0).max(80.0);
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.spacing_mut().item_spacing.x = GAP;
            for ph in Phase::ALL {
                let n = count(ph);
                let wide = ph == Phase::NeedsWork;
                let tw = if wide { small * 2.0 } else { small };
                let on = self.prs.filter == Some(ph);
                let (rect, resp) = ui.allocate_exact_size(vec2(tw, 72.0), Sense::click());
                let col = phase_color(th, ph);
                let p = ui.painter();
                let fill = if on { mix(th.raised(), col, 0.18) } else if resp.hovered() { th.hover() } else { th.raised() };
                p.rect(rect, th.radius() + 2.0, fill, Stroke::new(if on { 2.0 } else { 1.0 }, if on { col } else { th.border() }), egui::StrokeKind::Inside);
                let numc = if n == 0 { th.muted() } else { col };
                p.text(rect.min + vec2(14.0, 12.0), egui::Align2::LEFT_TOP, n.to_string(), egui::FontId::proportional(th.ui_font + 13.0), numc);
                widgets::paint_text_fit(ui, Rect::from_min_max(rect.min + vec2(14.0, 46.0), pos2(rect.max.x - 8.0, rect.min.y + 66.0)), ph.label(), widgets::ui_font(th), th.fg());
                if wide && n > 0 && tw > 280.0 {
                    let names: Vec<String> = prs.iter().filter(|p| p.phase() == ph).take(4).map(|p| format!("#{} {}", p.number, needs_reason(p))).collect();
                    let x = rect.min.x + 120.0;
                    for (k, t) in names.iter().enumerate() {
                        let r = Rect::from_min_size(pos2(x, rect.min.y + 10.0 + k as f32 * 14.0), vec2(rect.max.x - x - 10.0, 14.0));
                        widgets::paint_text_fit(ui, r, t, widgets::small_font(th), th.muted());
                    }
                }
                if resp.has_focus() {
                    ui.painter().rect_stroke(rect, th.radius() + 2.0, Stroke::new(th.m().focus_ring, th.accent()), egui::StrokeKind::Outside);
                }
                if resp.on_hover_text(if on { "Show all" } else { "Show only these" }).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.prs.filter = if on { None } else { Some(ph) };
                }
            }
        });
    }

    /// Up/Down walk the visible cards in board order; Enter opens the selected PR on GitHub.
    fn prs_keys(&mut self, ui: &Ui, prs: &[Pr]) {
        if ui.ctx().wants_keyboard_input() {
            return;
        }
        let order: Vec<u64> = crate::prs::stacks(prs).iter().flatten().map(|&(i, _)| &prs[i]).filter(|p| self.prs.filter.is_none_or(|f| p.phase() == f)).map(|p| p.number).collect();
        let (down, up, enter) = ui.ctx().input_mut(|i| (i.consume_key(Modifiers::NONE, Key::ArrowDown), i.consume_key(Modifiers::NONE, Key::ArrowUp), i.consume_key(Modifiers::NONE, Key::Enter)));
        if order.is_empty() {
            return;
        }
        let cur = self.prs.selected.and_then(|n| order.iter().position(|&x| x == n));
        if down || up {
            let i = match (cur, down) {
                (None, true) => 0,
                (None, false) => order.len() - 1,
                (Some(i), true) => (i + 1).min(order.len() - 1),
                (Some(i), false) => i.saturating_sub(1),
            };
            self.prs.selected = Some(order[i]);
            self.prs_scroll_to = self.prs.selected;
        }
        if enter {
            if let Some(p) = self.prs.selected.and_then(|n| prs.iter().find(|p| p.number == n)) {
                crate::updater::open_url(&p.url);
            }
        }
    }

    fn prs_board(&mut self, ui: &mut Ui, th: &Theme, prs: &[Pr]) {
        let stacks = crate::prs::stacks(prs);
        let filter = self.prs.filter;
        let visible = |s: &Vec<(usize, usize)>| filter.is_none_or(|f| s.iter().any(|&(i, _)| prs[i].phase() == f));
        type Stack<'a> = Vec<&'a Vec<(usize, usize)>>;
        let (chains, singles): (Stack, Stack) = stacks.iter().filter(|s| visible(s)).partition(|s| s.len() > 1);
        let mut click = None;
        let scroll_to = self.prs_scroll_to.take();
        let mut open_url = None;
        ScrollArea::vertical().id_salt("pr_board").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = GAP;
            for s in chains {
                let root = &prs[s[0].0];
                let title = format!("Stack of {} on {}", s.len(), root.base);
                tile(ui, th, &title, |ui| {
                    let pos = layout_stack(s);
                    let lanes = pos.iter().map(|p| p.0).max().unwrap_or(0) + 1;
                    // cards shrink (and simplify) with the space the detail split leaves
                    let lane_gap = if lanes > 1 && ui.available_width() / (lanes as f32) < FULL_W { 16.0 } else { LANE_GAP };
                    let cw = ((ui.available_width() - lane_gap * (lanes as f32 - 1.0)) / lanes as f32).clamp(CARD_MIN_W, 420.0);
                    let ch = card_h(cw);
                    let rows = pos.iter().map(|p| p.1).max().unwrap_or(0) + 1;
                    let total = vec2(cw * lanes as f32 + lane_gap * (lanes as f32 - 1.0), rows as f32 * (ch + ROW_GAP) - ROW_GAP);
                    ScrollArea::horizontal().id_salt(("stack", root.number)).show(ui, |ui| {
                        let (area, _) = ui.allocate_exact_size(total, Sense::hover());
                        let at = |(lane, row): (usize, usize)| Rect::from_min_size(area.min + vec2(lane as f32 * (cw + lane_gap), row as f32 * (ch + ROW_GAP)), vec2(cw, ch));
                        // arrows first, under the cards: from a PR to the one stacked on it (merge order)
                        for (k, &(i, d)) in s.iter().enumerate() {
                            if d == 0 {
                                continue;
                            }
                            if let Some(pk) = (0..k).rev().find(|&j| s[j].1 + 1 == d && prs[s[j].0].head == prs[i].base) {
                                arrow(ui.painter(), th, at(pos[pk]), at(pos[k]));
                            }
                        }
                        for (k, &(i, d)) in s.iter().enumerate() {
                            let r = at(pos[k]);
                            if card(ui, th, &prs[i], r, self.prs.selected == Some(prs[i].number), filter, d == 0).clicked() {
                                if ui.ctx().input(|i| i.modifiers.ctrl) {
                                    if !prs[i].url.is_empty() { open_url = Some(prs[i].url.clone()); }
                                } else {
                                    click = Some(prs[i].number);
                                }
                            }
                            if scroll_to == Some(prs[i].number) {
                                ui.scroll_to_rect(r, Some(Align::Center));
                            }
                        }
                    });
                });
            }
            if !singles.is_empty() {
                tile(ui, th, &format!("Single pull requests ({})", singles.len()), |ui| {
                    let cols = ((ui.available_width() + GAP) / (FULL_W + GAP)).floor().max(1.0) as usize;
                    let cw = (ui.available_width() - GAP * (cols as f32 - 1.0)) / cols as f32;
                    let ch = card_h(cw);
                    let rows = singles.len().div_ceil(cols);
                    let (area, _) = ui.allocate_exact_size(vec2(ui.available_width(), rows as f32 * (ch + GAP) - GAP), Sense::hover());
                    for (k, s) in singles.iter().enumerate() {
                        let i = s[0].0;
                        let r = Rect::from_min_size(area.min + vec2((k % cols) as f32 * (cw + GAP), (k / cols) as f32 * (ch + GAP)), vec2(cw, ch));
                        if card(ui, th, &prs[i], r, self.prs.selected == Some(prs[i].number), filter, false).clicked() {
                            if ui.ctx().input(|i| i.modifiers.ctrl) {
                                if !prs[i].url.is_empty() { open_url = Some(prs[i].url.clone()); }
                            } else {
                                click = Some(prs[i].number);
                            }
                        }
                        if scroll_to == Some(prs[i].number) {
                            ui.scroll_to_rect(r, Some(Align::Center));
                        }
                    }
                });
            }
        });
        if let Some(url) = open_url {
            crate::updater::open_url(&url);
        } else if let Some(n) = click {
            self.prs.selected = if self.prs.selected == Some(n) { None } else { Some(n) };
        }
    }

    fn pr_detail(&mut self, ui: &mut Ui, th: &Theme, p: &Pr) {
        ScrollArea::vertical().id_salt("pr_detail_scroll").auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 14)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("#{}", p.number)).font(widgets::mono_font(th)).color(th.muted()));
                    widgets::badge(ui, th, p.phase().label(), phase_color(th, p.phase()));
                });
                ui.add_space(4.0);
                ui.label(RichText::new(&p.title).font(widgets::heading_font(th)).color(th.fg()));
                ui.add_space(4.0);
                ui.label(RichText::new(format!("{}  into  {}", p.head, p.base)).font(widgets::mono_font(th)).color(th.muted()));
                if !p.repo.is_empty() && self.prs.list.as_ref().is_some_and(|(slug, _)| *slug != p.repo) {
                    ui.label(RichText::new(format!("in {}", p.repo)).font(widgets::mono_font(th)).color(th.warn()));
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if Btn::new("Open on GitHub").kind(BtnKind::Primary).compact().show(ui, th).clicked() {
                        crate::updater::open_url(&p.url);
                    }
                    if Btn::new("Copy link").icon(Icon::Copy).compact().show(ui, th).clicked() {
                        ui.ctx().copy_text(p.url.clone());
                        self.flash("Pull request link copied.");
                    }
                });

                section(ui, th, &format!("Reviewers ({} approved of {})", p.approvals(), p.reviewers.len()));
                if p.reviewers.is_empty() {
                    ui.label(RichText::new("Nobody is assigned yet.").color(th.muted()));
                }
                let now = crate::timefmt::now();
                let mut rv = p.reviewers.clone();
                rv.sort_by_key(|r| (r.state != Review::ChangesRequested, !(r.state == Review::Pending || r.re_requested), r.state != Review::Approved));
                for r in &rv {
                    row(ui, th, |ui| {
                        let state = if r.re_requested { format!("Asked again (was {})", r.state.label().to_lowercase()) } else { r.state.label().to_string() };
                        let col = if r.re_requested || (r.mine && r.state == Review::Approved) { th.warn() } else { review_color(th, r.state) };
                        let who = if r.team { format!("{} (team)", r.name) } else { r.name.clone() };
                        ui.label(RichText::new(who).color(th.fg()));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if let Some(t) = r.at {
                                ui.label(RichText::new(crate::timefmt::rel(now, t)).font(widgets::small_font(th)).color(th.muted()));
                            }
                            widgets::badge(ui, th, &state, col);
                        });
                    });
                }
                if p.unresolved > 0 {
                    ui.add_space(4.0);
                    ui.label(RichText::new(format!("{} unresolved review thread{}", p.unresolved, if p.unresolved == 1 { "" } else { "s" })).color(th.warn()));
                }

                let n = |c: Ci| p.checks.iter().filter(|k| k.state == c).count();
                section(ui, th, &format!("Checks: {} ({} failing, {} running, {} passing)", p.ci.label().to_lowercase(), n(Ci::Fail), n(Ci::Running), n(Ci::Pass)));
                if p.checks.is_empty() {
                    ui.label(RichText::new("No checks reported for the head commit.").color(th.muted()));
                }
                for c in &p.checks {
                    let resp = row(ui, th, |ui| {
                        ui.label(RichText::new(c.state.label()).font(widgets::small_font(th)).color(ci_color(th, c.state)));
                        ui.add_space(4.0);
                        let w = ui.available_width();
                        let g = widgets::fit(ui, &c.name, widgets::ui_font(th), th.fg(), w);
                        ui.label(g);
                    });
                    if !c.url.is_empty() && resp.on_hover_text(&c.url).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        crate::updater::open_url(&c.url);
                    }
                }

                section(ui, th, "Merge");
                let merge = match (&p.queue, p.merge_label()) {
                    (Some((pos, st)), _) => format!("In the merge queue, position {pos} ({})", st.to_lowercase()),
                    (None, Some(l)) => l.to_string(),
                    (None, None) => "Unknown".into(),
                };
                ui.label(RichText::new(merge).color(th.fg()));
                if p.auto_merge {
                    ui.label(RichText::new("Auto-merge is on.").color(th.muted()));
                }
                section(ui, th, "Size");
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("+{}", p.additions)).font(widgets::mono_font(th)).color(th.p().diff.add.fg.0));
                    ui.label(RichText::new(format!("-{}", p.deletions)).font(widgets::mono_font(th)).color(th.p().diff.del.fg.0));
                    ui.label(RichText::new(format!("in {} files, updated {}", p.files, crate::timefmt::rel(now, p.updated))).color(th.muted()));
                });
            });
        });
    }
}

fn needs_reason(p: &Pr) -> &'static str {
    if p.merge_state == "DIRTY" {
        "has conflicts"
    } else if p.decision == "CHANGES_REQUESTED" {
        "changes requested"
    } else if p.ci == Ci::Fail {
        "CI failing"
    } else {
        ""
    }
}

fn short_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s} s")
    } else {
        format!("{} min", s / 60)
    }
}

fn section(ui: &mut Ui, th: &Theme, title: &str) {
    ui.add_space(18.0);
    ui.label(RichText::new(title).strong().color(th.fg()));
    ui.add_space(4.0);
}

fn row(ui: &mut Ui, th: &Theme, add: impl FnOnce(&mut Ui)) -> egui::Response {
    let r = ui.horizontal(|ui| {
        ui.set_min_height(th.m().control_height);
        add(ui);
    });
    r.response.interact(Sense::click())
}

/// One bento tile: a raised surface with a title, holding the body.
fn tile(ui: &mut Ui, th: &Theme, title: &str, body: impl FnOnce(&mut Ui)) {
    egui::Frame::new().fill(th.bg()).stroke(Stroke::new(1.0, th.border())).corner_radius(th.radius() + 2.0).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).strong().color(th.fg()));
        ui.add_space(8.0);
        body(ui);
    });
}

/// Elbow arrow from the bottom of `a` to the top of `b`, head on `b`.
fn arrow(p: &egui::Painter, th: &Theme, a: Rect, b: Rect) {
    let st = Stroke::new(1.6, th.muted());
    let mid = a.max.y + (b.min.y - a.max.y) * 0.45;
    let tip = pos2(b.center().x, b.min.y - 2.0);
    p.line_segment([pos2(a.center().x, a.max.y), pos2(a.center().x, mid)], st);
    p.line_segment([pos2(a.center().x, mid), pos2(b.center().x, mid)], st);
    p.line_segment([pos2(b.center().x, mid), pos2(tip.x, tip.y - 6.0)], st);
    p.add(egui::Shape::convex_polygon(vec![tip, pos2(tip.x - 5.0, tip.y - 8.0), pos2(tip.x + 5.0, tip.y - 8.0)], th.muted(), Stroke::NONE));
}

/// `first`: the bottom of a stack, the PR to merge before the ones on top of it; drawn heavier.
#[allow(clippy::too_many_arguments)]
fn card(ui: &mut Ui, th: &Theme, p: &Pr, r: Rect, selected: bool, filter: Option<Phase>, first: bool) -> egui::Response {
    let resp = ui.interact(r, ui.id().with(("pr_card", p.number)), Sense::click());
    let ph = p.phase();
    let dim = filter.is_some_and(|f| f != ph);
    let fade = |c: Color32| if dim { mix(c, th.raised(), 0.6) } else { c };
    let pcol = phase_color(th, ph);
    let painter = ui.painter_at(r.expand(2.0));
    let fill = if selected { mix(th.raised(), th.accent(), 0.12) } else if resp.hovered() { th.hover() } else { th.raised() };
    let edge = if selected { Stroke::new(2.0, th.accent()) } else if first { Stroke::new(2.0, fade(th.fg())) } else { Stroke::new(1.0, th.border()) };
    painter.rect(r, th.radius() + 2.0, fill, edge, egui::StrokeKind::Inside);
    // phase stripe across the top edge, so the state reads before any text
    painter.rect_filled(Rect::from_min_size(r.min + vec2(1.0, 1.0), vec2(r.width() - 2.0, 3.0)), egui::CornerRadius { nw: 3, ne: 3, sw: 0, se: 0 }, fade(pcol));
    let full = r.width() >= FULL_W;
    let mini = r.width() < COMPACT_W;
    let inner = r.shrink2(vec2(if mini { 8.0 } else { 12.0 }, if mini { 8.0 } else { 10.0 }));
    let numc = if first { fade(th.fg()) } else { fade(th.muted()) };
    let num = painter.text(pos2(inner.min.x, inner.min.y + 2.0), egui::Align2::LEFT_TOP, format!("#{}", p.number), widgets::mono_font(th), numc);
    if first {
        // faux bold: the UI face has no bold cut loaded
        painter.text(pos2(inner.min.x + 0.7, inner.min.y + 2.0), egui::Align2::LEFT_TOP, format!("#{}", p.number), widgets::mono_font(th), numc);
    }
    let badge_w = |t: &str| painter.layout_no_wrap(t.to_string(), widgets::small_font(th), th.fg()).size().x + 12.0;
    // header badges, right to left: phase, then "Merge first" when there is room
    let mut right = inner.max.x;
    if !mini && num.max.x + 8.0 + badge_w(ph.label()) <= right {
        right -= badge_w(ph.label());
        widgets::paint_badge(&painter, th, pos2(right, num.center().y), ph.label(), fade(pcol));
        right -= 6.0;
    }
    if first && !mini && num.max.x + 8.0 + badge_w("Merge first") <= right {
        widgets::paint_badge(&painter, th, pos2(num.max.x + 8.0, num.center().y), "Merge first", fade(th.fg()));
    }
    let title_r = Rect::from_min_max(pos2(inner.min.x, num.max.y + 4.0), pos2(inner.max.x, num.max.y + 24.0));
    let tfont = if mini { widgets::small_font(th) } else { widgets::ui_font(th) };
    widgets::paint_text_fit(ui, title_r, &p.title, tfont.clone(), fade(th.fg()));
    if first {
        widgets::paint_text_fit(ui, title_r.translate(vec2(0.7, 0.0)), &p.title, tfont, fade(th.fg()));
    }
    if full {
        widgets::paint_text_fit(ui, Rect::from_min_max(pos2(inner.min.x, num.max.y + 26.0), pos2(inner.max.x, num.max.y + 42.0)), &p.head, widgets::small_font(th), fade(th.muted()));
    }
    if !mini {
        // status chips in priority order (the compact card keeps the most telling one); a chip
        // that does not fit is dropped, never clipped
        let (rv, rs) = review_summary(p);
        let review_col = if has_my_approval(p) { th.warn() } else { review_color(th, rs) };
        let ci = (format!("CI {}", p.ci.label().to_lowercase()), ci_color(th, p.ci));
        let mut chips: Vec<(String, Color32)> = if p.ci == Ci::Fail && !full { vec![ci] } else { vec![(rv, review_col), ci] };
        if let Some((pos, _)) = &p.queue {
            chips.push((format!("Queue {pos}"), pcol));
        } else if let Some(m) = p.merge_label().filter(|m| *m != "Mergeable" && *m != "Blocked") {
            chips.push((m.to_string(), if m == "Conflicts" { th.p().diff.del.fg.0 } else { th.warn() }));
        }
        if p.unresolved > 0 {
            chips.push((format!("{} open threads", p.unresolved), th.warn()));
        }
        if !full {
            chips.truncate(1);
        }
        let y = inner.max.y - 9.0;
        let mut x = inner.min.x;
        for (t, c) in chips {
            if x + badge_w(&t) > inner.max.x {
                break;
            }
            x += widgets::paint_badge(&painter, th, pos2(x, y), &t, fade(c)) + 6.0;
        }
    }
    // what the smaller cards leave out is one hover away
    let hover = if full {
        "Ctrl-click to open on GitHub".to_string()
    } else {
        let (rv, _) = review_summary(p);
        format!("#{} {}\n{}{}\n{}, CI {}\nCtrl-click to open on GitHub", p.number, p.title, if first { "Merge first. " } else { "" }, ph.label(), rv, p.ci.label().to_lowercase())
    };
    let resp = resp.on_hover_text(hover);
    if resp.has_focus() {
        painter.rect_stroke(r, th.radius() + 2.0, Stroke::new(th.m().focus_ring, th.accent()), egui::StrokeKind::Outside);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chain_stays_in_one_lane_and_a_branch_opens_a_new_one() {
        // 1 -> 2 -> {3, 4}: 3 and 4 are both depth 2
        assert_eq!(layout_stack(&[(0, 0), (1, 1), (2, 2), (3, 2)]), [(0, 0), (0, 1), (0, 2), (1, 2)]);
        assert_eq!(layout_stack(&[(0, 0), (1, 1), (2, 2)]), [(0, 0), (0, 1), (0, 2)]);
        assert_eq!(layout_stack(&[(0, 0), (1, 1), (2, 1)]), [(0, 0), (0, 1), (1, 1)]);
    }

    fn pr(n: u64, head: &str, base: &str, decision: &str, ci: Ci) -> Pr {
        Pr { number: n, repo: "o/n".into(), title: format!("PR {n}"), url: format!("https://x/{n}"), draft: false, head: head.into(), base: base.into(), additions: 1, deletions: 1, files: 1, updated: n as i64, decision: decision.into(), merge_state: "CLEAN".into(), queue: None, auto_merge: false, reviewers: vec![crate::prs::Reviewer { name: "ann".into(), state: Review::Approved, at: Some(0), re_requested: false, team: false, mine: false }], unresolved: 0, ci, checks: vec![] }
    }

    #[test]
    fn the_page_renders_selects_with_arrows_and_filters_by_phase() {
        let ctx = egui::Context::default();
        let mut app = crate::ui::settings_window_tests::test_app(&ctx);
        app.rail = crate::app::RailTab::Prs;
        app.prs.at = Some(Instant::now()); // fresh: no fetch
        app.prs.list = Some(("o/n".into(), vec![pr(3, "c", "b", "APPROVED", Ci::Pass), pr(2, "b", "a", "", Ci::Fail), pr(1, "a", "main", "", Ci::Running)]));
        let frame = |app: &mut App, k: Option<Key>| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1400.0, 900.0))),
                events: k.map(|k| egui::Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }).into_iter().collect(),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.prs_page(ui));
            });
        };
        frame(&mut app, None);
        frame(&mut app, Some(Key::ArrowDown));
        assert_eq!(app.prs.selected, Some(1), "board order is the stack, root first");
        frame(&mut app, Some(Key::ArrowDown));
        frame(&mut app, Some(Key::ArrowDown));
        frame(&mut app, Some(Key::ArrowDown));
        assert_eq!(app.prs.selected, Some(3), "stops at the last card");
        app.prs.filter = Some(Phase::NeedsWork);
        app.prs.selected = None;
        frame(&mut app, Some(Key::ArrowDown));
        assert_eq!(app.prs.selected, Some(2), "the filter limits keyboard order to matching PRs");
        // a late answer for the other scope is ignored
        app.prs_loaded(Scope::ReviewRequested, Ok(("o/n".into(), vec![], 0)));
        assert_eq!(app.prs.list.as_ref().unwrap().1.len(), 3);
        app.prs_loaded(Scope::Mine, Err("gh is not logged in".into()));
        assert!(app.prs.error.is_some() && app.prs.list.is_some(), "an error keeps the last list");
        assert!(app.prs.auth_prompt, "authentication failures open the sign-in prompt");
        frame(&mut app, None);
    }

    #[test]
    fn my_approval_is_detected_separately_for_yellow_emphasis() {
        let mut p = pr(1, "a", "main", "", Ci::Pass);
        assert!(!has_my_approval(&p));
        p.reviewers[0].mine = true;
        assert!(has_my_approval(&p));
        p.reviewers[0].re_requested = true;
        assert!(!has_my_approval(&p));
    }

    #[test]
    fn review_summary_puts_change_requests_first() {
        let mut p = pr(1, "a", "main", "", Ci::Pass);
        assert_eq!(review_summary(&p).0, "1 approved");
        p.reviewers.push(crate::prs::Reviewer { name: "bob".into(), state: Review::Pending, at: None, re_requested: false, team: true, mine: false });
        assert_eq!(review_summary(&p).0, "1 approved, 1 waiting");
        p.reviewers.push(crate::prs::Reviewer { name: "cy".into(), state: Review::ChangesRequested, at: Some(1), re_requested: false, team: false, mine: false });
        assert_eq!(review_summary(&p), ("1 requested changes".into(), Review::ChangesRequested));
    }
}
