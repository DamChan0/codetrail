//! Font discovery. UI: Pretendard > Noto Sans CJK KR. Code: JetBrains Mono > D2Coding >
//! DejaVu Sans Mono, always with a CJK fallback so Korean renders in code too.

use egui::{FontData, FontDefinitions, FontFamily};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundFont {
    pub path: PathBuf,
    /// Face index inside a .ttc collection.
    pub index: u32,
    pub label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FontChoice {
    pub ui: Option<FoundFont>,
    pub code: Option<FoundFont>,
    pub cjk: Option<FoundFont>,
}

fn font_roots() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/usr/share/fonts"), PathBuf::from("/usr/local/share/fonts")];
    if let Some(h) = std::env::var_os("HOME") {
        let h = PathBuf::from(h);
        v.push(h.join(".local/share/fonts"));
        v.push(h.join(".fonts"));
    }
    v
}

fn scan(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > 6 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            scan(&p, depth + 1, out);
        } else if matches!(p.extension().and_then(|x| x.to_str()).map(|x| x.to_ascii_lowercase()).as_deref(), Some("ttf" | "otf" | "ttc")) {
            out.push(p);
        }
    }
}

const BAD_STYLE: &[&str] = &["bold", "italic", "oblique", "light", "thin", "medium", "black", "semibold", "extrabold", "extralight", "heavy", "condensed", "demilight", "nerd", "propo"];

/// Pick the regular-weight file for `key` (lower-case, alnum-only match against the file stem).
fn pick<'a>(files: &'a [PathBuf], key: &str, extra: impl Fn(&str) -> bool) -> Option<&'a PathBuf> {
    let norm = |p: &Path| -> String { p.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect()).unwrap_or_default() };
    let mut best: Option<(&PathBuf, usize)> = None;
    for f in files {
        let n = norm(f);
        if !n.contains(key) || !extra(&n) || BAD_STYLE.iter().any(|b| n.contains(b)) {
            continue;
        }
        let score = n.len() + if n.contains("regular") { 0 } else { 100 };
        if best.map_or(true, |(_, s)| score < s) {
            best = Some((f, score));
        }
    }
    best.map(|(f, _)| f)
}

fn found(p: &PathBuf, label: &str, kr_index: bool) -> FoundFont {
    let is_ttc = p.extension().map_or(false, |e| e.eq_ignore_ascii_case("ttc"));
    // In NotoSansCJK-*.ttc the faces are JP, KR, SC, TC, HK.
    FoundFont { path: p.clone(), index: if is_ttc && kr_index { 1 } else { 0 }, label: label.into() }
}

pub fn choose(files: &[PathBuf]) -> FontChoice {
    let noto_kr = || {
        pick(files, "notosanscjkkr", |_| true)
            .or_else(|| pick(files, "notosanscjk", |_| true))
            .map(|p| found(p, "Noto Sans CJK KR", true))
    };
    let cjk = noto_kr();
    let ui = pick(files, "pretendard", |n| !n.contains("jp") && !n.contains("std")).map(|p| found(p, "Pretendard", false)).or_else(|| cjk.clone());
    let code = pick(files, "jetbrainsmono", |n| !n.contains("nl"))
        .map(|p| found(p, "JetBrains Mono", false))
        .or_else(|| pick(files, "d2coding", |n| !n.contains("ligature")).map(|p| found(p, "D2Coding", false)))
        .or_else(|| pick(files, "dejavusansmono", |_| true).map(|p| found(p, "DejaVu Sans Mono", false)));
    FontChoice { ui, code, cjk }
}

pub fn discover() -> FontChoice {
    let mut files = Vec::new();
    for r in font_roots() {
        scan(&r, 0, &mut files);
    }
    choose(&files)
}

fn load(f: &FoundFont) -> Option<FontData> {
    let bytes = std::fs::read(&f.path).ok()?;
    let mut d = FontData::from_owned(bytes);
    d.index = f.index;
    Some(d)
}

/// Install the chosen fonts into egui. Returns a human-readable summary for the status bar.
pub fn install(ctx: &egui::Context, choice: &FontChoice) -> String {
    let mut defs = FontDefinitions::default();
    let mut prop: Vec<String> = Vec::new();
    let mut mono: Vec<String> = Vec::new();
    let mut add = |defs: &mut FontDefinitions, key: &str, f: &FoundFont| -> bool {
        if defs.font_data.contains_key(key) {
            return true;
        }
        match load(f) {
            Some(d) => {
                defs.font_data.insert(key.to_string(), Arc::new(d));
                true
            }
            None => false,
        }
    };
    if let Some(ui) = &choice.ui {
        if add(&mut defs, "ct-ui", ui) {
            prop.push("ct-ui".into());
        }
    }
    if let Some(code) = &choice.code {
        if add(&mut defs, "ct-code", code) {
            mono.push("ct-code".into());
        }
    }
    let cjk_ok = choice.cjk.as_ref().map_or(false, |c| add(&mut defs, "ct-cjk", c));
    for (fam, list) in [(FontFamily::Proportional, &prop), (FontFamily::Monospace, &mono)] {
        let entry = defs.families.entry(fam).or_default();
        for (i, k) in list.iter().enumerate() {
            entry.insert(i, k.clone());
        }
        if cjk_ok {
            entry.push("ct-cjk".into());
        }
    }
    ctx.set_fonts(defs);
    format!(
        "ui: {} · code: {} · cjk: {}",
        choice.ui.as_ref().map_or("egui default", |f| &f.label),
        choice.code.as_ref().map_or("egui default", |f| &f.label),
        choice.cjk.as_ref().map_or("none (Korean will not render)", |f| &f.label),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn priority_prefers_pretendard_and_jetbrains() {
        let files = vec![
            p("/f/NotoSansCJK-Regular.ttc"),
            p("/f/NotoSansCJK-Bold.ttc"),
            p("/f/DejaVuSansMono.ttf"),
            p("/f/DejaVuSansMono-Bold.ttf"),
            p("/f/Pretendard-Regular.otf"),
            p("/f/Pretendard-Bold.otf"),
            p("/f/JetBrainsMono-Regular.ttf"),
            p("/f/JetBrainsMono-Bold.ttf"),
            p("/f/D2Coding.ttf"),
        ];
        let c = choose(&files);
        assert_eq!(c.ui.unwrap().label, "Pretendard");
        assert_eq!(c.code.unwrap().label, "JetBrains Mono");
        let cjk = c.cjk.unwrap();
        assert_eq!((cjk.label.as_str(), cjk.index), ("Noto Sans CJK KR", 1));
    }

    #[test]
    fn falls_back_to_noto_ui_and_dejavu_mono() {
        let files = vec![p("/f/NotoSansCJK-Regular.ttc"), p("/f/NotoSansCJK-Bold.ttc"), p("/f/DejaVuSansMono.ttf"), p("/f/DejaVuSansMono-Bold.ttf")];
        let c = choose(&files);
        assert_eq!(c.ui.as_ref().unwrap().path, p("/f/NotoSansCJK-Regular.ttc"));
        assert_eq!(c.code.unwrap().label, "DejaVu Sans Mono");
    }

    #[test]
    fn d2coding_beats_dejavu() {
        let files = vec![p("/f/DejaVuSansMono.ttf"), p("/f/D2Coding-Ver1.3.2.ttf")];
        assert_eq!(choose(&files).code.unwrap().label, "D2Coding");
    }

    #[test]
    fn empty_system_gives_none() {
        assert_eq!(choose(&[]), FontChoice::default());
    }
}
