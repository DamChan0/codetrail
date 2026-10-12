//! File-name filter shared by the Commits and Current tabs of the left rail.
//!
//! Syntax (space separated, all terms must hold):
//! - `text`      case-insensitive substring of the full path
//! - `*.rs` `a?c` glob when the term has `*` or `?` (`*` also crosses `/`); a pattern without `/`
//!   may match the file name alone
//! - `!term`     exclusion (any of the forms above)
//! - `ext:rs`    extension shortcut
//!
//! Filtering is recomputed only when the text or the data changes (`Shown` is the cache).

use crate::theme::Theme;
use crate::widgets;
use egui::{vec2, Key, Ui};
use std::sync::Arc;
use std::time::Instant;

/// Above this many entries the matching runs on a worker instead of the UI thread.
pub const ASYNC_MIN: usize = 5000;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Pat {
    Sub(String),
    Glob(Vec<char>, bool),
    Ext(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Term {
    neg: bool,
    pat: Pat,
    /// The term as typed (without `!`).
    src: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileFilter {
    terms: Vec<Term>,
}

fn glob(p: &[char], t: &[char]) -> bool {
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None::<usize>, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

impl FileFilter {
    pub fn parse(text: &str) -> FileFilter {
        let mut terms = Vec::new();
        for tok in text.split_whitespace() {
            let (neg, body) = match tok.strip_prefix('!') {
                Some(b) => (true, b),
                None => (false, tok),
            };
            let lower = body.to_lowercase();
            let pat = if let Some(e) = lower.strip_prefix("ext:") {
                let e = e.trim_start_matches('.');
                if e.is_empty() {
                    continue;
                }
                Pat::Ext(e.to_string())
            } else if lower.is_empty() {
                continue;
            } else if lower.contains(['*', '?']) {
                Pat::Glob(lower.chars().collect(), lower.contains('/'))
            } else {
                Pat::Sub(lower)
            };
            terms.push(Term { neg, pat, src: body.to_string() });
        }
        FileFilter { terms }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub fn matches(&self, path: &str) -> bool {
        if self.terms.is_empty() {
            return true;
        }
        let lp = path.to_lowercase();
        let mut full: Option<Vec<char>> = None;
        for t in &self.terms {
            let hit = match &t.pat {
                Pat::Sub(s) => lp.contains(s.as_str()),
                Pat::Ext(e) => lp.len() > e.len() && lp.ends_with(e.as_str()) && lp[..lp.len() - e.len()].ends_with('.'),
                Pat::Glob(p, has_slash) => {
                    let chars = full.get_or_insert_with(|| lp.chars().collect());
                    glob(p, chars) || (!has_slash && glob(p, &lp.rsplit('/').next().unwrap_or("").chars().collect::<Vec<_>>()))
                }
            };
            if hit == t.neg {
                return false;
            }
        }
        true
    }

    /// First include term as a git pathspec for history narrowing. The spec may match more
    /// than `matches` does (never less), the file list is narrowed again in memory.
    pub fn pathspec(&self) -> Option<String> {
        let t = self.terms.iter().find(|t| !t.neg)?;
        Some(match &t.pat {
            Pat::Sub(s) => format!(":(icase)*{}*", s.replace('\\', "\\\\").replace('[', "\\[")),
            Pat::Ext(e) => format!(":(icase)*.{e}"),
            Pat::Glob(p, _) => {
                let s: String = p.iter().collect();
                if s.starts_with('*') || s.contains('/') {
                    format!(":(icase){s}")
                } else {
                    format!(":(icase)*{s}")
                }
            }
        })
    }

    /// The include term `pathspec` was built from, as typed.
    pub fn pathspec_term(&self) -> Option<&str> {
        self.terms.iter().find(|t| !t.neg).map(|t| t.src.as_str())
    }

    /// How many include terms history narrowing cannot use (it takes the first only).
    pub fn extra_terms(&self) -> usize {
        self.terms.len().saturating_sub(self.terms.iter().take_while(|t| t.neg).count() + 1)
    }
}

/// Totals of the files that pass the filter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub files: usize,
    pub add: u32,
    pub del: u32,
}

/// Filter text of one tab, parsed lazily. The text survives tab switches.
#[derive(Default)]
pub struct RailFilter {
    pub text: String,
    pub parsed: Arc<FileFilter>,
    parsed_text: String,
    /// Bumped whenever the parsed filter changes (cache key).
    pub rev: u64,
    /// Set when the text changed; the selection follows the filter after a short pause.
    pub changed_at: Option<Instant>,
}

impl RailFilter {
    /// Re-parses when the text changed. Cheap enough to call every frame.
    pub fn sync(&mut self) -> bool {
        if self.text == self.parsed_text {
            return false;
        }
        self.parsed_text.clone_from(&self.text);
        self.parsed = Arc::new(FileFilter::parse(&self.text));
        self.rev += 1;
        self.changed_at = Some(Instant::now());
        true
    }

    pub fn active(&self) -> bool {
        !self.parsed.is_empty()
    }
}

/// Cached filter result for one list. `key` = (filter rev, data rev) it was computed for.
#[derive(Default)]
pub struct Shown<T> {
    pub key: Option<(u64, u64)>,
    pub pending: Option<(u64, u64)>,
    pub items: T,
    pub stat: Stat,
}

impl<T> Shown<T> {
    pub fn fresh(&self, key: (u64, u64)) -> bool {
        self.key == Some(key)
    }

    pub fn store(&mut self, key: (u64, u64), items: T, stat: Stat) {
        if self.pending == Some(key) || self.pending.is_none() {
            self.pending = None;
            self.key = Some(key);
            self.items = items;
            self.stat = stat;
        }
    }
}

/// Indices (into `files`) of the files that pass `f`, with their totals.
pub fn pick(files: &[ct_core::FileStat], f: &FileFilter) -> (Vec<usize>, Stat) {
    let mut st = Stat::default();
    let idx: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, x)| f.matches(&x.path))
        .map(|(i, x)| {
            st.files += 1;
            st.add += x.add;
            st.del += x.del;
            i
        })
        .collect();
    (idx, st)
}

/// "37 of 2515 files" / "2515 files".
pub fn count_label(shown: usize, total: usize, filtered: bool) -> String {
    let unit = if total == 1 { "file" } else { "files" };
    if filtered {
        format!("{shown} of {total} {unit}")
    } else {
        format!("{total} {unit}")
    }
}

/// The filter text box (Esc clears it). Returns true when the text changed.
pub fn bar(ui: &mut Ui, th: &Theme, f: &mut RailFilter) -> bool {
    let m = th.m().clone();
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.add_space(m.space[2]);
        let w = ui.available_width() - m.space[2];
        let r = ui.add_sized([w, m.control_height_compact], egui::TextEdit::singleline(&mut f.text).hint_text("Filter files: src *.rs !test").desired_width(w).margin(vec2(8.0, 4.0)));
        changed = r.changed();
        if r.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) && !f.text.is_empty() {
            f.text.clear();
            changed = true;
        }
        r.on_hover_text("Space-separated terms are all required. * and ? match any text / one character, !term excludes, ext:rs matches an extension.");
    });
    changed
}

/// `dir/…/name` with the file name always visible: the directory prefix shrinks first, then the
/// name keeps its extension.
pub fn path_label(ui: &Ui, font: &egui::FontId, path: &str, max_w: f32) -> String {
    let width = |s: &str| widgets::galley(ui, s, font.clone(), egui::Color32::WHITE).size().x;
    if width(path) <= max_w {
        return path.to_string();
    }
    let (dir, name) = path.rsplit_once('/').map_or(("", path), |(d, n)| (d, n));
    let fits = |s: &str| width(s) <= max_w;
    if !dir.is_empty() {
        let chars: Vec<char> = dir.chars().collect();
        let (mut lo, mut hi) = (0usize, chars.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let cand = format!("{}…/{name}", chars[..mid].iter().collect::<String>());
            if fits(&cand) {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let cand = format!("{}…/{name}", chars[..lo].iter().collect::<String>());
        if fits(&cand) {
            return cand;
        }
    }
    shrink_name(name, max_w, &fits)
}

fn shrink_name(name: &str, max_w: f32, fits: &dyn Fn(&str) -> bool) -> String {
    let _ = max_w;
    if fits(name) {
        return name.to_string();
    }
    let chars: Vec<char> = name.chars().collect();
    let ext = name.rfind('.').map_or(0, |i| name[i..].chars().count()).min(8);
    let tail: String = chars[chars.len() - ext..].iter().collect();
    let (mut lo, mut hi) = (0usize, chars.len() - ext);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if fits(&format!("{}…{tail}", chars[..mid].iter().collect::<String>())) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    format!("{}…{tail}", chars[..lo].iter().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ct_core::{FileKind, FileStat, Status};

    fn m(filter: &str, path: &str) -> bool {
        FileFilter::parse(filter).matches(path)
    }

    fn fs(p: &str, add: u32, del: u32) -> FileStat {
        FileStat { path: p.into(), old_path: None, status: Status::Modified, similarity: None, add, del, binary: false, kind: FileKind::Text, old_mode: None, new_mode: None }
    }

    #[test]
    fn plain_terms_are_case_insensitive_substrings_of_the_full_path() {
        assert!(m("", "anything"));
        assert!(m("SRC/ma", "crates/src/Main.rs"));
        assert!(!m("main.rs lib", "crates/src/main.rs"));
        assert!(m("한글", "문서/한글.md"));
    }

    #[test]
    fn several_terms_are_anded_and_a_bang_excludes() {
        assert!(m("src rs", "src/a.rs"));
        assert!(!m("src rs", "docs/a.rs"));
        assert!(m("src !test", "src/a.rs"));
        assert!(!m("src !test", "src/test/a.rs"));
        assert!(m("!test", "src/a.rs"), "an exclusion alone keeps everything else");
        assert!(m("! ", "x"), "a lone bang is ignored");
    }

    #[test]
    fn glob_terms_match_the_whole_path_or_just_the_file_name() {
        assert!(m("*.rs", "a/b/c.RS"));
        assert!(!m("*.rs", "a/b/c.rs.bak"));
        assert!(m("c?.rs", "a/b/c1.rs"), "no slash in the pattern: the name alone may match");
        assert!(!m("c?.rs", "a/b/c12.rs"));
        assert!(m("src/*/mod.rs", "src/x/y/mod.rs"), "* crosses directories");
        assert!(!m("src/*/mod.rs", "lib/x/mod.rs"));
        assert!(m("*.rs !gen*", "src/a.rs"));
        assert!(!m("*.rs !gen*", "gen_a.rs"));
    }

    #[test]
    fn ext_shortcut_matches_the_extension_only() {
        assert!(m("ext:rs", "a/b.rs"));
        assert!(m("ext:.RS", "a/b.rs"));
        assert!(!m("ext:rs", "a/rs"));
        assert!(!m("ext:rs", "a/b.rsx"));
        assert!(m("!ext:md", "a/b.rs"));
        assert!(m("ext:", "anything"), "an empty ext: is ignored");
    }

    #[test]
    fn counts_and_totals_cover_only_the_matches() {
        let files = vec![fs("src/a.rs", 3, 1), fs("docs/b.md", 5, 0), fs("src/c.rs", 2, 2)];
        let (idx, st) = pick(&files, &FileFilter::parse("src"));
        assert_eq!(idx, [0, 2]);
        assert_eq!(st, Stat { files: 2, add: 5, del: 3 });
        assert_eq!(count_label(2, 3, true), "2 of 3 files");
        assert_eq!(count_label(3, 3, false), "3 files");
        assert_eq!(count_label(1, 1, true), "1 of 1 file");
        let (all, st) = pick(&files, &FileFilter::default());
        assert_eq!((all.len(), st.add), (3, 10));
    }

    #[test]
    fn history_pathspec_uses_the_first_include_term_and_escapes_literals() {
        assert_eq!(FileFilter::parse("").pathspec(), None);
        assert_eq!(FileFilter::parse("!test").pathspec(), None);
        assert_eq!(FileFilter::parse("!test Main").pathspec().as_deref(), Some(":(icase)*main*"));
        assert_eq!(FileFilter::parse("*.rs src").pathspec().as_deref(), Some(":(icase)*.rs"));
        assert_eq!(FileFilter::parse("*.rs src").extra_terms(), 1);
        assert_eq!(FileFilter::parse("foo*").pathspec().as_deref(), Some(":(icase)*foo*"));
        assert_eq!(FileFilter::parse("src/*.rs").pathspec().as_deref(), Some(":(icase)src/*.rs"));
        assert_eq!(FileFilter::parse("ext:rs").pathspec().as_deref(), Some(":(icase)*.rs"));
        assert_eq!(FileFilter::parse("a[1]").pathspec().as_deref(), Some(":(icase)*a\\[1]*"));
        assert_eq!(FileFilter::parse("*.rs").pathspec_term(), Some("*.rs"));
    }

    #[test]
    fn rail_filter_reparses_only_when_the_text_changes_and_keeps_the_text() {
        let mut f = RailFilter::default();
        assert!(!f.sync());
        f.text = "src !test".into();
        assert!(f.sync());
        assert!(f.active());
        let rev = f.rev;
        assert!(!f.sync());
        assert_eq!(f.rev, rev);
        f.text.clear();
        assert!(f.sync());
        assert!(!f.active());
    }

    #[test]
    fn shown_cache_accepts_only_the_pending_key() {
        let mut s: Shown<Vec<usize>> = Shown::default();
        s.pending = Some((2, 1));
        s.store((1, 1), vec![9], Stat::default());
        assert!(s.key.is_none(), "a stale result is dropped");
        s.store((2, 1), vec![1, 2], Stat { files: 2, add: 0, del: 0 });
        assert!(s.fresh((2, 1)));
        assert_eq!(s.items, [1, 2]);
    }
}
