//! The single connection / open-failure banner at the top of the main area.

use crate::app::{App, Loadable};
use crate::widgets::{self, Btn, BtnKind, Level};
use egui::text::LayoutJob;
use egui::{Context, TextFormat};

const MAX_ROWS: usize = 3;

impl App {
    /// One full-width banner (max 3 lines, `Details` shows the rest) with Retry and, for remotes,
    /// a shortcut to the Remotes settings. The rail and centre only show a neutral empty state.
    pub(crate) fn conn_banner(&mut self, ctx: &Context, th: &crate::theme::Theme) {
        let open_err = match &self.open_state {
            Loadable::Failed(e) => Some(e.clone()),
            _ => None,
        };
        let Some(text) = crate::panels::banner_text(self.rs.conn_lost.as_deref(), open_err.as_deref()) else { return };
        let remote = self.remote.is_some() || self.rs.conn_lost.is_some();
        let (mut retry, mut open_remotes) = (false, false);
        let id = egui::Id::new("conn_banner_expanded");
        let mut expanded = ctx.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
        egui::TopBottomPanel::top("conn_banner").frame(egui::Frame::new().fill(th.bg())).show(ctx, |ui| {
            let m = th.m();
            let col = widgets::level_color(th, Level::Error);
            egui::Frame::new()
                .fill(crate::theme::mix(th.bg(), col, 0.14))
                .stroke(egui::Stroke::new(1.0, crate::theme::mix(th.bg(), col, 0.5)))
                .corner_radius(m.radius)
                .inner_margin(egui::Margin::symmetric(m.space[2] as i8, m.space[1] as i8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let w = ui.available_width();
                    let job = |rows: usize| {
                        let mut j = LayoutJob::default();
                        j.wrap.max_width = w;
                        j.wrap.max_rows = rows;
                        j.append(&text, 0.0, TextFormat { font_id: widgets::ui_font(th), color: th.fg(), ..Default::default() });
                        j
                    };
                    let elided = ui.fonts(|f| f.layout_job(job(MAX_ROWS)).elided);
                    ui.add(egui::Label::new(job(if expanded { usize::MAX } else { MAX_ROWS })).selectable(true));
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
                        retry |= Btn::new("Retry").compact().kind(BtnKind::Primary).show(ui, th).clicked();
                        if remote {
                            open_remotes |= Btn::new("Open Remotes settings").compact().show(ui, th).clicked();
                        }
                        if (elided || expanded) && Btn::new(if expanded { "Hide details" } else { "Details" }).compact().show(ui, th).clicked() {
                            expanded = !expanded;
                        }
                    });
                });
        });
        ctx.data_mut(|d| d.insert_temp(id, expanded));
        if retry {
            self.rs_retry();
        }
        if open_remotes {
            self.settings_open_section(crate::settingspage::Section::Remotes);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use eframe::App as _;

    fn run(app: &mut App, ctx: &egui::Context, w: f32, h: f32) -> egui::Rect {
        let mut f = eframe::Frame::_new_kittest();
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, h))), ..Default::default() };
        let mut r = egui::Rect::NOTHING;
        let _ = ctx.run(input, |ctx| {
            app.update(ctx, &mut f);
            r = ctx.used_rect();
        });
        r
    }

    #[test]
    fn saved_wide_panels_never_overflow_and_are_not_overwritten_by_the_clamp() {
        for w in [900.0f32, 1000.0, 1280.0] {
            let ctx = egui::Context::default();
            let mut app = crate::ui::settings_window_tests::test_app(&ctx);
            app.settings.rail_width = 520.0;
            app.settings.inspector_width = 560.0;
            let e = "authentication failed. Use Tailscale (no keys)".to_string();
            app.rs.conn_lost = Some(e.clone());
            app.open_state = Loadable::Failed(e);
            for _ in 0..4 {
                let used = run(&mut app, &ctx, w, 700.0);
                assert!(used.width() <= w + 0.5, "overflow at {w}: {used:?}");
            }
            // saved values are untouched (no drag happened)
            assert_eq!((app.settings.rail_width, app.settings.inspector_width), (520.0, 560.0));
        }
    }
}
