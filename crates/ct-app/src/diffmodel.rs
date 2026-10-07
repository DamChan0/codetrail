//! Diff display model: flat rows (unified + side-by-side), word-level ranges (`similar`),
//! per-line syntax spans. Pure data, no egui.

use crate::highlight::{self, Span};
use crate::selection::RowLines;
use ct_core::{DiffFile, LineKind};
use similar::{capture_diff_slices, Algorithm, DiffOp};

pub const LARGE_DIFF_LINES: usize = 20_000;
const WORD_DIFF_MAX_BYTES: usize = 1_000;

#[derive(Clone, Debug)]
pub struct Line {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    /// Tabs expanded to spaces.
    pub text: String,
    /// Changed byte ranges inside `text` (word-level highlight), merged and sorted.
    pub words: Vec<(usize, usize)>,
    pub spans: Vec<Span>,
    pub no_newline: bool,
    pub hunk: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnifiedRow {
    Hunk(usize),
    Line(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitRow {
    Hunk(usize),
    Pair(Option<usize>, Option<usize>),
}

pub struct DiffModel {
    pub file: DiffFile,
    pub lines: Vec<Line>,
    pub unified: Vec<UnifiedRow>,
    pub split: Vec<SplitRow>,
    pub hunk_headers: Vec<String>,
    pub max_cols: usize,
}

pub fn expand_tabs(s: &str, tab: usize) -> String {
    if !s.contains('\t') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut col = 0;
    for ch in s.chars() {
        if ch == '\t' {
            let n = tab - col % tab;
            out.extend(std::iter::repeat(' ').take(n));
            col += n;
        } else {
            out.push(ch);
            col += cell_width(ch);
        }
    }
    out
}

/// Terminal-style cell width: CJK/fullwidth = 2.
pub fn cell_width(ch: char) -> usize {
    let c = ch as u32;
    if (0x1100..=0x115F).contains(&c) || (0x2E80..=0xA4CF).contains(&c) || (0xAC00..=0xD7A3).contains(&c) || (0xF900..=0xFAFF).contains(&c) || (0xFE30..=0xFE6F).contains(&c) || (0xFF00..=0xFF60).contains(&c) || (0xFFE0..=0xFFE6).contains(&c) {
        2
    } else {
        1
    }
}

pub fn cols(s: &str) -> usize {
    s.chars().map(cell_width).sum()
}

/// Split into identifier / whitespace / single-punct tokens (byte ranges).
fn tokens(s: &str) -> Vec<&str> {
    let mut v = Vec::new();
    let mut start = None::<usize>;
    let mut kind = 0u8; // 1 ident, 2 space
    for (i, ch) in s.char_indices() {
        let k = if ch.is_alphanumeric() || ch == '_' { 1 } else if ch.is_whitespace() { 2 } else { 0 };
        match (start, k) {
            (Some(_), k2) if k2 == kind && k != 0 => {}
            _ => {
                if let Some(st) = start {
                    v.push(&s[st..i]);
                }
                start = Some(i);
                kind = k;
            }
        }
    }
    if let Some(st) = start {
        v.push(&s[st..]);
    }
    v
}

/// Word-level changed ranges for a removed/added pair. Empty when the lines are too different
/// (highlighting everything is noise) or too long.
pub fn word_ranges(old: &str, new: &str) -> (Vec<(usize, usize)>, Vec<(usize, usize)>) {
    if old.len() > WORD_DIFF_MAX_BYTES || new.len() > WORD_DIFF_MAX_BYTES || old == new {
        return (vec![], vec![]);
    }
    let (a, b) = (tokens(old), tokens(new));
    let ops = capture_diff_slices(Algorithm::Myers, &a, &b);
    let offs = |t: &[&str]| -> Vec<usize> {
        let mut o = Vec::with_capacity(t.len() + 1);
        let mut p = 0;
        for x in t {
            o.push(p);
            p += x.len();
        }
        o.push(p);
        o
    };
    let (ao, bo) = (offs(&a), offs(&b));
    let (mut ar, mut br) = (Vec::new(), Vec::new());
    let mut equal_bytes = 0;
    for op in ops {
        match op {
            DiffOp::Equal { old_index, len, .. } => equal_bytes += ao[old_index + len] - ao[old_index],
            DiffOp::Delete { old_index, old_len, .. } => ar.push((ao[old_index], ao[old_index + old_len])),
            DiffOp::Insert { new_index, new_len, .. } => br.push((bo[new_index], bo[new_index + new_len])),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                ar.push((ao[old_index], ao[old_index + old_len]));
                br.push((bo[new_index], bo[new_index + new_len]));
            }
        }
    }
    let total = old.len().max(new.len()).max(1);
    if (equal_bytes as f32) / (total as f32) < 0.35 {
        return (vec![], vec![]);
    }
    (merge(ar), merge(br))
}

fn merge(mut v: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    v.retain(|r| r.1 > r.0);
    v.sort();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for r in v {
        match out.last_mut() {
            Some(l) if r.0 <= l.1 => l.1 = l.1.max(r.1),
            _ => out.push(r),
        }
    }
    out
}

impl DiffModel {
    pub fn total_lines(file: &DiffFile) -> usize {
        file.hunks.iter().map(|h| h.lines.len()).sum()
    }

    pub fn build(file: DiffFile, tab: usize) -> DiffModel {
        let lang = highlight::lang_for_path(&file.path);
        let mut lines: Vec<Line> = Vec::new();
        let mut unified = Vec::new();
        let mut split = Vec::new();
        let mut headers = Vec::new();
        let mut max_cols = 0;
        for (hi, h) in file.hunks.iter().enumerate() {
            headers.push(format!("@@ -{},{} +{},{} @@ {}", h.old_start, h.old_len, h.new_start, h.new_len, h.section).trim_end().to_string());
            unified.push(UnifiedRow::Hunk(hi));
            split.push(SplitRow::Hunk(hi));
            let texts: Vec<String> = h.lines.iter().map(|l| expand_tabs(&l.text, tab)).collect();
            // syntax: old side (ctx+del) and new side (ctx+add) are highlighted separately
            let old_idx: Vec<usize> = (0..h.lines.len()).filter(|&i| h.lines[i].kind != LineKind::Add).collect();
            let new_idx: Vec<usize> = (0..h.lines.len()).filter(|&i| h.lines[i].kind != LineKind::Del).collect();
            let run = |idx: &[usize]| highlight::highlight(lang, &idx.iter().map(|&i| texts[i].as_str()).collect::<Vec<_>>());
            let (old_sp, new_sp) = (run(&old_idx), run(&new_idx));
            let mut spans: Vec<Vec<Span>> = vec![Vec::new(); h.lines.len()];
            for (k, &i) in old_idx.iter().enumerate() {
                if h.lines[i].kind == LineKind::Del {
                    spans[i] = old_sp[k].clone();
                }
            }
            for (k, &i) in new_idx.iter().enumerate() {
                spans[i] = new_sp[k].clone();
            }
            let base = lines.len();
            let mut words: Vec<Vec<(usize, usize)>> = vec![Vec::new(); h.lines.len()];
            let mut i = 0;
            while i < h.lines.len() {
                if h.lines[i].kind == LineKind::Del {
                    let ds = i;
                    while i < h.lines.len() && h.lines[i].kind == LineKind::Del {
                        i += 1;
                    }
                    let as_ = i;
                    while i < h.lines.len() && h.lines[i].kind == LineKind::Add {
                        i += 1;
                    }
                    for k in 0..(as_ - ds).min(i - as_) {
                        let (wo, wn) = word_ranges(&texts[ds + k], &texts[as_ + k]);
                        words[ds + k] = wo;
                        words[as_ + k] = wn;
                    }
                } else {
                    i += 1;
                }
            }
            for (k, l) in h.lines.iter().enumerate() {
                max_cols = max_cols.max(cols(&texts[k]));
                lines.push(Line { kind: l.kind, old_no: l.old_no, new_no: l.new_no, text: texts[k].clone(), words: std::mem::take(&mut words[k]), spans: std::mem::take(&mut spans[k]), no_newline: l.no_newline_at_eof, hunk: hi });
                unified.push(UnifiedRow::Line(base + k));
            }
            // side-by-side: pair del-runs with add-runs
            let mut k = 0;
            while k < h.lines.len() {
                match h.lines[k].kind {
                    LineKind::Ctx => {
                        split.push(SplitRow::Pair(Some(base + k), Some(base + k)));
                        k += 1;
                    }
                    _ => {
                        let ds = k;
                        while k < h.lines.len() && h.lines[k].kind == LineKind::Del {
                            k += 1;
                        }
                        let de = k;
                        let as_ = k;
                        while k < h.lines.len() && h.lines[k].kind == LineKind::Add {
                            k += 1;
                        }
                        let ae = k;
                        for j in 0..(de - ds).max(ae - as_) {
                            let l = (ds + j < de).then_some(base + ds + j);
                            let r = (as_ + j < ae).then_some(base + as_ + j);
                            split.push(SplitRow::Pair(l, r));
                        }
                    }
                }
            }
        }
        DiffModel { file, lines, unified, split, hunk_headers: headers, max_cols }
    }

    pub fn row_lines(&self) -> Vec<RowLines> {
        self.lines.iter().map(|l| RowLines { kind: l.kind, old_no: l.old_no, new_no: l.new_no }).collect()
    }

    /// Row index (in the active layout) of each hunk header.
    pub fn hunk_rows(&self, split: bool) -> Vec<usize> {
        if split {
            self.split.iter().enumerate().filter_map(|(i, r)| matches!(r, SplitRow::Hunk(_)).then_some(i)).collect()
        } else {
            self.unified.iter().enumerate().filter_map(|(i, r)| matches!(r, UnifiedRow::Hunk(_)).then_some(i)).collect()
        }
    }

    pub fn row_count(&self, split: bool) -> usize {
        if split {
            self.split.len()
        } else {
            self.unified.len()
        }
    }

    /// Row index in the active layout that shows line `idx`.
    pub fn row_of_line(&self, idx: usize, split: bool) -> Option<usize> {
        if split {
            self.split.iter().position(|r| matches!(r, SplitRow::Pair(a, b) if *a == Some(idx) || *b == Some(idx)))
        } else {
            self.unified.iter().position(|r| *r == UnifiedRow::Line(idx))
        }
    }

    /// Added-line texts of one hunk (for ct-store `match_hunk`).
    pub fn added_lines(&self, hunk: usize) -> Vec<String> {
        self.file.hunks[hunk].lines.iter().filter(|l| l.kind == LineKind::Add).map(|l| l.text.clone()).collect()
    }

    /// Unified-diff text of the hunks overlapping `lo..=hi` line indexes (for the Ask AI prompt).
    pub fn hunk_text(&self, lo: usize, hi: usize) -> String {
        let mut hs: Vec<usize> = self.lines[lo.min(self.lines.len().saturating_sub(1))..=hi.min(self.lines.len().saturating_sub(1))].iter().map(|l| l.hunk).collect();
        hs.dedup();
        let mut out = String::new();
        if self.lines.is_empty() {
            return out;
        }
        out.push_str(&format!("--- a/{}\n+++ b/{}\n", self.file.old_path.as_deref().unwrap_or(&self.file.path), self.file.path));
        for h in hs {
            out.push_str(&self.hunk_headers[h]);
            out.push('\n');
            for l in self.file.hunks[h].lines.iter() {
                out.push(match l.kind {
                    LineKind::Ctx => ' ',
                    LineKind::Add => '+',
                    LineKind::Del => '-',
                });
                out.push_str(&l.text);
                out.push('\n');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ct_core::{DiffLine, FileKind, Hunk, Status};

    fn dl(kind: LineKind, o: Option<u32>, n: Option<u32>, t: &str) -> DiffLine {
        DiffLine { kind, old_no: o, new_no: n, text: t.into(), no_newline_at_eof: false }
    }
    fn file(lines: Vec<DiffLine>) -> DiffFile {
        DiffFile {
            path: "src/a.rs".into(),
            old_path: None,
            old_oid: None,
            new_oid: None,
            old_mode: None,
            new_mode: None,
            kind: FileKind::Text,
            status: Status::Modified,
            hunks: vec![Hunk { old_start: 1, old_len: 3, new_start: 1, new_len: 3, section: "fn x".into(), lines }],
        }
    }

    #[test]
    fn word_ranges_mark_only_changed_tokens() {
        let (o, n) = word_ranges("let total = price * qty;", "let total = price * count;");
        assert_eq!(&"let total = price * qty;"[o[0].0..o[0].1], "qty");
        assert_eq!(&"let total = price * count;"[n[0].0..n[0].1], "count");
        assert_eq!((o.len(), n.len()), (1, 1));
    }

    #[test]
    fn word_ranges_skip_unrelated_lines_and_identical_lines() {
        assert_eq!(word_ranges("aaaa bbbb cccc", "xxxx yyyy zzzz"), (vec![], vec![]));
        assert_eq!(word_ranges("same", "same"), (vec![], vec![]));
    }

    #[test]
    fn word_ranges_are_char_boundaries_with_korean() {
        let (a, b) = ("let msg = \"안녕 세상\";", "let msg = \"안녕 우주\";");
        let (o, n) = word_ranges(a, b);
        assert!(!o.is_empty() && !n.is_empty());
        for (s, e) in o {
            assert!(a.is_char_boundary(s) && a.is_char_boundary(e));
        }
        for (s, e) in n {
            assert!(b.is_char_boundary(s) && b.is_char_boundary(e));
        }
    }

    #[test]
    fn tab_expansion_is_column_aware() {
        assert_eq!(expand_tabs("\tx", 4), "    x");
        assert_eq!(expand_tabs("ab\tc", 4), "ab  c");
        assert_eq!(expand_tabs("한\tc", 4), "한  c");
    }

    #[test]
    fn unified_and_split_rows_cover_every_line_once() {
        let f = file(vec![
            dl(LineKind::Ctx, Some(1), Some(1), "a"),
            dl(LineKind::Del, Some(2), None, "old1"),
            dl(LineKind::Del, Some(3), None, "old2"),
            dl(LineKind::Add, None, Some(2), "new1"),
            dl(LineKind::Ctx, Some(4), Some(3), "z"),
            dl(LineKind::Add, None, Some(4), "tail"),
        ]);
        let m = DiffModel::build(f, 4);
        assert_eq!(m.unified.len(), 1 + 6);
        // split: hunk + ctx + (old1|new1) + (old2|-) + ctx + (-|tail)
        assert_eq!(m.split.len(), 1 + 5);
        let mut seen = vec![0; m.lines.len()];
        for r in &m.split {
            if let SplitRow::Pair(a, b) = r {
                if a == b {
                    seen[a.unwrap()] += 1;
                } else {
                    for x in [a, b].into_iter().flatten() {
                        seen[*x] += 1;
                    }
                }
            }
        }
        assert!(seen.iter().all(|&c| c == 1), "{seen:?}");
        assert_eq!(m.split[2], SplitRow::Pair(Some(1), Some(3)));
        assert_eq!(m.split[3], SplitRow::Pair(Some(2), None));
        assert_eq!(m.split[5], SplitRow::Pair(None, Some(5)));
    }

    #[test]
    fn paired_replacements_get_word_ranges_but_pure_adds_do_not() {
        let f = file(vec![
            dl(LineKind::Del, Some(1), None, "let total = price * qty;"),
            dl(LineKind::Add, None, Some(1), "let total = price * count;"),
            dl(LineKind::Add, None, Some(2), "let extra = 1;"),
        ]);
        let m = DiffModel::build(f, 4);
        assert!(!m.lines[0].words.is_empty() && !m.lines[1].words.is_empty());
        assert!(m.lines[2].words.is_empty());
    }

    #[test]
    fn hunk_navigation_and_row_lookup() {
        let f = file(vec![dl(LineKind::Ctx, Some(1), Some(1), "a"), dl(LineKind::Add, None, Some(2), "b")]);
        let m = DiffModel::build(f, 4);
        assert_eq!(m.hunk_rows(false), vec![0]);
        assert_eq!(m.row_of_line(1, false), Some(2));
        assert_eq!(m.row_of_line(1, true), Some(2));
        assert_eq!(m.added_lines(0), vec!["b".to_string()]);
        assert!(m.hunk_text(1, 1).contains("+b"));
    }

    #[test]
    fn syntax_spans_follow_the_correct_side() {
        let f = file(vec![dl(LineKind::Del, Some(1), None, "let a = 1; // gone"), dl(LineKind::Add, None, Some(1), "let b = 2;")]);
        let m = DiffModel::build(f, 4);
        assert!(m.lines[0].spans.iter().any(|s| s.tok == highlight::Tok::Comment));
        assert!(!m.lines[1].spans.iter().any(|s| s.tok == highlight::Tok::Comment));
    }
}
