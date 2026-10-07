//! Design tokens: colours, metrics, theme.toml load/save, contrast math, egui visuals.

use egui::{Color32, Stroke};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};

// ---------- colour newtype with "#rrggbb" / "#rrggbbaa" serde ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hex(pub Color32);

impl Hex {
    pub fn parse(s: &str) -> Result<Hex, String> {
        let h = s.trim().trim_start_matches('#');
        if !(h.len() == 6 || h.len() == 8) || !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("invalid colour '{s}' (expected #rrggbb or #rrggbbaa)"));
        }
        let b = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap();
        let a = if h.len() == 8 { b(6) } else { 255 };
        Ok(Hex(Color32::from_rgba_unmultiplied(b(0), b(2), b(4), a)))
    }
    pub fn to_hex(self) -> String {
        let [r, g, b, a] = self.0.to_srgba_unmultiplied();
        if a == 255 {
            format!("#{r:02x}{g:02x}{b:02x}")
        } else {
            format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
        }
    }
}

impl Serialize for Hex {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}
impl<'de> Deserialize<'de> for Hex {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Hex::parse(&s).map_err(serde::de::Error::custom)
    }
}

fn hx(s: &str) -> Hex {
    Hex::parse(s).expect("builtin colour")
}

// ---------- palette ----------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Bg {
    pub base: Hex,
    pub raised: Hex,
    pub sunken: Hex,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Fg {
    pub primary: Hex,
    pub muted: Hex,
    pub disabled: Hex,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DiffSide {
    pub bg: Hex,
    pub fg: Hex,
    pub word: Hex,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Diff {
    pub add: DiffSide,
    pub del: DiffSide,
}

/// Every colour token of PLAN §6. TOML keys read like the spec: `bg.base`, `diff.add.word`, ...
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Palette {
    pub bg: Bg,
    pub fg: Fg,
    pub accent: Hex,
    /// Text drawn on top of `accent` fills.
    pub accent_fg: Hex,
    pub border: Hex,
    pub diff: Diff,
    pub ai_badge: Hex,
    pub warn: Hex,
    /// syntect theme name used for code colouring.
    pub syntax: String,
}

impl Palette {
    pub fn dark() -> Self {
        Palette {
            bg: Bg { base: hx("#16181d"), raised: hx("#1e2128"), sunken: hx("#111317") },
            fg: Fg { primary: hx("#e4e7ec"), muted: hx("#9aa3b2"), disabled: hx("#69717f") },
            accent: hx("#5b8def"),
            accent_fg: hx("#0b1020"),
            border: hx("#2d323c"),
            diff: Diff {
                add: DiffSide { bg: hx("#16301f"), fg: hx("#7ee2a0"), word: hx("#25613a") },
                del: DiffSide { bg: hx("#3a1c22"), fg: hx("#ff9aa8"), word: hx("#7a2c38") },
            },
            ai_badge: hx("#b48cf2"),
            warn: hx("#f0b04a"),
            syntax: "base16-ocean.dark".into(),
        }
    }
    pub fn light() -> Self {
        Palette {
            bg: Bg { base: hx("#fbfbfc"), raised: hx("#f1f3f6"), sunken: hx("#e6e9ee") },
            fg: Fg { primary: hx("#1b1f27"), muted: hx("#505a6b"), disabled: hx("#8a92a1") },
            accent: hx("#2f5fd0"),
            accent_fg: hx("#ffffff"),
            border: hx("#cfd5de"),
            diff: Diff {
                add: DiffSide { bg: hx("#e2f5e8"), fg: hx("#14602e"), word: hx("#a9e3bb") },
                del: DiffSide { bg: hx("#fbe4e7"), fg: hx("#9b1c2f"), word: hx("#f4b3bc") },
            },
            ai_badge: hx("#7442c8"),
            warn: hx("#8a5a00"),
            syntax: "InspiredGitHub".into(),
        }
    }
}

impl Default for Bg {
    fn default() -> Self {
        Palette::dark().bg
    }
}
impl Default for Fg {
    fn default() -> Self {
        Palette::dark().fg
    }
}
impl Default for DiffSide {
    fn default() -> Self {
        Palette::dark().diff.add
    }
}
impl Default for Diff {
    fn default() -> Self {
        Palette::dark().diff
    }
}
impl Default for Palette {
    fn default() -> Self {
        Palette::dark()
    }
}

// ---------- metrics (spacing / size tokens) ----------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Metrics {
    pub space: [f32; 5], // 4/8/12/16/24
    pub control_height: f32,
    pub control_height_compact: f32,
    pub button_pad_x: f32,
    pub icon_gap: f32,
    pub diff_row_height: f32,
    pub commit_row_height: f32,
    pub radius: f32,
    pub radius_small: f32,
    pub focus_ring: f32,
}
impl Default for Metrics {
    fn default() -> Self {
        Metrics {
            space: [4.0, 8.0, 12.0, 16.0, 24.0],
            control_height: 28.0,
            control_height_compact: 24.0,
            button_pad_x: 12.0,
            icon_gap: 6.0,
            diff_row_height: 22.0,
            commit_row_height: 40.0,
            radius: 6.0,
            radius_small: 4.0,
            focus_ring: 2.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Dark,
    Light,
}

/// On-disk theme.toml: both palettes, which one is active, metrics.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ThemeFile {
    pub active: Mode,
    pub dark: Palette,
    pub light: Palette,
    pub metrics: Metrics,
}
impl Default for ThemeFile {
    fn default() -> Self {
        ThemeFile { active: Mode::Dark, dark: Palette::dark(), light: Palette::light(), metrics: Metrics::default() }
    }
}

impl ThemeFile {
    pub fn palette(&self) -> &Palette {
        match self.active {
            Mode::Dark => &self.dark,
            Mode::Light => &self.light,
        }
    }
    pub fn palette_mut(&mut self) -> &mut Palette {
        match self.active {
            Mode::Dark => &mut self.dark,
            Mode::Light => &mut self.light,
        }
    }
    pub fn is_dark(&self) -> bool {
        self.active == Mode::Dark
    }

    /// Parse a (possibly partial) theme.toml: user tokens are merged over the *matching*
    /// built-in palette, so a sparse `[light]` table never inherits dark colours.
    pub fn parse(src: &str) -> Result<ThemeFile, String> {
        let user: toml::Table = toml::from_str(src).map_err(|e| e.to_string())?;
        let mut base = toml::Table::try_from(ThemeFile::default()).map_err(|e| e.to_string())?;
        merge(&mut base, user);
        toml::Value::Table(base).try_into().map_err(|e: toml::de::Error| e.to_string())
    }

    /// Missing file => defaults, no banner. Broken file => defaults + banner message.
    pub fn load(path: &Path) -> (ThemeFile, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(s) => match ThemeFile::parse(&s) {
                Ok(t) => (t, None),
                Err(e) => (
                    ThemeFile::default(),
                    Some(format!("theme.toml is invalid, using defaults ({})", e.lines().next().unwrap_or(""))),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (ThemeFile::default(), None),
            Err(e) => (ThemeFile::default(), Some(format!("cannot read theme.toml: {e}"))),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let s = toml::to_string_pretty(self).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        crate::fsutil::atomic_write(path, s.as_bytes())
    }
}

fn merge(base: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("codetrail");
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("codetrail")
}
pub fn theme_path() -> PathBuf {
    config_dir().join("theme.toml")
}

// ---------- contrast ----------

fn lin(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}
pub fn luminance(c: Color32) -> f32 {
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}
pub fn contrast(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

// ---------- runtime theme handed to widgets ----------

#[derive(Clone, Debug)]
pub struct Theme {
    pub file: ThemeFile,
    pub ui_font: f32,
    pub code_font: f32,
}

impl Theme {
    pub fn p(&self) -> &Palette {
        self.file.palette()
    }
    pub fn m(&self) -> &Metrics {
        &self.file.metrics
    }
    pub fn c(&self, h: Hex) -> Color32 {
        h.0
    }
    pub fn bg(&self) -> Color32 {
        self.p().bg.base.0
    }
    pub fn raised(&self) -> Color32 {
        self.p().bg.raised.0
    }
    pub fn sunken(&self) -> Color32 {
        self.p().bg.sunken.0
    }
    pub fn fg(&self) -> Color32 {
        self.p().fg.primary.0
    }
    pub fn muted(&self) -> Color32 {
        self.p().fg.muted.0
    }
    pub fn disabled(&self) -> Color32 {
        self.p().fg.disabled.0
    }
    pub fn accent(&self) -> Color32 {
        self.p().accent.0
    }
    pub fn border(&self) -> Color32 {
        self.p().border.0
    }
    pub fn warn(&self) -> Color32 {
        self.p().warn.0
    }
    pub fn ai(&self) -> Color32 {
        self.p().ai_badge.0
    }
    /// Hover fill: raised surface nudged toward foreground.
    pub fn hover(&self) -> Color32 {
        mix(self.raised(), self.fg(), 0.08)
    }
    pub fn selection(&self) -> Color32 {
        mix(self.bg(), self.accent(), 0.28)
    }
    pub fn radius(&self) -> f32 {
        self.m().radius
    }

    /// Push tokens into egui's global style.
    pub fn apply(&self, ctx: &egui::Context) {
        let p = self.p();
        let mut v = if self.file.is_dark() { egui::Visuals::dark() } else { egui::Visuals::light() };
        v.override_text_color = None;
        v.panel_fill = p.bg.base.0;
        v.window_fill = p.bg.raised.0;
        v.extreme_bg_color = p.bg.sunken.0;
        v.faint_bg_color = mix(p.bg.base.0, p.bg.raised.0, 0.5);
        v.code_bg_color = p.bg.sunken.0;
        v.hyperlink_color = p.accent.0;
        v.warn_fg_color = p.warn.0;
        v.window_stroke = Stroke::new(1.0, p.border.0);
        v.window_corner_radius = egui::CornerRadius::same(self.m().radius as u8 + 2);
        v.menu_corner_radius = egui::CornerRadius::same(self.m().radius as u8);
        v.selection.bg_fill = self.selection();
        v.selection.stroke = Stroke::new(1.0, p.accent.0);
        let cr = egui::CornerRadius::same(self.m().radius_small as u8);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = p.bg.base.0;
        w.noninteractive.weak_bg_fill = p.bg.base.0;
        w.noninteractive.bg_stroke = Stroke::new(1.0, p.border.0);
        w.noninteractive.fg_stroke = Stroke::new(1.0, p.fg.primary.0);
        w.noninteractive.corner_radius = cr;
        for (ws, fill) in [(&mut w.inactive, p.bg.raised.0), (&mut w.hovered, self.hover()), (&mut w.active, mix(p.bg.raised.0, p.accent.0, 0.25))] {
            ws.bg_fill = fill;
            ws.weak_bg_fill = fill;
            ws.bg_stroke = Stroke::new(1.0, p.border.0);
            ws.fg_stroke = Stroke::new(1.0, p.fg.primary.0);
            ws.corner_radius = cr;
        }
        w.hovered.bg_stroke = Stroke::new(1.0, mix(p.border.0, p.fg.muted.0, 0.5));
        w.open.bg_fill = p.bg.raised.0;
        w.open.corner_radius = cr;
        ctx.set_visuals(v);
        ctx.style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(self.m().space[1], self.m().space[0]);
            s.spacing.button_padding = egui::vec2(self.m().button_pad_x, (self.m().control_height - self.ui_font * 1.3) / 2.0);
            s.spacing.interact_size.y = self.m().control_height;
            s.spacing.scroll.bar_width = 8.0;
            s.spacing.scroll.floating = true;
            s.spacing.menu_margin = egui::Margin::same(self.m().space[1] as i8);
            s.spacing.window_margin = egui::Margin::same(self.m().space[2] as i8);
            s.interaction.selectable_labels = true;
        });
    }
}

pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_text_contrast_meets_aa_in_both_themes() {
        for (name, p) in [("dark", Palette::dark()), ("light", Palette::light())] {
            for (what, bg) in [("base", p.bg.base.0), ("raised", p.bg.raised.0), ("sunken", p.bg.sunken.0)] {
                let c = contrast(p.fg.primary.0, bg);
                assert!(c >= 4.5, "{name} fg.primary on bg.{what} = {c:.2}");
                let m = contrast(p.fg.muted.0, bg);
                assert!(m >= 4.5, "{name} fg.muted on bg.{what} = {m:.2}");
            }
            assert!(contrast(p.accent_fg.0, p.accent.0) >= 4.5, "{name} accent_fg on accent");
            for (side, d) in [("add", &p.diff.add), ("del", &p.diff.del)] {
                assert!(contrast(p.fg.primary.0, d.bg.0) >= 4.5, "{name} fg on diff.{side}.bg");
                assert!(contrast(p.fg.primary.0, d.word.0) >= 4.5, "{name} fg on diff.{side}.word");
                assert!(contrast(d.fg.0, d.bg.0) >= 4.5, "{name} diff.{side}.fg on bg");
            }
            assert!(contrast(p.warn.0, p.bg.base.0) >= 4.5, "{name} warn");
            assert!(contrast(p.ai_badge.0, p.bg.base.0) >= 4.5, "{name} ai_badge");
        }
    }

    #[test]
    fn contrast_known_values() {
        assert!((contrast(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.01);
        assert!((contrast(Color32::WHITE, Color32::WHITE) - 1.0).abs() < 0.001);
    }

    #[test]
    fn hex_roundtrip_and_rejects_garbage() {
        assert_eq!(Hex::parse("#5b8def").unwrap().to_hex(), "#5b8def");
        assert_eq!(Hex::parse("#5b8def80").unwrap().to_hex(), "#5b8def80");
        assert!(Hex::parse("5b8de").is_err());
        assert!(Hex::parse("#zzzzzz").is_err());
    }

    #[test]
    fn theme_roundtrips_through_toml_and_uses_spec_keys() {
        let t = ThemeFile::default();
        let s = toml::to_string_pretty(&t).unwrap();
        assert!(s.contains("[dark.bg]") && s.contains("[dark.diff.add]"), "{s}");
        assert_eq!(ThemeFile::parse(&s).unwrap(), t);
    }

    #[test]
    fn partial_theme_file_overrides_only_given_tokens() {
        let t = ThemeFile::parse("active = \"light\"\n[light]\naccent = \"#ff0000\"\n").unwrap();
        assert_eq!(t.active, Mode::Light);
        assert_eq!(t.palette().accent.to_hex(), "#ff0000");
        assert_eq!(t.palette().bg.base, Palette::light().bg.base, "sparse [light] keeps light base");
        assert_eq!(t.dark, Palette::dark());
    }

    #[test]
    fn broken_theme_file_falls_back_with_banner() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("theme.toml");
        std::fs::write(&p, "active = \"dark\"\n[dark]\naccent = \"not-a-colour\"\n").unwrap();
        let (t, banner) = ThemeFile::load(&p);
        assert_eq!(t, ThemeFile::default());
        assert!(banner.unwrap().contains("invalid"));
        std::fs::write(&p, "[[[ not toml").unwrap();
        assert!(ThemeFile::load(&p).1.is_some());
        let (t2, b2) = ThemeFile::load(&d.path().join("missing.toml"));
        assert_eq!(t2, ThemeFile::default());
        assert!(b2.is_none());
    }

    #[test]
    fn save_then_load_is_identity() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/theme.toml");
        let mut t = ThemeFile::default();
        t.active = Mode::Light;
        t.palette_mut().accent = Hex::parse("#123456").unwrap();
        t.save(&p).unwrap();
        let (l, b) = ThemeFile::load(&p);
        assert!(b.is_none());
        assert_eq!(l, t);
    }
}
