//! "Current" rail tab: local (uncommitted) changes grouped as Staged / Unstaged / Untracked.

use crate::app::{App, RailTab, TargetSel};
use crate::ui::status_style;
use crate::widgets::{self, Btn, BtnKind};
use crate::worktree::{Groups, WtView};
use crate::filter::{self, FileFilter, Stat};
use ct_core::FileStat;
use egui::{pos2, vec2, Rect, RichText, ScrollArea, Ui};
use std::collections::HashSet;

/// One line of the grouped list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    All,
    Header(WtView, usize),
    File(WtView, usize),
}

fn list_of(g: &Groups, v: WtView) -> &[FileStat] {
    match v {
        WtView::Staged => &g.staged,
        WtView::Unstaged => &g.unstaged,
        WtView::Untracked => &g.untracked,
        WtView::All => &[],
    }
}

/// Flat row model: "All changes", then each non-empty group as header + files passing `f`.
/// Groups whose files are all filtered out disappear. The totals count each path once.
pub fn rows(g: &Groups, f: &FileFilter) -> (Vec<Row>, Stat) {
    let mut out = vec![Row::All];
    let mut st = Stat::default();
    let mut seen: HashSet<&str> = HashSet::new();
    for v in [WtView::Staged, WtView::Unstaged, WtView::Untracked] {
        let list = list_of(g, v);
        let idx: Vec<usize> = (0..list.len()).filter(|&i| f.matches(&list[i].path)).collect();
        if idx.is_empty() {
            continue;
        }
        out.push(Row::Header(v, idx.len()));
        for i in idx {
            if seen.insert(list[i].path.as_str()) {
                st.files += 1;
                st.add += list[i].add;
                st.del += list[i].del;
            }
            out.push(Row::File(v, i));
        }
    }
    (out, st)
}

fn title(v: WtView) -> &'static str {
    match v {
        WtView::Staged => "Staged",
        WtView::Unstaged => "Unstaged",
        WtView::Untracked => "Untracked",
        WtView::All => "All changes",
    }
}

impl App {
    pub fn current_panel(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        let Some(sum) = self.wt.summary.clone() else {
            widgets::skeleton(ui, &th, 5, 28.0);
            return;
        };
        if !sum.dirty() {
            self.current_clean(ui);
            return;
        }
        let fresh = self.ensure_shown_wt();
        let active = self.flt_current.active();
        let st = if fresh && active { self.shown_wt.stat } else { Stat { files: sum.files, add: sum.add, del: sum.del } };
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            let label = if fresh || !active { filter::count_label(st.files, sum.files, active) } else { format!("Filtering {} files…", sum.files) };
            ui.label(RichText::new(label).font(widgets::ui_font(&th)).color(th.fg()));
            if fresh || !active {
                ui.label(RichText::new(format!("+{}", st.add)).font(widgets::mono_font(&th)).color(th.c(th.p().diff.add.fg)));
                ui.label(RichText::new(format!("−{}", st.del)).font(widgets::mono_font(&th)).color(th.c(th.p().diff.del.fg)));
            }
            if self.wt.paused {
                ui.label(RichText::new("auto-refresh paused · Ctrl+K → Reload").font(widgets::small_font(&th)).color(th.muted()));
            }
        });
        ui.add_space(m.space[0]);
        if filter::bar(ui, &th, &mut self.flt_current) {
            self.flt_current.sync();
        }
        let rows = if fresh { std::mem::take(&mut self.shown_wt.items) } else { Vec::new() };
        let on_wt = matches!(self.target, Some(TargetSel::Worktree));
        let (view, sel) = (self.wt_view, self.file_sel.clone());
        let mut pick: Option<(WtView, Option<String>)> = None;
        let row_h = 28.0;
        if !fresh {
            ui.label(RichText::new("Filtering…").font(widgets::small_font(&th)).color(th.muted()));
        } else if rows.len() == 1 && active {
            widgets::empty_state(ui, &th, "No matching files", "Clear the filter to see all changes.");
        }
        ScrollArea::vertical().id_salt("current_list").auto_shrink([false, false]).show_rows(ui, row_h, rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for r in &rows[range] {
                match *r {
                    Row::All => {
                        let (resp, rect) = widgets::list_row(ui, &th, row_h, on_wt && view == WtView::All);
                        widgets::paint_text_fit(ui, Rect::from_min_max(rect.min + vec2(8.0, 0.0), rect.max), "All changes", widgets::ui_font(&th), th.fg());
                        widgets::paint_text_right(ui, rect, "HEAD → working tree", widgets::small_font(&th), th.muted());
                        if resp.clicked() {
                            pick = Some((WtView::All, None));
                        }
                    }
                    Row::Header(v, n) => {
                        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), egui::Sense::hover());
                        ui.painter().text(pos2(rect.min.x + 8.0, rect.center().y), egui::Align2::LEFT_CENTER, format!("{} · {n}", title(v)), widgets::small_font(&th), th.muted());
                    }
                    Row::File(v, i) => {
                        let f = &list_of(&sum.groups, v)[i];
                        let selected = on_wt && view == v && sel.as_deref() == Some(f.path.as_str());
                        let (resp, rect) = widgets::list_row(ui, &th, row_h, selected);
                        let (letter, col) = if v == WtView::Untracked { ("?", th.muted()) } else { status_style(&th, f.status) };
                        let lr = Rect::from_min_size(rect.min, vec2(22.0, row_h));
                        ui.painter().text(lr.center(), egui::Align2::CENTER_CENTER, letter, widgets::mono_font(&th), col);
                        let stat = if f.binary { "bin".to_string() } else { format!("+{} −{}", f.add, f.del) };
                        let sw = widgets::paint_text_right(ui, rect, &stat, widgets::small_font(&th), th.muted());
                        let label = match &f.old_path {
                            Some(o) => format!("{o} → {}", f.path),
                            None => f.path.clone(),
                        };
                        let area = Rect::from_min_max(pos2(rect.min.x + 26.0, rect.min.y), pos2(rect.max.x - sw - 8.0, rect.max.y));
                        let shown = filter::path_label(ui, &widgets::ui_font(&th), &label, area.width());
                        widgets::paint_text_fit(ui, area, &shown, widgets::ui_font(&th), th.fg());
                        if resp.clicked() {
                            pick = Some((v, Some(f.path.clone())));
                        }
                        resp.on_hover_text(&label);
                    }
                }
            }
        });
        if fresh {
            self.shown_wt.items = rows;
        }
        match pick {
            Some((WtView::All, _)) => {
                if self.centre == crate::app::Centre::Run {
                    self.centre = crate::app::Centre::Diff;
                }
                self.select_worktree();
            }
            Some((v, Some(p))) => {
                if self.centre == crate::app::Centre::Run {
                    self.centre = crate::app::Centre::Diff;
                }
                self.select_wt_file(v, &p);
            }
            _ => {}
        }
    }

    /// Clean tree: no stale diff, just where to go next.
    fn current_clean(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        widgets::empty_state(ui, &th, "No local changes", "Everything is committed.");
        if let Some(c) = self.commits.iter().find(|c| c.sha == self.head).or(self.commits.first()).cloned() {
            ui.add_space(th.m().space[2]);
            let (_, rect) = widgets::list_row(ui, &th, 44.0, false);
            let top = Rect::from_min_max(rect.min + vec2(12.0, 4.0), pos2(rect.max.x - 8.0, rect.min.y + 22.0));
            let bot = Rect::from_min_max(pos2(rect.min.x + 12.0, rect.min.y + 22.0), pos2(rect.max.x - 8.0, rect.max.y - 2.0));
            widgets::paint_text_fit(ui, top, &c.subject, widgets::ui_font(&th), th.fg());
            let meta = format!("{} · {}", crate::timefmt::short(&c.sha), crate::timefmt::rel(crate::timefmt::now(), c.time));
            widgets::paint_text_fit(ui, bot, &meta, widgets::small_font(&th), th.muted());
        }
        ui.add_space(th.m().space[2]);
        ui.vertical_centered(|ui| {
            if Btn::new("Review last commit").kind(BtnKind::Primary).show(ui, &th).clicked() {
                self.rail = RailTab::Commits;
                let head = self.head.clone();
                if !head.is_empty() {
                    self.target = None;
                    self.select_commit(&head);
                    self.jump_to_commit(&head);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ct_core::{FileKind, Status};

    fn f(p: &str) -> FileStat {
        FileStat { path: p.into(), old_path: None, status: Status::Modified, similarity: None, add: 1, del: 0, binary: false, kind: FileKind::Text, old_mode: None, new_mode: None }
    }

    #[test]
    fn rows_group_with_counts_and_a_file_can_appear_twice() {
        let g = Groups { staged: vec![f("a"), f("b")], unstaged: vec![f("a")], untracked: vec![] };
        let (r, st) = rows(&g, &FileFilter::default());
        assert_eq!(r, vec![Row::All, Row::Header(WtView::Staged, 2), Row::File(WtView::Staged, 0), Row::File(WtView::Staged, 1), Row::Header(WtView::Unstaged, 1), Row::File(WtView::Unstaged, 0)]);
        assert_eq!(st.files, 2, "a path in two groups counts once");
    }

    #[test]
    fn filter_drops_empty_groups_but_keeps_all_changes() {
        let g = Groups { staged: vec![f("src/a.rs")], unstaged: vec![f("docs/b.md")], untracked: vec![f("src/c.rs")] };
        let (r, st) = rows(&g, &FileFilter::parse("SRC"));
        assert_eq!(r, vec![Row::All, Row::Header(WtView::Staged, 1), Row::File(WtView::Staged, 0), Row::Header(WtView::Untracked, 1), Row::File(WtView::Untracked, 0)]);
        assert_eq!((st.files, st.add), (2, 2));
        assert_eq!(rows(&g, &FileFilter::parse("zzz")).0, vec![Row::All]);
        assert_eq!(rows(&g, &FileFilter::parse("src !c.rs")).0.len(), 3);
    }
}
