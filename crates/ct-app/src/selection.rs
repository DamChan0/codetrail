//! Line selection -> CodeRef (AC3). Pure functions, shared by diff / blame / search / editor.

use ct_core::{CodeRef, LineKind, RefAt};

/// What one selectable row maps to in file coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowLines {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
}

/// Inclusive row range (either order) of a diff selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowSel {
    pub a: usize,
    pub b: usize,
}
impl RowSel {
    pub fn single(i: usize) -> Self {
        RowSel { a: i, b: i }
    }
    pub fn lo(&self) -> usize {
        self.a.min(self.b)
    }
    pub fn hi(&self) -> usize {
        self.a.max(self.b)
    }
    pub fn contains(&self, i: usize) -> bool {
        (self.lo()..=self.hi()).contains(&i)
    }
}

/// Build the CodeRef for selected diff rows. New-side line numbers win (the commit/worktree
/// the user is looking at); a selection of only deleted lines refers to the old side at
/// `old_at` (`None` when the old side is not a single commit, e.g. the empty tree).
pub fn code_ref_for_rows(rows: &[RowLines], sel: RowSel, path: &str, old_path: Option<&str>, new_at: &RefAt, old_at: Option<&RefAt>) -> Option<CodeRef> {
    let slice = rows.get(sel.lo()..=sel.hi().min(rows.len().checked_sub(1)?))?;
    let new: Vec<u32> = slice.iter().filter_map(|r| r.new_no).collect();
    if let (Some(&s), Some(&e)) = (new.iter().min(), new.iter().max()) {
        return Some(CodeRef { path: path.to_string(), start: s, end: e, at: new_at.clone() });
    }
    let old: Vec<u32> = slice.iter().filter_map(|r| r.old_no).collect();
    let (s, e) = (*old.iter().min()?, *old.iter().max()?);
    Some(CodeRef { path: old_path.unwrap_or(path).to_string(), start: s, end: e, at: old_at?.clone() })
}

/// Selection over plain numbered lines (blame / search / editor).
pub fn code_ref_for_lines(path: &str, a: u32, b: u32, at: &RefAt) -> CodeRef {
    let (s, e) = (a.min(b).max(1), a.max(b).max(1));
    CodeRef { path: path.to_string(), start: s, end: e, at: at.clone() }
}

/// `@<sha>` suffix for a full oid: 12 hex chars (parseable by CodeRef, short enough to read).
pub fn at_commit(sha: &str) -> RefAt {
    RefAt::Commit(sha[..sha.len().min(12)].to_ascii_lowercase())
}

/// Diff column a selection was made in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Old,
    New,
    Unified,
}

/// What Ctrl+C and "Copy path:line" take from a code view selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopySel {
    pub path: String,
    pub start: u32,
    pub end: u32,
    /// Code only: no line numbers or diff markers.
    pub text: String,
}

/// The lines of `sel` shown in `pane`, joined without numbers or markers (`texts[i]` is row i's
/// raw text). Old pane: old-side numbers and `old_path`; new and unified panes: new-side numbers,
/// old side only when nothing on the new side is selected.
pub fn copy_for_rows(rows: &[RowLines], texts: &[&str], sel: RowSel, pane: Pane, path: &str, old_path: Option<&str>) -> Option<CopySel> {
    let hi = sel.hi().min(rows.len().checked_sub(1)?);
    let idx: Vec<usize> = (sel.lo()..=hi)
        .filter(|&i| match pane {
            Pane::Old => rows[i].old_no.is_some(),
            Pane::New => rows[i].new_no.is_some(),
            Pane::Unified => true,
        })
        .collect();
    let span = |old: bool| {
        let n = idx.iter().filter_map(|&i| if old { rows[i].old_no } else { rows[i].new_no });
        Some((n.clone().min()?, n.max()?))
    };
    let (old, (start, end)) = match pane {
        Pane::Old => (true, span(true)?),
        _ => span(false).map(|s| (false, s)).or_else(|| span(true).map(|s| (true, s)))?,
    };
    let text = idx.iter().map(|&i| texts[i].strip_suffix('\r').unwrap_or(texts[i])).collect::<Vec<_>>().join("\n");
    Some(CopySel { path: if old { old_path.unwrap_or(path) } else { path }.to_string(), start, end, text })
}

/// `path:12`, or `path:12-15` for a range.
pub fn path_line(path: &str, start: u32, end: u32) -> String {
    if start == end {
        format!("{path}:{start}")
    } else {
        format!("{path}:{start}-{end}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(kind: LineKind, o: Option<u32>, n: Option<u32>) -> RowLines {
        RowLines { kind, old_no: o, new_no: n }
    }
    fn rows() -> Vec<RowLines> {
        vec![
            r(LineKind::Ctx, Some(9), Some(9)),
            r(LineKind::Del, Some(10), None),
            r(LineKind::Del, Some(11), None),
            r(LineKind::Add, None, Some(10)),
            r(LineKind::Add, None, Some(11)),
            r(LineKind::Add, None, Some(12)),
            r(LineKind::Ctx, Some(12), Some(13)),
        ]
    }
    const SHA: &str = "abcdef0123456789abcdef0123456789abcdef01";

    #[test]
    fn mixed_selection_uses_new_side_numbers() {
        let c = code_ref_for_rows(&rows(), RowSel { a: 1, b: 4 }, "src/a.rs", None, &at_commit(SHA), None).unwrap();
        assert_eq!(c.to_string(), "src/a.rs:10-11@abcdef012345");
    }

    #[test]
    fn reversed_drag_selection_equals_forward() {
        let f = code_ref_for_rows(&rows(), RowSel { a: 3, b: 6 }, "a", None, &RefAt::Worktree, None);
        let b = code_ref_for_rows(&rows(), RowSel { a: 6, b: 3 }, "a", None, &RefAt::Worktree, None);
        assert_eq!(f, b);
        assert_eq!(f.unwrap().to_string(), "a:10-13@worktree");
    }

    #[test]
    fn deleted_only_selection_points_at_old_side_and_old_path() {
        let old = at_commit("1111111111111111111111111111111111111111");
        let c = code_ref_for_rows(&rows(), RowSel { a: 1, b: 2 }, "new.rs", Some("old.rs"), &RefAt::Worktree, Some(&old)).unwrap();
        assert_eq!((c.path.as_str(), c.start, c.end, c.at), ("old.rs", 10, 11, old));
    }

    #[test]
    fn deleted_only_without_resolvable_old_side_is_none() {
        assert!(code_ref_for_rows(&rows(), RowSel { a: 1, b: 2 }, "a", None, &RefAt::Worktree, None).is_none());
    }

    #[test]
    fn single_line_roundtrips_through_parse() {
        let c = code_ref_for_lines("한글 경로/파일 a.rs", 5, 5, &RefAt::Worktree);
        assert_eq!(c.to_string(), "한글 경로/파일 a.rs:5@worktree");
        assert_eq!(CodeRef::parse(&c.to_string()).unwrap(), c);
    }

    #[test]
    fn out_of_range_selection_is_none() {
        assert!(code_ref_for_rows(&rows(), RowSel { a: 50, b: 60 }, "a", None, &RefAt::Worktree, None).is_none());
        assert!(code_ref_for_rows(&[], RowSel::single(0), "a", None, &RefAt::Worktree, None).is_none());
    }

    #[test]
    fn line_selection_clamps_to_one_based() {
        let c = code_ref_for_lines("a", 0, 3, &RefAt::Worktree);
        assert_eq!((c.start, c.end), (1, 3));
    }

    const TEXTS: [&str; 7] = ["ctx9", "old10", "old11\r", "new10", "new11", "new12", "ctx13"];

    #[test]
    fn pane_copy_keeps_only_that_side_and_its_numbers() {
        let old = copy_for_rows(&rows(), &TEXTS, RowSel { a: 0, b: 2 }, Pane::Old, "new.rs", Some("old.rs")).unwrap();
        assert_eq!(old, CopySel { path: "old.rs".into(), start: 9, end: 11, text: "ctx9\nold10\nold11".into() });
        let new = copy_for_rows(&rows(), &TEXTS, RowSel { a: 6, b: 0 }, Pane::New, "new.rs", Some("old.rs")).unwrap();
        assert_eq!(new, CopySel { path: "new.rs".into(), start: 9, end: 13, text: "ctx9\nnew10\nnew11\nnew12\nctx13".into() });
    }

    #[test]
    fn unified_copy_takes_every_line_and_falls_back_to_old_numbers() {
        let u = copy_for_rows(&rows(), &TEXTS, RowSel { a: 1, b: 3 }, Pane::Unified, "a", None).unwrap();
        assert_eq!((u.start, u.end, u.text.as_str()), (10, 10, "old10\nold11\nnew10"));
        let d = copy_for_rows(&rows(), &TEXTS, RowSel { a: 1, b: 2 }, Pane::Unified, "a", None).unwrap();
        assert_eq!((d.start, d.end), (10, 11));
        assert!(copy_for_rows(&rows(), &TEXTS, RowSel { a: 3, b: 5 }, Pane::Old, "a", None).is_none(), "added lines have no old side");
    }

    #[test]
    fn path_line_formats_single_and_range() {
        assert_eq!(path_line("/r/a.cpp", 7, 7), "/r/a.cpp:7");
        assert_eq!(path_line("/r/a.cpp", 7, 9), "/r/a.cpp:7-9");
    }
}
