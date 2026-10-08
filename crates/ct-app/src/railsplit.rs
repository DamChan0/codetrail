//! Height split between the commit list and the changed-files list in the left rail.

pub const MIN_FRAC: f32 = 0.15;
pub const MAX_FRAC: f32 = 0.75;
/// Files above this count get the larger default share.
pub const MANY_FILES: usize = 6;
pub const FILTER_MIN_FILES: usize = 8;

pub fn clamp(frac: f32) -> f32 {
    if frac.is_finite() { frac.clamp(MIN_FRAC, MAX_FRAC) } else { 0.25 }
}

/// Files list share of the panel when the user has not dragged the splitter.
pub fn default_frac(files: usize) -> f32 {
    if files > MANY_FILES { 0.45 } else { 0.25 }
}

/// Pixel height of the files list: the saved fraction (clamped) or the default for `files`.
pub fn files_height(saved: Option<f32>, files: usize, total: f32) -> f32 {
    (saved.map_or_else(|| default_frac(files), clamp) * total).round()
}

/// Fraction after dragging the splitter so the files list has `new_height` of `total`.
pub fn frac_for(new_height: f32, total: f32) -> f32 {
    if total <= 0.0 { 0.25 } else { clamp(new_height / total) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_depends_on_file_count() {
        assert_eq!(default_frac(0), 0.25);
        assert_eq!(default_frac(6), 0.25);
        assert_eq!(default_frac(7), 0.45);
        assert_eq!(files_height(None, 44, 800.0), 360.0);
        assert_eq!(files_height(None, 3, 800.0), 200.0);
    }

    #[test]
    fn saved_fraction_wins_and_is_clamped() {
        assert_eq!(files_height(Some(0.6), 3, 1000.0), 600.0);
        assert_eq!(files_height(Some(0.01), 44, 1000.0), 150.0);
        assert_eq!(files_height(Some(0.99), 44, 1000.0), 750.0);
        assert_eq!(clamp(f32::NAN), 0.25);
        assert_eq!(frac_for(5000.0, 1000.0), MAX_FRAC);
        assert_eq!(frac_for(10.0, 1000.0), MIN_FRAC);
        assert_eq!(frac_for(300.0, 0.0), 0.25);
    }
}
