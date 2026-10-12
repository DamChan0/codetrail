//! Panel geometry: side-panel widths derived from the CURRENT window width every frame, so a
//! layout saved on a large screen can never squeeze the centre pane on a small one.

pub const RAIL_MIN: f32 = 240.0;
pub const INSP_MIN: f32 = 260.0;
pub const CENTRE_MIN: f32 = 360.0;
/// Below this window width the inspector is not docked; it opens as a drawer over the centre pane.
pub const DOCK_MIN_WINDOW: f32 = 900.0;
const RAIL_MAX_FRAC: f32 = 0.40;
const INSP_MAX_FRAC: f32 = 0.35;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanelLayout {
    /// Docked rail: (initial width, min, max).
    pub rail: (f32, f32, f32),
    /// Docked inspector, `None` when the window is too narrow (drawer mode) or it is closed.
    pub insp: Option<(f32, f32, f32)>,
    /// Width of the drawer when the inspector is open in a narrow window.
    pub drawer: Option<f32>,
}

/// `saved_*` are the user's stored widths (never modified here). Rules: rail <= 40 % and inspector
/// <= 35 % of the window, centre >= 360 px; when they cannot all hold, the inspector shrinks first
/// (down to its minimum), then the rail. Below 900 px the inspector becomes a drawer.
pub fn panel_layout(window_w: f32, saved_rail: f32, saved_insp: f32, insp_open: bool) -> PanelLayout {
    let w = window_w.max(1.0);
    let docked = w >= DOCK_MIN_WINDOW;
    let rail_cap = (w * RAIL_MAX_FRAC).max(160.0);
    let rail_lo = RAIL_MIN.min(rail_cap);
    let mut rail = saved_rail.clamp(rail_lo, rail_cap);
    if !docked {
        // centre must still reach its minimum whenever the window allows it
        rail = rail.min((w - CENTRE_MIN).max(rail_lo));
        let drawer = insp_open.then(|| saved_insp.clamp(INSP_MIN, (w * 0.85).max(INSP_MIN)).min(w));
        return PanelLayout { rail: (rail, rail_lo, rail_cap.min((w - CENTRE_MIN).max(rail_lo))), insp: None, drawer };
    }
    let insp_cap = w * INSP_MAX_FRAC;
    let mut insp = if insp_open { saved_insp.clamp(INSP_MIN, insp_cap.max(INSP_MIN)) } else { 0.0 };
    let mut short = CENTRE_MIN - (w - rail - insp);
    if short > 0.0 && insp_open {
        let d = short.min(insp - INSP_MIN);
        insp -= d;
        short -= d;
    }
    if short > 0.0 {
        rail -= short.min(rail - rail_lo);
    }
    let rail_hi = rail_cap.min(w - insp - CENTRE_MIN).max(rail_lo);
    let insp_hi = insp_cap.min(w - rail - CENTRE_MIN).max(INSP_MIN);
    PanelLayout { rail: (rail, rail_lo, rail_hi), insp: insp_open.then_some((insp, INSP_MIN, insp_hi)), drawer: None }
}

/// Which single message the connection/open banner shows. `conn_lost` wins over an open failure
/// because it carries the same error plus the retry context.
pub fn banner_text(conn_lost: Option<&str>, open_failed: Option<&str>) -> Option<String> {
    match (conn_lost, open_failed) {
        (Some(e), _) => Some(format!("Connection lost \u{2014} {e}")),
        (None, Some(e)) => Some(format!("Cannot open repository: {e}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centre_is_never_a_sliver_for_any_saved_widths_and_window() {
        for w in (400..=3000).step_by(20) {
            let w = w as f32;
            for (r, i) in [(520.0, 560.0), (340.0, 360.0), (640.0, 640.0), (240.0, 260.0), (10.0, 10.0)] {
                for open in [true, false] {
                    let l = panel_layout(w, r, i, open);
                    let docked_i = l.insp.map_or(0.0, |x| x.0);
                    let centre = w - l.rail.0 - docked_i;
                    assert!(l.rail.0 <= w * 0.40 + 0.01, "rail {} at {w}", l.rail.0);
                    assert!(l.rail.0 >= l.rail.1 - 0.01 && l.rail.0 <= l.rail.2 + 0.01);
                    if let Some((v, lo, hi)) = l.insp {
                        assert!(v <= w * 0.35 + 0.01 && v >= lo - 0.01 && v <= hi + 0.01, "insp {v} at {w}");
                    }
                    if w >= 880.0 {
                        assert!(centre >= CENTRE_MIN - 0.01, "centre {centre} at {w} saved ({r},{i}) open={open}");
                    }
                    assert_eq!(l.insp.is_some(), open && w >= DOCK_MIN_WINDOW);
                    assert_eq!(l.drawer.is_some(), open && w < DOCK_MIN_WINDOW);
                }
            }
        }
    }

    #[test]
    fn the_live_bug_1280_with_wide_saved_widths() {
        let l = panel_layout(1280.0, 520.0, 560.0, true);
        // 40% = 512 / 35% = 448 caps; centre 1280-512-448 = 320 < 360 -> inspector shrinks first
        assert_eq!(l.rail.0, 512.0);
        assert_eq!(l.insp.unwrap().0, 408.0);
        assert!(1280.0 - l.rail.0 - l.insp.unwrap().0 >= 360.0);
    }

    #[test]
    fn inspector_shrinks_before_rail_and_small_values_are_kept() {
        let l = panel_layout(900.0, 360.0, 315.0, true);
        assert_eq!(l.insp.unwrap().0, INSP_MIN);
        assert_eq!(l.rail.0, 900.0 - 360.0 - INSP_MIN);
        let l = panel_layout(2000.0, 300.0, 320.0, true);
        assert_eq!((l.rail.0, l.insp.unwrap().0), (300.0, 320.0));
    }

    #[test]
    fn narrow_windows_use_a_drawer_and_keep_a_centre() {
        let l = panel_layout(700.0, 520.0, 560.0, true);
        assert!(l.insp.is_none());
        assert!(l.drawer.unwrap() <= 700.0 * 0.85 + 0.01);
        assert!(700.0 - l.rail.0 >= CENTRE_MIN - 0.01 && l.rail.0 <= 280.0 + 0.01);
    }

    #[test]
    fn banner_has_one_source() {
        assert_eq!(banner_text(None, None), None);
        assert_eq!(banner_text(Some("auth"), Some("auth")).unwrap(), "Connection lost \u{2014} auth");
        assert_eq!(banner_text(None, Some("nope")).unwrap(), "Cannot open repository: nope");
    }
}
