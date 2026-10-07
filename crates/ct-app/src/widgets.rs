//! Widget kit. Every clickable control is sized and painted here from theme tokens, so
//! padding (12 horizontal, label vertically centred, 6 icon gap, min width = label + 24)
//! is identical everywhere.

use crate::theme::{mix, Theme};
use egui::{pos2, text::LayoutJob, vec2, Align2, Color32, FontId, Galley, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, TextFormat, Ui};
use std::sync::Arc;

// ---------- icons (drawn, not glyphs) ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    ChevronDown,
    ChevronRight,
    Close,
    Search,
    Spark,
    Edit,
    Save,
    Copy,
    Settings,
    Commit,
    File,
    Blame,
    Refresh,
}

pub const ICON_SIZE: f32 = 14.0;

pub fn paint_icon(p: &Painter, c: Pos2, icon: Icon, col: Color32) {
    let s = Stroke::new(1.5, col);
    let r = ICON_SIZE / 2.0 - 1.0;
    let l = |a: Pos2, b: Pos2| p.line_segment([a, b], s);
    let o = |dx: f32, dy: f32| pos2(c.x + dx, c.y + dy);
    match icon {
        Icon::ChevronDown => {
            l(o(-r * 0.7, -r * 0.35), o(0.0, r * 0.35));
            l(o(0.0, r * 0.35), o(r * 0.7, -r * 0.35));
        }
        Icon::ChevronRight => {
            l(o(-r * 0.35, -r * 0.7), o(r * 0.35, 0.0));
            l(o(r * 0.35, 0.0), o(-r * 0.35, r * 0.7));
        }
        Icon::Close => {
            l(o(-r * 0.6, -r * 0.6), o(r * 0.6, r * 0.6));
            l(o(-r * 0.6, r * 0.6), o(r * 0.6, -r * 0.6));
        }
        Icon::Search => {
            p.circle_stroke(o(-r * 0.15, -r * 0.15), r * 0.65, s);
            l(o(r * 0.35, r * 0.35), o(r * 0.9, r * 0.9));
        }
        Icon::Spark => {
            // four-point star
            let pts = [o(0.0, -r), o(r * 0.28, -r * 0.28), o(r, 0.0), o(r * 0.28, r * 0.28), o(0.0, r), o(-r * 0.28, r * 0.28), o(-r, 0.0), o(-r * 0.28, -r * 0.28)];
            p.add(Shape::convex_polygon(pts.to_vec(), col, Stroke::NONE));
        }
        Icon::Edit => {
            l(o(-r * 0.7, r * 0.7), o(r * 0.7, -r * 0.7));
            l(o(-r * 0.7, r * 0.7), o(-r * 0.7, r * 0.2));
            l(o(-r * 0.7, r * 0.7), o(-r * 0.2, r * 0.7));
        }
        Icon::Save => {
            p.rect_stroke(Rect::from_center_size(c, vec2(r * 1.7, r * 1.7)), 1.0, s, egui::StrokeKind::Middle);
            l(o(-r * 0.4, -r * 0.85), o(-r * 0.4, -r * 0.25));
            l(o(-r * 0.4, -r * 0.25), o(r * 0.4, -r * 0.25));
        }
        Icon::Copy => {
            p.rect_stroke(Rect::from_min_size(o(-r * 0.2, -r * 0.2), vec2(r * 1.0, r * 1.1)), 1.0, s, egui::StrokeKind::Middle);
            p.rect_stroke(Rect::from_min_size(o(-r * 0.9, -r * 0.9), vec2(r * 1.0, r * 1.1)), 1.0, s, egui::StrokeKind::Middle);
        }
        Icon::Settings => {
            for dy in [-r * 0.6, 0.0, r * 0.6] {
                l(o(-r * 0.8, dy), o(r * 0.8, dy));
            }
            p.circle_filled(o(-r * 0.3, -r * 0.6), 1.8, col);
            p.circle_filled(o(r * 0.3, 0.0), 1.8, col);
            p.circle_filled(o(-r * 0.1, r * 0.6), 1.8, col);
        }
        Icon::Commit => {
            p.circle_stroke(c, r * 0.45, s);
            l(o(-r, 0.0), o(-r * 0.45, 0.0));
            l(o(r * 0.45, 0.0), o(r, 0.0));
        }
        Icon::File => {
            let pts = vec![o(-r * 0.6, -r), o(r * 0.2, -r), o(r * 0.7, -r * 0.45), o(r * 0.7, r), o(-r * 0.6, r)];
            p.add(Shape::closed_line(pts, s));
        }
        Icon::Blame => {
            for (i, w) in [0.9, 0.55, 0.8].iter().enumerate() {
                let y = -r * 0.6 + i as f32 * r * 0.6;
                l(o(-r * 0.8, y), o(-r * 0.8 + r * 1.7 * w, y));
            }
        }
        Icon::Refresh => {
            p.circle_stroke(c, r * 0.75, s);
            l(o(r * 0.75, -r * 0.2), o(r * 0.75, -r * 0.8));
            l(o(r * 0.75, -r * 0.2), o(r * 0.2, -r * 0.2));
        }
    }
}

// ---------- text helpers ----------

pub fn galley(ui: &Ui, text: &str, font: FontId, color: Color32) -> Arc<Galley> {
    ui.painter().layout_no_wrap(text.to_owned(), font, color)
}

/// Single-line galley truncated with an ellipsis at `max_w`.
pub fn fit(ui: &Ui, text: &str, font: FontId, color: Color32, max_w: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap.max_width = max_w.max(8.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    ui.fonts(|f| f.layout_job(job))
}

pub fn ui_font(th: &Theme) -> FontId {
    FontId::proportional(th.ui_font)
}
pub fn small_font(th: &Theme) -> FontId {
    FontId::proportional((th.ui_font - 1.5).max(9.0))
}
pub fn mono_font(th: &Theme) -> FontId {
    FontId::monospace(th.code_font)
}
pub fn heading_font(th: &Theme) -> FontId {
    FontId::proportional(th.ui_font + 3.0)
}

fn paint_focus(ui: &Ui, th: &Theme, rect: Rect, resp: &Response, radius: f32) {
    if resp.has_focus() {
        ui.painter().rect_stroke(rect.expand(1.0), radius, Stroke::new(th.m().focus_ring, th.accent()), egui::StrokeKind::Outside);
    }
}

// ---------- Button ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BtnKind {
    Primary,
    Secondary,
    Ghost,
}

pub struct Btn<'a> {
    label: &'a str,
    kind: BtnKind,
    icon: Option<Icon>,
    compact: bool,
    enabled: bool,
    selected: bool,
    min_width: f32,
}

impl<'a> Btn<'a> {
    pub fn new(label: &'a str) -> Self {
        Btn { label, kind: BtnKind::Secondary, icon: None, compact: false, enabled: true, selected: false, min_width: 0.0 }
    }
    pub fn primary(label: &'a str) -> Self {
        Btn::new(label).kind(BtnKind::Primary)
    }
    pub fn ghost(label: &'a str) -> Self {
        Btn::new(label).kind(BtnKind::Ghost)
    }
    pub fn kind(mut self, k: BtnKind) -> Self {
        self.kind = k;
        self
    }
    pub fn icon(mut self, i: Icon) -> Self {
        self.icon = Some(i);
        self
    }
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }
    pub fn enabled(mut self, e: bool) -> Self {
        self.enabled = e;
        self
    }
    pub fn selected(mut self, s: bool) -> Self {
        self.selected = s;
        self
    }

    /// Button width for this label (also used by layout code that right-aligns buttons).
    pub fn width(&self, ui: &Ui, th: &Theme) -> f32 {
        let g = galley(ui, self.label, ui_font(th), Color32::WHITE);
        content_width(th, g.size().x, self.icon.is_some(), !self.label.is_empty()).max(self.min_width)
    }

    pub fn show(self, ui: &mut Ui, th: &Theme) -> Response {
        let m = th.m();
        let h = if self.compact { m.control_height_compact } else { m.control_height };
        let (fg, fill, stroke) = self.colors(th, false, false);
        let g = galley(ui, self.label, ui_font(th), fg);
        let w = content_width(th, g.size().x, self.icon.is_some(), !self.label.is_empty()).max(self.min_width);
        let sense = if self.enabled { Sense::click() } else { Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(vec2(w, h), sense);
        if ui.is_rect_visible(rect) {
            let (fg, fill, stroke) = self.colors(th, resp.hovered(), resp.is_pointer_button_down_on());
            let r = m.radius;
            ui.painter().rect(rect, r, fill, stroke, egui::StrokeKind::Inside);
            paint_focus(ui, th, rect, &resp, r);
            let g = galley(ui, self.label, ui_font(th), fg);
            let icon_w = if self.icon.is_some() { ICON_SIZE } else { 0.0 };
            let gap = if self.icon.is_some() && !self.label.is_empty() { m.icon_gap } else { 0.0 };
            let total = icon_w + gap + g.size().x;
            let mut x = rect.center().x - total / 2.0;
            if let Some(i) = self.icon {
                paint_icon(ui.painter(), pos2(x + ICON_SIZE / 2.0, rect.center().y), i, fg);
                x += ICON_SIZE + gap;
            }
            if !self.label.is_empty() {
                ui.painter().galley(pos2(x, rect.center().y - g.size().y / 2.0), g, fg);
            }
        }
        let _ = (stroke, fill, g);
        if self.enabled {
            resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        } else {
            resp
        }
    }

    fn colors(&self, th: &Theme, hover: bool, down: bool) -> (Color32, Color32, Stroke) {
        let p = th.p();
        if !self.enabled {
            return (th.disabled(), Color32::TRANSPARENT, Stroke::new(1.0, mix(th.bg(), th.border(), 0.6)));
        }
        match self.kind {
            BtnKind::Primary => {
                let base = th.accent();
                let fill = if down { mix(base, Color32::BLACK, 0.2) } else if hover { mix(base, p.accent_fg.0, 0.15) } else { base };
                (p.accent_fg.0, fill, Stroke::NONE)
            }
            BtnKind::Secondary => {
                let fill = if self.selected { mix(th.raised(), th.accent(), 0.25) } else if down { mix(th.raised(), th.fg(), 0.14) } else if hover { th.hover() } else { th.raised() };
                (th.fg(), fill, Stroke::new(1.0, if self.selected { th.accent() } else { th.border() }))
            }
            BtnKind::Ghost => {
                let fill = if self.selected { mix(th.bg(), th.accent(), 0.22) } else if down { mix(th.bg(), th.fg(), 0.14) } else if hover { th.hover() } else { Color32::TRANSPARENT };
                (if hover || self.selected { th.fg() } else { th.muted() }, fill, Stroke::NONE)
            }
        }
    }
}

fn content_width(th: &Theme, label_w: f32, has_icon: bool, has_label: bool) -> f32 {
    let m = th.m();
    let inner = match (has_icon, has_label) {
        (true, true) => ICON_SIZE + m.icon_gap + label_w,
        (true, false) => ICON_SIZE,
        _ => label_w,
    };
    inner + 2.0 * m.button_pad_x
}

/// Square icon-only button with tooltip.
pub fn icon_button(ui: &mut Ui, th: &Theme, icon: Icon, tip: &str) -> Response {
    let s = th.m().control_height;
    let (rect, resp) = ui.allocate_exact_size(vec2(s, s), Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if resp.is_pointer_button_down_on() { mix(th.bg(), th.fg(), 0.14) } else if resp.hovered() { th.hover() } else { Color32::TRANSPARENT };
        ui.painter().rect_filled(rect, th.radius(), fill);
        paint_focus(ui, th, rect, &resp, th.radius());
        paint_icon(ui.painter(), rect.center(), icon, if resp.hovered() { th.fg() } else { th.muted() });
    }
    resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

// ---------- Chip / badge ----------

/// Clickable pill: `label ▾`. Used for the Base chip.
pub fn chip(ui: &mut Ui, th: &Theme, prefix: &str, value: &str, chevron: bool, max_w: f32) -> Response {
    let m = th.m();
    let h = m.control_height;
    let pre = galley(ui, prefix, ui_font(th), th.muted());
    let val_max = (max_w - pre.size().x - 2.0 * m.button_pad_x - m.icon_gap * 2.0 - ICON_SIZE).max(40.0);
    let val = fit(ui, value, FontId::proportional(th.ui_font), th.fg(), val_max);
    let mut w = 2.0 * m.button_pad_x + pre.size().x + 4.0 + val.size().x;
    if chevron {
        w += m.icon_gap + ICON_SIZE;
    }
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if resp.hovered() { th.hover() } else { th.raised() };
        ui.painter().rect(rect, h / 2.0, fill, Stroke::new(1.0, th.border()), egui::StrokeKind::Inside);
        paint_focus(ui, th, rect, &resp, h / 2.0);
        let mut x = rect.min.x + m.button_pad_x;
        ui.painter().galley(pos2(x, rect.center().y - pre.size().y / 2.0), pre.clone(), th.muted());
        x += pre.size().x + 4.0;
        ui.painter().galley(pos2(x, rect.center().y - val.size().y / 2.0), val.clone(), th.fg());
        x += val.size().x + m.icon_gap;
        if chevron {
            paint_icon(ui.painter(), pos2(x + ICON_SIZE / 2.0, rect.center().y), Icon::ChevronDown, th.muted());
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Small non-interactive pill (ref names, confidence, status letters).
pub fn badge(ui: &mut Ui, th: &Theme, text: &str, color: Color32) -> Response {
    let g = galley(ui, text, small_font(th), color);
    let pad = vec2(6.0, 1.0);
    let size = g.size() + pad * 2.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(size.x, size.y.max(18.0)), Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect(rect, th.m().radius_small, mix(th.bg(), color, 0.16), Stroke::new(1.0, mix(th.bg(), color, 0.45)), egui::StrokeKind::Inside);
        ui.painter().galley(pos2(rect.min.x + pad.x, rect.center().y - g.size().y / 2.0), g, color);
    }
    resp
}

/// Badge painted at a fixed position inside custom row painting; returns its width.
pub fn paint_badge(painter: &Painter, th: &Theme, left_center: Pos2, text: &str, color: Color32) -> f32 {
    let g = painter.layout_no_wrap(text.to_owned(), small_font(th), color);
    let pad = vec2(6.0, 1.0);
    let size = vec2(g.size().x + pad.x * 2.0, (g.size().y + pad.y * 2.0).max(18.0));
    let rect = Rect::from_min_size(pos2(left_center.x, left_center.y - size.y / 2.0), size);
    painter.rect(rect, th.m().radius_small, mix(th.bg(), color, 0.16), Stroke::new(1.0, mix(th.bg(), color, 0.45)), egui::StrokeKind::Inside);
    painter.galley(pos2(rect.min.x + pad.x, rect.center().y - g.size().y / 2.0), g, color);
    size.x
}

// ---------- Segmented toggle ----------

pub fn segmented(ui: &mut Ui, th: &Theme, options: &[&str], selected: &mut usize) -> bool {
    let m = th.m();
    let h = m.control_height;
    let gal: Vec<_> = options.iter().map(|o| galley(ui, o, ui_font(th), th.fg())).collect();
    let widths: Vec<f32> = gal.iter().map(|g| g.size().x + 2.0 * m.button_pad_x).collect();
    let total: f32 = widths.iter().sum();
    let (rect, _) = ui.allocate_exact_size(vec2(total + 2.0, h), Sense::hover());
    ui.painter().rect(rect, m.radius, th.sunken(), Stroke::new(1.0, th.border()), egui::StrokeKind::Inside);
    let mut x = rect.min.x + 1.0;
    let mut changed = false;
    for (i, (g, w)) in gal.iter().zip(&widths).enumerate() {
        let seg = Rect::from_min_size(pos2(x, rect.min.y + 1.0), vec2(*w, h - 2.0));
        let resp = ui.interact(seg, ui.id().with(("seg", i, options[i])), Sense::click());
        let sel = *selected == i;
        if sel {
            ui.painter().rect_filled(seg, m.radius_small, th.accent());
        } else if resp.hovered() {
            ui.painter().rect_filled(seg, m.radius_small, th.hover());
        }
        paint_focus(ui, th, seg, &resp, m.radius_small);
        let col = if sel { th.p().accent_fg.0 } else if resp.hovered() { th.fg() } else { th.muted() };
        let g2 = galley(ui, options[i], ui_font(th), col);
        let _ = g;
        ui.painter().galley(pos2(seg.center().x - g2.size().x / 2.0, seg.center().y - g2.size().y / 2.0), g2, col);
        if resp.clicked() && !sel {
            *selected = i;
            changed = true;
        }
        x += w;
    }
    changed
}

/// Segmented control where some segments are disabled; `disabled[i] = Some(reason)` shows the
/// reason as a tooltip and blocks selection.
pub fn segmented_gated(ui: &mut Ui, th: &Theme, options: &[&str], disabled: &[Option<String>], selected: &mut usize, pad_x: f32) -> bool {
    let m = th.m();
    let h = m.control_height;
    let widths: Vec<f32> = options.iter().map(|o| galley(ui, o, ui_font(th), th.fg()).size().x + 2.0 * pad_x).collect();
    let total: f32 = widths.iter().sum();
    let (rect, _) = ui.allocate_exact_size(vec2(total + 2.0, h), Sense::hover());
    ui.painter().rect(rect, m.radius, th.sunken(), Stroke::new(1.0, th.border()), egui::StrokeKind::Inside);
    let mut x = rect.min.x + 1.0;
    let mut changed = false;
    for (i, w) in widths.iter().enumerate() {
        let seg = Rect::from_min_size(pos2(x, rect.min.y + 1.0), vec2(*w, h - 2.0));
        let reason = disabled.get(i).and_then(|d| d.as_deref());
        let resp = ui.interact(seg, ui.id().with(("segg", i, options[i])), Sense::click());
        let sel = *selected == i;
        if sel {
            ui.painter().rect_filled(seg, m.radius_small, if reason.is_some() { th.border() } else { th.accent() });
        } else if resp.hovered() && reason.is_none() {
            ui.painter().rect_filled(seg, m.radius_small, th.hover());
        }
        paint_focus(ui, th, seg, &resp, m.radius_small);
        let col = if reason.is_some() { th.disabled() } else if sel { th.p().accent_fg.0 } else if resp.hovered() { th.fg() } else { th.muted() };
        let g2 = galley(ui, options[i], ui_font(th), col);
        ui.painter().galley(pos2(seg.center().x - g2.size().x / 2.0, seg.center().y - g2.size().y / 2.0), g2, col);
        if let Some(r) = reason {
            resp.on_hover_text(r);
        } else if resp.clicked() && !sel {
            *selected = i;
            changed = true;
        }
        x += w;
    }
    changed
}

// ---------- ListRow ----------

/// Allocates a full-width row, paints hover/selected background, returns the response and the
/// content rect (inset by 12px horizontally) for the caller to paint into.
pub fn list_row(ui: &mut Ui, th: &Theme, height: f32, selected: bool) -> (Response, Rect) {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, height), Sense::click());
    if ui.is_rect_visible(rect) {
        if selected {
            ui.painter().rect_filled(rect, 0.0, th.selection());
            ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2(2.0, rect.height())), 0.0, th.accent());
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, 0.0, th.hover());
        }
        paint_focus(ui, th, rect.shrink(1.0), &resp, 0.0);
    }
    let content = rect.shrink2(vec2(th.m().space[2], 0.0));
    (resp, content)
}

// ---------- Banner ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BannerAction {
    None,
    Action,
    Dismiss,
}

pub fn level_color(th: &Theme, l: Level) -> Color32 {
    match l {
        Level::Info => th.accent(),
        Level::Warn => th.warn(),
        Level::Error => th.p().diff.del.fg.0,
    }
}

/// Inline banner with optional action button (e.g. Retry) and dismiss.
pub fn banner(ui: &mut Ui, th: &Theme, level: Level, text: &str, action: Option<&str>, dismissable: bool) -> BannerAction {
    let m = th.m();
    let col = level_color(th, level);
    let mut out = BannerAction::None;
    let frame = egui::Frame::new()
        .fill(mix(th.bg(), col, 0.14))
        .stroke(Stroke::new(1.0, mix(th.bg(), col, 0.5)))
        .corner_radius(m.radius)
        .inner_margin(egui::Margin::symmetric(m.space[2] as i8, m.space[0] as i8));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = m.space[1];
            let reserve = action.map_or(0.0, |a| Btn::new(a).compact().width(ui, th) + m.space[1]) + if dismissable { m.control_height_compact + m.space[1] } else { 0.0 };
            let w = (ui.available_width() - reserve).max(60.0);
            let mut job = LayoutJob::default();
            job.wrap.max_width = w;
            job.append(text, 0.0, TextFormat { font_id: ui_font(th), color: th.fg(), ..Default::default() });
            ui.add_sized([w, 0.0], egui::Label::new(job).selectable(true));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if dismissable && icon_button(ui, th, Icon::Close, "Dismiss").clicked() {
                    out = BannerAction::Dismiss;
                }
                if let Some(a) = action {
                    if Btn::new(a).compact().show(ui, th).clicked() {
                        out = BannerAction::Action;
                    }
                }
            });
        });
    });
    out
}

// ---------- Kbd ----------

pub fn kbd(ui: &mut Ui, th: &Theme, text: &str) -> Response {
    let g = galley(ui, text, small_font(th), th.muted());
    let size = g.size() + vec2(10.0, 2.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(size.x, size.y.max(18.0)), Sense::hover());
    ui.painter().rect(rect, th.m().radius_small, th.sunken(), Stroke::new(1.0, th.border()), egui::StrokeKind::Inside);
    ui.painter().galley(pos2(rect.center().x - g.size().x / 2.0, rect.center().y - g.size().y / 2.0), g, th.muted());
    resp
}

// ---------- misc ----------

/// Heading label (section title inside panels).
pub fn heading(ui: &mut Ui, th: &Theme, text: &str) {
    ui.add_space(th.m().space[1]);
    ui.label(egui::RichText::new(text).font(heading_font(th)).color(th.fg()).strong());
    ui.add_space(th.m().space[0]);
}

pub fn muted(ui: &mut Ui, th: &Theme, text: &str) -> Response {
    ui.label(egui::RichText::new(text).font(ui_font(th)).color(th.muted()))
}

/// Centered empty-state block: title + hint.
pub fn empty_state(ui: &mut Ui, th: &Theme, title: &str, hint: &str) {
    ui.add_space(th.m().space[4]);
    ui.vertical_centered(|ui| {
        ui.label(egui::RichText::new(title).font(heading_font(th)).color(th.fg()));
        ui.add_space(th.m().space[0]);
        ui.label(egui::RichText::new(hint).font(ui_font(th)).color(th.muted()));
    });
}

/// Loading skeleton: pulsing grey bars (static fallback when repaint is off).
pub fn skeleton(ui: &mut Ui, th: &Theme, rows: usize, row_h: f32) {
    let t = ui.input(|i| i.time) as f32;
    let a = 0.5 + 0.5 * (t * 3.0).sin();
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(60));
    for i in 0..rows {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::hover());
        let bar_w = rect.width() * (0.45 + 0.4 * (((i * 37) % 11) as f32 / 11.0));
        let r = Rect::from_min_size(rect.min + vec2(th.m().space[2], row_h * 0.3), vec2(bar_w.max(40.0), row_h * 0.4));
        ui.painter().rect_filled(r, 3.0, mix(th.bg(), th.border(), 0.5 + 0.3 * a));
    }
}

/// Paints text left-aligned inside rect with vertical centring; truncates with an ellipsis.
pub fn paint_text_fit(ui: &Ui, rect: Rect, text: &str, font: FontId, color: Color32) -> f32 {
    let g = fit(ui, text, font, color, rect.width());
    let w = g.size().x;
    ui.painter().galley(pos2(rect.min.x, rect.center().y - g.size().y / 2.0), g, color);
    w
}

pub fn paint_text_right(ui: &Ui, rect: Rect, text: &str, font: FontId, color: Color32) -> f32 {
    let g = galley(ui, text, font, color);
    let w = g.size().x;
    ui.painter().galley(pos2(rect.max.x - w, rect.center().y - g.size().y / 2.0), g, color);
    w
}

pub fn text_at(painter: &Painter, pos: Pos2, align: Align2, text: &str, font: FontId, color: Color32) -> Rect {
    painter.text(pos, align, text, font, color)
}
