//! Centre views (diff, blame, editor), inspector (Why / Blame / Ask AI), Ask preview dialog, smoke mode.

use crate::app::*;
use crate::diffmodel::{cols, DiffModel, SplitRow, UnifiedRow};
use crate::editor::ReadOnly;
use crate::highlight::{self, Span};
use crate::settings::ViewMode;
use crate::selection::{code_ref_for_lines, code_ref_for_rows, RowSel};
use crate::theme::Theme;
use crate::widgets::{self, Btn, BtnKind, Icon, Level};
use ct_core::{LineKind, RefAt};
use ct_store::Confidence;
use egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use egui::{pos2, vec2, Align, Color32, FontId, Layout, Rect, RichText, ScrollArea, Sense, Ui};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn line_job(th: &Theme, text: &str, spans: &[Span], font: &FontId) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let mut at = 0usize;
    let fmt = |c: Color32| TextFormat { font_id: font.clone(), color: c, ..Default::default() };
    for s in spans {
        if s.start > at && text.is_char_boundary(at) && text.is_char_boundary(s.start) {
            job.append(&text[at..s.start], 0.0, fmt(th.fg()));
        }
        if s.end <= text.len() && s.start < s.end && text.is_char_boundary(s.start) && text.is_char_boundary(s.end) {
            job.append(&text[s.start..s.end], 0.0, fmt(highlight::color(th, s.tok)));
            at = s.end;
        }
    }
    if at < text.len() && text.is_char_boundary(at) {
        job.append(&text[at..], 0.0, fmt(th.fg()));
    }
    if text.is_empty() {
        job.append("", 0.0, fmt(th.fg()));
    }
    job
}

fn x_of(g: &egui::Galley, text: &str, byte: usize) -> f32 {
    let b = byte.min(text.len());
    let b = (0..=b).rev().find(|i| text.is_char_boundary(*i)).unwrap_or(0);
    g.pos_from_ccursor(CCursor::new(text[..b].chars().count())).min.x
}

impl App {
    // ------------------------------------------------------------ centre

    pub fn centre_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        if let Loadable::Failed(e) = &self.open_state {
            ui.add_space(m.space[2]);
            widgets::banner(ui, &th, Level::Error, &format!("Cannot open repository: {e}"), None, false);
            widgets::empty_state(ui, &th, "No repository", "Start CodeTrail inside a git repository, or pass its path as an argument.");
            return;
        }
        if self.repo_arg.as_os_str().is_empty() {
            widgets::empty_state(ui, &th, "No project open", "Press Ctrl+O or click the project name to choose a git repository.");
            ui.vertical_centered(|ui| {
                if Btn::new("Open folder…").kind(BtnKind::Primary).show(ui, &th).clicked() {
                    self.pj_open_browser();
                }
            });
            return;
        }
        if self.rail == RailTab::Current && self.centre == Centre::Diff && self.wt.summary.as_ref().is_some_and(|s| !s.dirty()) {
            widgets::empty_state(ui, &th, "No local changes", "Everything is committed. Use Review last commit, or pick a commit in Commits.");
            return;
        }
        if self.centre == Centre::Run {
            self.run_ui(ui);
            return;
        }
        // Header: file path + view switch.
        egui::Frame::new().fill(th.raised()).stroke(egui::Stroke::new(1.0, th.border())).inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
            ui.set_height(m.control_height);
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = m.space[1];
                let name = match self.centre {
                    Centre::Editor => self.editor.as_ref().map(|e| e.path.clone()),
                    Centre::Blame => Some(self.blame.path.clone()).filter(|p| !p.is_empty()),
                    Centre::Diff | Centre::Run => self.file_sel.clone(),
                };
                let w = (ui.available_width() - 330.0).max(80.0);
                match &name {
                    Some(n) => {
                        let g = widgets::fit(ui, n, widgets::ui_font(&th), th.fg(), w);
                        ui.label(g);
                        if let Some(e) = &self.editor {
                            if self.centre == Centre::Editor && e.dirty() {
                                widgets::badge(ui, &th, "modified", th.warn());
                            }
                        }
                    }
                    None => {
                        ui.label(RichText::new("No file selected").color(th.muted()));
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Shows the stored preference, not the (possibly fallback) effective mode.
                    let mut i = match self.settings.view_mode {
                        _ if self.tt.is_some() => 1,
                        ViewMode::Diff => 0,
                        ViewMode::Blame => 1,
                        ViewMode::Edit => 2,
                    };
                    if widgets::segmented(ui, &th, &["Diff", "Blame", "Edit"], &mut i) {
                        self.set_view_pref([ViewMode::Diff, ViewMode::Blame, ViewMode::Edit][i]);
                    }
                    if let Some(note) = self.view_note {
                        widgets::badge(ui, &th, note, th.muted());
                    }
                });
            });
        });
        if self.tt.is_some() {
            self.tt_banner(ui);
            if self.tt.as_ref().is_some_and(|t| t.missing) {
                self.tt_missing_ui(ui);
                return;
            }
        }
        match self.centre {
            Centre::Diff | Centre::Run => self.diff_ui(ui),
            Centre::Blame => self.blame_ui(ui),
            Centre::Editor => self.editor_ui(ui),
        }
    }

    // ------------------------------------------------------------ diff

    fn diff_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        match &self.diffset {
            Loadable::Idle | Loadable::Loading => {
                widgets::skeleton(ui, &th, 12, 22.0);
                return;
            }
            Loadable::Failed(e) => {
                let e = e.clone();
                ui.add_space(12.0);
                if widgets::banner(ui, &th, Level::Error, &format!("Could not compute the diff: {e}"), Some("Retry"), false) == widgets::BannerAction::Action {
                    self.request_diff();
                }
                return;
            }
            Loadable::Ready(set) if set.files.is_empty() => {
                widgets::empty_state(ui, &th, "No changes", "This comparison has no differences. Pick another commit or change the base.");
                return;
            }
            Loadable::Ready(_) => {}
        }
        match &self.prepared {
            Loadable::Idle => {
                widgets::empty_state(ui, &th, "Select a file", "Choose a changed file on the left.");
                return;
            }
            Loadable::Loading => {
                widgets::skeleton(ui, &th, 12, 22.0);
                return;
            }
            Loadable::Failed(e) => {
                let e = e.clone();
                ui.add_space(12.0);
                if widgets::banner(ui, &th, Level::Error, &format!("Could not load this file's diff: {e}"), Some("Retry"), false) == widgets::BannerAction::Action {
                    self.request_file_diff();
                }
                return;
            }
            Loadable::Ready(_) => {}
        }
        let Loadable::Ready(p) = std::mem::take(&mut self.prepared) else { return };
        self.diff_body(ui, &th, &p);
        self.prepared = Loadable::Ready(p);
    }

    fn diff_body(&mut self, ui: &mut Ui, th: &Theme, p: &Prepared) {
        use ct_core::FileKind;
        let model = &p.model;
        let f = &model.file;
        if f.kind != FileKind::Text {
            let what = match f.kind {
                FileKind::Binary => "Binary file",
                FileKind::Symlink => "Symbolic link",
                FileKind::Submodule => "Submodule",
                FileKind::Text => "",
            };
            widgets::empty_state(ui, th, &format!("{what} changed"), "No text diff is shown for this kind of file.");
            return;
        }
        if model.lines.is_empty() {
            widgets::empty_state(ui, th, "No textual changes", "Mode or whitespace-only change, or an empty file.");
            return;
        }
        if model.lines.len() > crate::diffmodel::LARGE_DIFF_LINES && !self.show_large {
            ui.add_space(24.0);
            widgets::banner(ui, th, Level::Warn, &format!("This diff has {} lines. Rendering it is collapsed to keep the app responsive.", model.lines.len()), None, false);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                if Btn::new("Show anyway").show(ui, th).clicked() {
                    self.show_large = true;
                }
            });
            return;
        }
        let split = self.settings.split_view;
        let mono = widgets::mono_font(th);
        let row_h = th.m().diff_row_height.max((th.code_font * 1.55).round());
        let cw = ui.fonts(|fo| fo.glyph_width(&mono, '0')).max(4.0);
        let digits = f.hunks.iter().map(|h| h.old_start.max(h.new_start) + h.old_len.max(h.new_len)).max().unwrap_or(1).to_string().len().max(3);
        let num_w = digits as f32 * cw + 14.0;
        let avail = ui.available_width();
        let text_w = model.max_cols as f32 * cw + 24.0;
        let n_rows = model.row_count(split);
        let width = if split { avail } else { avail.max(2.0 * num_w + 18.0 + text_w) };
        let rows_lines = model.row_lines();
        let cur = self.cur.clone();
        let mut area = ScrollArea::both().id_salt("diff_scroll").auto_shrink([false, false]);
        let want = self.diff_scroll_to.take();
        if let Some(r) = want {
            area = area.vertical_scroll_offset((r as f32 * row_h - row_h * 1.5).max(0.0));
        }
        let mut new_sel: Option<RowSel> = self.diff_sel;
        let mut anchor = self.diff_anchor;
        let mut top_row = self.diff_top_row;
        let shift = ui.input(|i| i.modifiers.shift);
        let sel_now = self.diff_sel;
        let line_of = |r: usize| -> Option<usize> {
            if split {
                match model.split[r] {
                    SplitRow::Pair(l, rr) => rr.or(l),
                    SplitRow::Hunk(_) => None,
                }
            } else {
                match model.unified[r] {
                    UnifiedRow::Line(i) => Some(i),
                    UnifiedRow::Hunk(_) => None,
                }
            }
        };
        let _ = &cur;
        area.show_rows(ui, row_h, n_rows, |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            top_row = range.start;
            let first_top = ui.cursor().top();
            for r in range.clone() {
                let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), Sense::click_and_drag());
                let clip = rect.intersect(ui.clip_rect());
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let painter = ui.painter().with_clip_rect(clip);
                let hunk_row = if split { matches!(model.split[r], SplitRow::Hunk(_)) } else { matches!(model.unified[r], UnifiedRow::Hunk(_)) };
                if hunk_row {
                    let h = match if split { model.split[r] } else { model.split.first().copied().unwrap_or(SplitRow::Hunk(0)) } {
                        SplitRow::Hunk(h) => h,
                        _ => 0,
                    };
                    let h = if split { h } else if let UnifiedRow::Hunk(h) = model.unified[r] { h } else { 0 };
                    painter.rect_filled(rect, 0.0, th.raised());
                    let vis_left = ui.clip_rect().left();
                    let tx = rect.min.x.max(vis_left) + 12.0;
                    painter.text(pos2(tx, rect.center().y), egui::Align2::LEFT_CENTER, &model.hunk_headers[h], widgets::small_font(th), th.muted());
                    continue;
                }
                let pointer_row = |resp: &egui::Response| -> Option<usize> {
                    let pos = resp.interact_pointer_pos()?;
                    let rr = range.start + ((pos.y - first_top) / row_h).floor().max(0.0) as usize;
                    Some(rr.min(n_rows - 1))
                };
                if resp.clicked() || resp.drag_started() {
                    if let Some(i) = line_of(r) {
                        match (shift, anchor) {
                            (true, Some(a)) => new_sel = Some(RowSel { a, b: i }),
                            _ => {
                                anchor = Some(i);
                                new_sel = Some(RowSel::single(i));
                            }
                        }
                    }
                } else if resp.dragged() {
                    if let (Some(a), Some(pr)) = (anchor, pointer_row(&resp)) {
                        if let Some(i) = line_of(pr) {
                            new_sel = Some(RowSel { a, b: i });
                        }
                    }
                }
                let in_sel = |i: usize| sel_now.is_some_and(|s| s.contains(i)) || new_sel.is_some_and(|s| s.contains(i));
                if split {
                    if let SplitRow::Pair(l, rr) = model.split[r] {
                        let half = (rect.width() / 2.0).floor();
                        let lrect = Rect::from_min_size(rect.min, vec2(half, row_h));
                        let rrect = Rect::from_min_size(pos2(rect.min.x + half, rect.min.y), vec2(rect.width() - half, row_h));
                        paint_cell(ui, &painter, th, model, l, lrect, num_w, cw, &mono, true, in_sel);
                        paint_cell(ui, &painter, th, model, rr, rrect, num_w, cw, &mono, false, in_sel);
                        painter.line_segment([rrect.left_top(), rrect.left_bottom()], egui::Stroke::new(1.0, th.border()));
                    }
                } else if let UnifiedRow::Line(i) = model.unified[r] {
                    paint_unified(ui, &painter, th, model, i, rect, num_w, cw, &mono, in_sel(i));
                }
            }
        });
        self.diff_top_row = top_row;
        self.diff_anchor = anchor;
        if new_sel != self.diff_sel {
            self.diff_sel = new_sel;
            if let (Some(s), Some(cur), Some(path)) = (new_sel, self.cur.clone(), self.file_sel.clone()) {
                let hi = s.hi().min(model.lines.len().saturating_sub(1));
                let lo = s.lo().min(hi);
                let new_at = cur.new_at.clone().unwrap_or(RefAt::Worktree);
                if let Some(cr) = code_ref_for_rows(&rows_lines, s, &path, f.old_path.as_deref(), &new_at, cur.old_at.as_ref()) {
                    let text = model.lines[lo..=hi].iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
                    self.set_selection(cr, SelSource::Diff, text);
                }
            }
        }
    }

    // ------------------------------------------------------------ blame

    fn blame_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        if self.blame.path.is_empty() {
            widgets::empty_state(ui, &th, "No file for blame", "Pick a file with Ctrl+P, then choose Blame.");
            return;
        }
        let data = match &self.blame.data {
            Loadable::Idle | Loadable::Loading => {
                widgets::skeleton(ui, &th, 12, 22.0);
                return;
            }
            Loadable::Failed(e) => {
                let e = e.clone();
                ui.add_space(12.0);
                if widgets::banner(ui, &th, Level::Error, &format!("Blame failed: {e}"), Some("Retry"), false) == widgets::BannerAction::Action {
                    let p = self.blame.path.clone();
                    self.blame.data = Loadable::Idle;
                    self.open_blame(&p, None);
                }
                return;
            }
            Loadable::Ready(d) => d.clone(),
        };
        if data.lines.is_empty() {
            widgets::empty_state(ui, &th, "Empty file", "There are no lines to blame.");
            return;
        }
        let mono = widgets::mono_font(&th);
        let row_h = th.m().diff_row_height.max((th.code_font * 1.55).round());
        let cw = ui.fonts(|fo| fo.glyph_width(&mono, '0')).max(4.0);
        let gutter = 260.0;
        let n = data.lines.len();
        let mut area = ScrollArea::both().id_salt("blame_scroll").auto_shrink([false, false]);
        if let Some(y) = self.blame.apply_y.take() {
            area = area.vertical_scroll_offset(y);
            self.blame.scroll_to = None;
        } else if let Some(l) = self.blame.scroll_to.take() {
            area = area.vertical_scroll_offset((l.saturating_sub(1) as f32 * row_h - row_h * 3.0).max(0.0));
        }
        let max_cols = data.lines.iter().map(|l| cols(&l.text)).max().unwrap_or(1);
        let width = ui.available_width().max(gutter + 50.0 + max_cols as f32 * cw + 24.0);
        let now = crate::timefmt::now();
        let mut click: Option<usize> = None;
        let sel = self.blame.selected.map(|(a, b)| (a.min(b), a.max(b)));
        let mut prev_sha_row: Option<usize> = None;
        let mut gutter_click: Option<usize> = None;
        let out = area.show_rows(ui, row_h, n, |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in range {
                let l = &data.lines[i];
                let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), Sense::click());
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let p = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                let selected = sel.is_some_and(|(a, b)| i >= a && i <= b);
                if selected {
                    p.rect_filled(rect, 0.0, th.selection());
                } else if resp.hovered() {
                    p.rect_filled(rect, 0.0, th.hover());
                }
                p.rect_filled(Rect::from_min_size(rect.min, vec2(gutter, row_h)), 0.0, th.raised());
                let first_of_block = i == 0 || data.lines[i - 1].sha != l.sha;
                let _ = prev_sha_row;
                prev_sha_row = Some(i);
                let uncommitted = l.sha.bytes().all(|b| b == b'0');
                if first_of_block {
                    let who = if uncommitted { "Uncommitted".to_string() } else { format!("{}  {}", crate::timefmt::short(&l.sha), l.author) };
                    let r = Rect::from_min_max(pos2(rect.min.x + 8.0, rect.min.y), pos2(rect.min.x + gutter - 64.0, rect.max.y));
                    widgets::paint_text_fit(ui, r, &who, widgets::small_font(&th), if uncommitted { th.warn() } else { th.muted() });
                    if !uncommitted {
                        widgets::paint_text_right(ui, Rect::from_min_max(rect.min, pos2(rect.min.x + gutter - 8.0, rect.max.y)), &crate::timefmt::rel(now, l.time), widgets::small_font(&th), th.muted());
                    }
                    p.line_segment([rect.left_top(), pos2(rect.min.x + gutter, rect.min.y)], egui::Stroke::new(1.0, th.border()));
                }
                p.text(pos2(rect.min.x + gutter + 40.0, rect.center().y), egui::Align2::RIGHT_CENTER, l.line_no.to_string(), mono.clone(), th.muted());
                let job = line_job(&th, &l.text, &data.spans[i], &mono);
                let g = ui.fonts(|f| f.layout_job(job));
                p.galley(pos2(rect.min.x + gutter + 50.0, rect.center().y - g.size().y / 2.0), g, th.fg());
                if resp.clicked() {
                    if resp.interact_pointer_pos().is_some_and(|pp| pp.x < rect.min.x + gutter) {
                        gutter_click = Some(i);
                    }
                    click = Some(i);
                }
            }
        });
        self.blame.scroll_y = out.state.offset.y;
        if self.tt.is_some() {
            // Marker for the tracked lines along the scrollbar side.
            if let Some((a, b)) = sel {
                let r = out.inner_rect;
                let h = r.height();
                let y0 = r.min.y + a as f32 / n as f32 * h;
                let hh = ((b - a + 1) as f32 / n as f32 * h).max(3.0);
                ui.painter().rect_filled(Rect::from_min_size(pos2(r.max.x - 4.0, y0), vec2(3.0, hh)), 1.0, th.accent());
            }
            if let Some(i) = gutter_click {
                self.tt_from_blame_line(i);
                return;
            }
        }
        if let Some(i) = click {
            let shift = ui.input(|i| i.modifiers.shift);
            let a = match (shift, self.blame.selected) {
                (true, Some((a, _))) => a,
                _ => i,
            };
            self.blame.selected = Some((a, i));
            let (lo, hi) = (a.min(i), a.max(i));
            let (ln_lo, ln_hi) = (data.lines[lo].line_no, data.lines[hi].line_no);
            let text = data.lines[lo..=hi].iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
            let at = match &self.blame.rev {
                Some(r) => RefAt::Commit(r.clone()),
                None => RefAt::Worktree,
            };
            let cr = code_ref_for_lines(&self.blame.path.clone(), ln_lo, ln_hi, &at);
            self.set_selection(cr, SelSource::Blame, text);
        }
    }

    // ------------------------------------------------------------ editor

    fn editor_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        let Some(e) = &mut self.editor else {
            widgets::empty_state(ui, &th, "No file open", "Open a file with Ctrl+P, from the Files tab, or from a search result.");
            return;
        };
        match &e.load {
            Loadable::Idle | Loadable::Loading => {
                widgets::skeleton(ui, &th, 12, 22.0);
                return;
            }
            Loadable::Failed(err) => {
                let err = err.clone();
                ui.add_space(12.0);
                if widgets::banner(ui, &th, Level::Error, &err, Some("Retry"), false) == widgets::BannerAction::Action {
                    self.reload_editor();
                }
                return;
            }
            Loadable::Ready(()) => {}
        }
        let mut act: Option<&str> = None;
        if let Some(c) = &e.conflict {
            let msg = match c {
                crate::editor::Disk::Gone => "This file was deleted on disk. Your copy is still here.",
                _ => "This file changed on disk since you opened it.",
            };
            ui.add_space(4.0);
            match widgets::banner(ui, &th, Level::Warn, msg, Some("Reload from disk"), false) {
                widgets::BannerAction::Action => act = Some("reload"),
                _ => {}
            }
            if !matches!(c, crate::editor::Disk::Gone) || true {
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    if Btn::new("Overwrite disk with my copy").show(ui, &th).clicked() {
                        act = Some("force");
                    }
                });
            }
        }
        if let Some(ro) = &e.read_only {
            widgets::banner(ui, &th, Level::Info, &format!("Read-only: {}", ro.reason()), None, false);
        } else if let Some((msg, t)) = &e.notice {
            if t.elapsed() < Duration::from_secs(6) {
                widgets::banner(ui, &th, Level::Info, msg, None, false);
            }
        }
        // Go to line bar.
        if e.goto_open {
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.label(RichText::new("Go to line").color(th.muted()));
                let r = ui.add_sized([90.0, m.control_height], egui::TextEdit::singleline(&mut e.goto_input).margin(vec2(8.0, 6.0)));
                r.request_focus();
                if r.lost_focus() {
                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        if let Ok(n) = e.goto_input.trim().parse::<u32>() {
                            e.goto = Some(n.max(1));
                        }
                    }
                    e.goto_open = false;
                    e.goto_input.clear();
                }
            });
        }
        let mono = widgets::mono_font(&th);
        let editable = e.read_only.is_none();
        let key = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            e.text.hash(&mut h);
            (th.file.is_dark(), th.code_font as u32).hash(&mut h);
            h.finish()
        };
        let lang = highlight::lang_for_path(&e.path);
        let id = egui::Id::new("editor_text");
        let goto = e.goto.take();
        if let Some(line) = goto {
            let byte = crate::editor::line_start_offset(&e.text, line);
            let ci = e.text[..byte.min(e.text.len())].chars().count();
            let mut st = egui::text_edit::TextEditState::load(ui.ctx(), id).unwrap_or_default();
            st.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(ci))));
            st.store(ui.ctx(), id);
            ui.ctx().memory_mut(|mm| mm.request_focus(id));
        }
        let mut cache = e.layout_cache.take();
        let mut out_cursor: Option<usize> = None;
        let mut scroll_rect: Option<Rect> = None;
        let th2 = th.clone();
        ScrollArea::both().id_salt("editor_scroll").auto_shrink([false, false]).show(ui, |ui| {
            let mut layouter = |ui: &Ui, text: &str, _wrap: f32| -> Arc<egui::Galley> {
                let job = match &cache {
                    Some((k, j)) if *k == key => j.clone(),
                    _ => {
                        let lines: Vec<&str> = text.split('\n').collect();
                        let spans = highlight::highlight(lang, &lines);
                        let mut job = LayoutJob::default();
                        job.wrap.max_width = f32::INFINITY;
                        let n = lines.len();
                        for (i, l) in lines.iter().enumerate() {
                            let sub = line_job(&th2, l, &spans[i], &mono);
                            for s in sub.sections {
                                job.append(&sub.text[s.byte_range.clone()], 0.0, s.format.clone());
                            }
                            if i + 1 < n {
                                job.append("\n", 0.0, TextFormat { font_id: mono.clone(), color: th2.fg(), ..Default::default() });
                            }
                        }
                        cache = Some((key, job.clone()));
                        job
                    }
                };
                ui.fonts(|f| f.layout_job(job))
            };
            let te = egui::TextEdit::multiline(&mut e.text).id(id).font(FontId::monospace(th.code_font)).code_editor().desired_width(f32::INFINITY).desired_rows(30).interactive(editable || true).layouter(&mut layouter).frame(false).margin(egui::Margin::symmetric(12, 8));
            let out = if editable { te.show(ui) } else { egui::TextEdit::multiline(&mut e.text.as_str()).id(id).font(FontId::monospace(th.code_font)).code_editor().desired_width(f32::INFINITY).desired_rows(30).layouter(&mut layouter).frame(false).margin(egui::Margin::symmetric(12, 8)).show(ui) };
            if let Some(cr) = out.cursor_range {
                out_cursor = Some(cr.primary.ccursor.index);
                if goto.is_some() {
                    let r = out.galley.pos_from_ccursor(cr.primary.ccursor);
                    scroll_rect = Some(r.translate(out.galley_pos.to_vec2()));
                }
            }
            if let Some(r) = scroll_rect {
                ui.scroll_to_rect(r.expand2(vec2(0.0, 80.0)), Some(Align::Center));
            }
        });
        e.layout_cache = cache;
        if let Some(ci) = out_cursor {
            if ci as u64 != e.spans_key {
                e.spans_key = ci as u64;
                e.cursor_line = 1 + e.text.chars().take(ci).filter(|c| *c == '\n').count() as u32;
            }
        }
        match act {
            Some("reload") => self.reload_editor(),
            Some("force") => self.save_editor(true),
            _ => {}
        }
        let _ = ReadOnly::Binary;
    }

    // ------------------------------------------------------------ inspector

    pub fn inspector_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        ui.add_space(m.space[1]);
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            let mut i = match self.insp {
                InspTab::Why => 0,
                InspTab::Blame => 1,
                InspTab::Ask => 2,
            };
            if widgets::segmented(ui, &th, &["Why", "Blame", "Ask AI"], &mut i) {
                self.insp = [InspTab::Why, InspTab::Blame, InspTab::Ask][i];
            }
        });
        ui.add_space(m.space[1]);
        // Selection header.
        match self.selection.clone() {
            Some(sel) => {
                ui.horizontal(|ui| {
                    ui.add_space(m.space[2]);
                    let w = ui.available_width() - 44.0;
                    let g = widgets::fit(ui, &sel.code_ref.to_string(), widgets::mono_font(&th), th.accent(), w);
                    ui.label(g);
                    if widgets::icon_button(ui, &th, Icon::Copy, "Copy reference (Ctrl+Shift+C)").clicked() {
                        self.copy_ref();
                    }
                });
            }
            None => {
                ui.horizontal(|ui| {
                    ui.add_space(m.space[2]);
                    ui.label(RichText::new("No selection").font(widgets::ui_font(&th)).color(th.muted()));
                });
            }
        }
        ui.add_space(m.space[1]);
        ui.separator();
        match self.insp {
            InspTab::Why => self.why_ui(ui),
            InspTab::Blame => self.blame_tab_ui(ui),
            InspTab::Ask => self.ask_ui(ui),
        }
    }

    fn why_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        if self.store.is_none() {
            let note = self.store_note.clone().unwrap_or_default();
            if self.open_state.is_loading() {
                widgets::skeleton(ui, &th, 3, 40.0);
            } else {
                widgets::banner(ui, &th, Level::Warn, &format!("Reason store unavailable. {note}"), None, false);
            }
            return;
        }
        if self.selection.is_none() && self.why.is_empty() {
            widgets::empty_state(ui, &th, "Select lines to see why", "Click a line in the diff, or drag over several.");
            return;
        }
        if self.why.is_empty() {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                widgets::badge(ui, &th, "Unknown", th.muted());
            });
            widgets::empty_state(ui, &th, "No recorded reason", "No agent edit is linked to these lines. Edits made through an installed agent hook show up here.");
            return;
        }
        let now = crate::timefmt::now();
        ScrollArea::vertical().id_salt("why_scroll").auto_shrink([false, false]).show(ui, |ui| {
            for item in &self.why {
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    let (label, col) = match item.conf {
                        Some(Confidence::High) => ("High", th.c(th.p().diff.add.fg)),
                        Some(Confidence::Medium) => ("Medium", th.warn()),
                        Some(Confidence::Unknown) => ("Unknown", th.muted()),
                        None => ("Overlaps", th.muted()),
                    };
                    widgets::badge(ui, &th, label, col);
                    let who = format!("{} · {}", item.rec.agent.name(), crate::timefmt::rel(now, (item.rec.ts_ms / 1000) as i64));
                    ui.label(RichText::new(who).font(widgets::small_font(&th)).color(th.muted())).on_hover_text(crate::timefmt::abs((item.rec.ts_ms / 1000) as i64));
                });
                egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 0)).show(ui, |ui| {
                    let text = if item.reason.trim().is_empty() { "No reason was recorded for this edit.".to_string() } else { item.reason.clone() };
                    ui.add(egui::Label::new(RichText::new(text).font(widgets::ui_font(&th)).color(th.fg())).wrap());
                });
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(RichText::new(format!("{} · {}", item.rec.tool, crate::timefmt::short(&item.rec.head_at_edit))).font(widgets::small_font(&th)).color(th.muted()));
                });
                ui.add_space(8.0);
                ui.separator();
            }
        });
    }

    fn blame_tab_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let Some(sel) = self.selection.clone() else {
            widgets::empty_state(ui, &th, "Select a line", "Blame details and line history appear here.");
            return;
        };
        let line = sel.code_ref.start;
        let path = sel.code_ref.path.clone();
        let tt = self.tt.is_some();
        let hist_path = self.history.path.clone();
        if !tt && (self.blame.path != path || self.blame.data.ready().is_none()) {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                if Btn::new("Open blame view").icon(Icon::Blame).show(ui, &th).clicked() {
                    self.open_blame(&path, Some(line as usize));
                }
            });
        }
        let info = self.blame.data.ready().filter(|_| !tt && self.blame.path == path).and_then(|d| d.lines.iter().find(|l| l.line_no == line)).cloned();
        let now = crate::timefmt::now();
        if let Some(l) = info {
            ui.add_space(12.0);
            let unc = l.sha.bytes().all(|b| b == b'0');
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                if unc {
                    widgets::badge(ui, &th, "Uncommitted", th.warn());
                } else {
                    ui.label(RichText::new(crate::timefmt::short(&l.sha)).font(widgets::mono_font(&th)).color(th.accent()));
                    ui.label(RichText::new(format!("{} · {}", l.author, crate::timefmt::rel(now, l.time))).font(widgets::small_font(&th)).color(th.muted()));
                }
            });
            if !unc {
                egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 0)).show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(&l.summary).color(th.fg())).wrap());
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    if Btn::new("Show commit").show(ui, &th).clicked() {
                        self.jump_to_commit(&l.sha);
                    }
                });
            }
        } else if !tt && matches!(self.blame.data, Loadable::Loading) {
            widgets::skeleton(ui, &th, 2, 24.0);
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            if tt {
                ui.label(RichText::new("↑/↓ step through history · Esc back").font(widgets::small_font(&th)).color(th.muted()));
            } else if Btn::new("Follow this line's history").icon(Icon::Commit).show(ui, &th).clicked() {
                self.request_history(&path, line, sel.code_ref.end.max(line));
            }
        });
        ui.separator();
        match &self.history.data {
            Loadable::Idle => widgets::empty_state(ui, &th, "Line history", "Follow the selected line back through renames and edits."),
            Loadable::Loading => widgets::skeleton(ui, &th, 4, 40.0),
            Loadable::Failed(e) => {
                widgets::banner(ui, &th, Level::Error, &format!("History failed: {e}"), None, false);
            }
            Loadable::Ready(v) if v.is_empty() => widgets::empty_state(ui, &th, "No history", "No commit touched this line."),
            Loadable::Ready(v) => {
                let (active, scroll) = self.tt.as_mut().map_or((None, false), |t| (Some(t.sha.clone()), std::mem::take(&mut t.list_scroll)));
                let mut pick = None;
                let mut area = ScrollArea::vertical().id_salt("history_scroll").auto_shrink([false, false]);
                if scroll {
                    if let Some(i) = v.iter().position(|r| Some(&r.commit.sha) == active.as_ref()) {
                        area = area.vertical_scroll_offset((i as f32 * 44.0 - 44.0 * 2.0).max(0.0));
                    }
                }
                area.show_rows(ui, 44.0, v.len(), |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for i in range {
                        let r = &v[i];
                        let c = &r.commit;
                        let is_active = active.as_ref() == Some(&c.sha);
                        let (resp, rect) = widgets::list_row(ui, &th, 44.0, is_active);
                        if is_active {
                            ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2(3.0, 44.0)), 0.0, th.accent());
                        }
                        let l1 = Rect::from_min_max(pos2(rect.min.x + 4.0, rect.min.y + 4.0), pos2(rect.max.x, rect.min.y + 24.0));
                        let l2 = Rect::from_min_max(pos2(rect.min.x + 4.0, rect.min.y + 24.0), pos2(rect.max.x, rect.max.y - 4.0));
                        widgets::paint_text_fit(ui, l1, &c.subject, widgets::ui_font(&th), if is_active { th.accent() } else { th.fg() });
                        let renamed = if r.path != hist_path { format!("  ·  {}", r.path) } else { String::new() };
                        widgets::paint_text_fit(ui, l2, &format!("{}  ·  {}  ·  {}{}", crate::timefmt::short(&c.sha), c.author, crate::timefmt::rel(now, c.time), renamed), widgets::small_font(&th), th.muted());
                        if resp.clicked() {
                            pick = Some(i);
                        }
                    }
                });
                if let Some(i) = pick {
                    self.tt_pick(i);
                }
            }
        }
    }

    fn ask_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let m = th.m().clone();
        ui.add_space(m.space[1]);
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            ui.vertical(|ui| self.model_picker(ui, "ask"));
        });
        ui.add_space(m.space[1]);
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            let w = ui.available_width() - m.space[2];
            ui.add(egui::TextEdit::multiline(&mut self.ask.question).hint_text("Question (optional). Default: why was this written?").desired_rows(3).desired_width(w).margin(vec2(8.0, 6.0)));
        });
        ui.add_space(m.space[1]);
        let busy = matches!(self.ask.phase, AskPhase::Building | AskPhase::Running);
        ui.horizontal(|ui| {
            ui.add_space(m.space[2]);
            if Btn::new("Ask AI").icon(Icon::Spark).kind(BtnKind::Primary).enabled(self.selection.is_some() && !busy).show(ui, &th).on_hover_text("Ctrl+L").clicked() {
                self.start_ask();
            }
            if busy && Btn::new("Cancel").show(ui, &th).clicked() {
                self.cancel_ask();
            }
            if busy {
                if let Some(t) = self.ask.started {
                    let left = (self.settings.ask_timeout_secs as i64 - t.elapsed().as_secs() as i64).max(0);
                    ui.label(RichText::new(format!("{}s · times out in {}s", t.elapsed().as_secs(), left)).font(widgets::small_font(&th)).color(th.muted()));
                    widgets::repaint_if_focused(ui.ctx(), Duration::from_millis(500));
                }
            }
        });
        if self.selection.is_none() {
            ui.add_space(m.space[1]);
            ui.horizontal(|ui| {
                ui.add_space(m.space[2]);
                ui.label(RichText::new("Select lines in the diff first.").font(widgets::small_font(&th)).color(th.muted()));
            });
        }
        ui.add_space(m.space[1]);
        ui.separator();
        match &self.ask.phase {
            AskPhase::Idle => {
                widgets::empty_state(ui, &th, "Ask about the selection", "You will see exactly what is sent before anything leaves this machine.");
            }
            AskPhase::Building => widgets::skeleton(ui, &th, 3, 32.0),
            AskPhase::Preview(_) => {
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(RichText::new("Review the prompt in the dialog.").color(th.muted()));
                });
            }
            AskPhase::Failed(e) => {
                let e = e.clone();
                if widgets::banner(ui, &th, Level::Error, &e, Some("Try again"), false) == widgets::BannerAction::Action {
                    self.start_ask();
                }
                self.ask_output(ui);
            }
            AskPhase::Running | AskPhase::Done => self.ask_output(ui),
        }
    }

    fn ask_output(&self, ui: &mut Ui) {
        let th = self.th.clone();
        if self.ask.output.is_empty() {
            if matches!(self.ask.phase, AskPhase::Running) {
                widgets::skeleton(ui, &th, 3, 24.0);
            }
            return;
        }
        ScrollArea::vertical().id_salt("ask_out").auto_shrink([false, false]).stick_to_bottom(matches!(self.ask.phase, AskPhase::Running)).show(ui, |ui| {
            ui.add_space(8.0);
            egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 0)).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(&self.ask.output).font(widgets::ui_font(&th)).color(th.fg())).wrap().selectable(true));
            });
        });
    }

    // ------------------------------------------------------------ Ask preview dialog (PLAN §9.5)

    pub fn ask_preview_window(&mut self, ctx: &egui::Context) {
        let AskPhase::Preview(_) = &self.ask.phase else { return };
        let th = self.th.clone();
        let mut send = false;
        let mut cancel = false;
        let screen = ctx.screen_rect();
        let w = 560.0_f32.min(screen.width() - 32.0);
        if let AskPhase::Preview(p) = &mut self.ask.phase {
            egui::Window::new("What will be sent").collapsible(false).resizable(false).fixed_size(vec2(w, 0.0)).anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0)).frame(egui::Frame::new().fill(th.raised()).stroke(egui::Stroke::new(1.0, th.border())).corner_radius(th.radius()).inner_margin(egui::Margin::same(16)).shadow(egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(120) })).show(ctx, |ui| {
                ui.spacing_mut().item_spacing = vec2(8.0, 10.0);
                ui.label(RichText::new("Untick anything you do not want to share with the agent.").color(th.muted()));
                let any_warn = p.sections.iter().any(|s| s.included && !s.warnings.is_empty());
                ScrollArea::vertical().max_height((screen.height() - 300.0).max(120.0)).show(ui, |ui| {
                    for s in &mut p.sections {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut s.included, RichText::new(&s.title).font(widgets::ui_font(&th)).color(th.fg()));
                            ui.label(RichText::new(format!("{} chars", s.body.chars().count())).font(widgets::small_font(&th)).color(th.muted()));
                        });
                        for wn in &s.warnings {
                            widgets::banner(ui, &th, Level::Warn, wn, None, false);
                        }
                        let mut preview = s.body.chars().take(1200).collect::<String>();
                        if s.body.chars().count() > 1200 {
                            preview.push('…');
                        }
                        egui::Frame::new().fill(th.sunken()).corner_radius(th.m().radius_small).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.add(egui::Label::new(RichText::new(preview).font(widgets::mono_font(&th)).color(if s.included { th.fg() } else { th.disabled() })).wrap());
                        });
                    }
                });
                if !p.question_warnings.is_empty() {
                    ui.label(RichText::new("Your question").font(widgets::ui_font(&th)).color(th.fg()));
                    for wn in &p.question_warnings {
                        widgets::banner(ui, &th, Level::Warn, wn, None, false);
                    }
                    ui.checkbox(&mut p.question_included, RichText::new("Send my question anyway (it will be shared as typed)").font(widgets::ui_font(&th)).color(th.fg()));
                    if !p.question_included {
                        ui.label(RichText::new("Question withheld: a placeholder is sent instead.").font(widgets::small_font(&th)).color(th.muted()));
                    }
                }
                if any_warn {
                    widgets::banner(ui, &th, Level::Warn, "A section that looks like it contains a secret is ticked. Untick it before sending.", None, false);
                }
                ui.horizontal(|ui| {
                    if Btn::primary("Send to agent").enabled(p.sections.iter().any(|s| s.included)).show(ui, &th).clicked() {
                        send = true;
                    }
                    if Btn::new("Cancel").show(ui, &th).clicked() {
                        cancel = true;
                    }
                });
            });
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        if send {
            if let AskPhase::Preview(p) = std::mem::take(&mut self.ask.phase) {
                self.send_ask(&p);
            }
        } else if cancel {
            self.ask.phase = AskPhase::Idle;
        }
    }

    // ------------------------------------------------------------ smoke mode

    pub fn smoke_prepare(&mut self) {
        let Some(s) = &mut self.smoke else { return };
        if !s.prepared {
            s.prepared = true;
            let scene = s.scene.clone();
            match scene.as_str() {
                "split" => self.settings.split_view = true,
                "light" => {
                    self.th.file.active = crate::theme::Mode::Light;
                    self.th.apply(&self.ctx);
                }
                "settings" => self.settings_open = true,
                "accounts" => {
                    self.settings_open = true;
                    self.ag.how_open = Some("claude".into());
                    let mut lu = crate::agentvm::LoginUi::new("github-copilot");
                    lu.apply(crate::agents::LoginEvent::OpenUrl("https://github.com/login/device".into()));
                    lu.apply(crate::agents::LoginEvent::NeedCode { prompt: "Paste the code from the browser".into() });
                    self.ag.login = Some((lu, None));
                }
                "runs" | "runs-stream" => {
                    self.rail = RailTab::Runs;
                    self.ag_open_runs();
                }
                "model-picker" => {
                    self.insp_open = true;
                    self.insp = InspTab::Ask;
                    self.ag_ensure_models();
                }
                // Opens on "Working tree changes" by itself when the repo is dirty.
                "worktree" | "current-dirty" => {}
                "current-clean" => self.rail = RailTab::Current,
                "history-time-travel" => {
                    self.insp_open = true;
                    self.insp = InspTab::Blame;
                }
                "mode-sticky" => self.settings.view_mode = ViewMode::Blame,
                // Static sample: amber app RAM (>70% of 150 MB) and runs subtotal.
                "resources" => {
                    self.res.sample = Some(crate::resmon::Sample { app_rss_kb: 118 * 1024, child_rss_kb: 41 * 1024, children: 2, cpu_pct: 0.4, ticks: 0 });
                    self.res.runs = vec![("r1".into(), crate::agents::Resource { rss_kb: 1800 * 1024, cpu_pct: 62.0, procs: 5 }), ("r2".into(), crate::agents::Resource { rss_kb: 900 * 1024, cpu_pct: 20.0, procs: 3 })];
                }
                "project-picker" => {
                    let now = crate::agentapp::now_ms();
                    let h = crate::projects::home_dir();
                    for (i, p) in ["/tmp/ct-demo", "project/penny-procmon", "project/codetrail", "work/clients/acme/services/billing-gateway-with-a-very-long-name"].iter().enumerate() {
                        let path = if p.starts_with('/') { std::path::PathBuf::from(p) } else { h.join(p) };
                        self.settings.recent.push(crate::projects::RecentProject { path: path.to_string_lossy().into_owned(), opened_ms: now - 1000 * (i as i64 + 1) });
                    }
                    self.pj.popover = true;
                }
                "folder-browser" => {
                    self.pj_open_browser();
                    if let Some(b) = &mut self.pj.browser {
                        b.focus = false;
                    }
                }
                "new-run" => {
                    self.rail = RailTab::Runs;
                    self.ag_open_runs();
                    self.ag_new_run_dialog();
                    if let Some(n) = &mut self.ag.new_run {
                        n.prompt = "Add a --json flag to the export command and cover it with a test.".into();
                        n.focus = false;
                    }
                }
                "search" => {
                    self.rail = RailTab::Search;
                    self.search.query = self.smoke.as_ref().map(|s| s.query.clone()).unwrap_or_default();
                }
                _ => {}
            }
        }
    }

    pub fn smoke_tick(&mut self, ctx: &egui::Context) {
        let Some(s) = &self.smoke else { return };
        let scene = s.scene.clone();
        ctx.request_repaint_after(Duration::from_millis(60));
        // Scene steps that need data.
        let failed = matches!(self.open_state, Loadable::Failed(_));
        let ready = failed || (!self.busy() && self.repo.is_some() && !self.commits.is_empty() && !matches!(self.diffset, Loadable::Loading));
        if scene == "search" && self.repo.is_some() && self.search.ran.is_none() {
            self.start_search();
        }
        if scene == "model-picker" && !self.insp_open && !self.smoke.as_ref().is_some_and(|s| s.requested) {
            self.insp_open = true;
        }
        let browser_ready = scene != "folder-browser" || self.pj.browser.as_ref().is_some_and(|b| b.listing.ready().is_some());
        let ag_scene = matches!(scene.as_str(), "runs" | "runs-stream" | "accounts" | "model-picker" | "new-run");
        let ag_ready = !ag_scene || (self.ag.accounts.ready().is_some() && !self.ag.models.is_loading() && (self.ag.runs_svc.ready().is_some() || !matches!(scene.as_str(), "runs" | "runs-stream" | "new-run")));
        if scene == "runs-stream" && self.ag.runs_svc.ready().is_some() && self.centre != Centre::Run {
            use crate::agents::{AgentEvent as E, RunUpdate as U};
            for e in [
                E::TextDelta("I'll start by reading the export command.\n".into()),
                E::ToolStart { id: "t1".into(), name: "read".into(), summary: "crates/ct-cli/src/export.rs".into() },
                E::ToolEnd { id: "t1".into(), name: "read".into(), ok: true, path: Some("crates/ct-cli/src/export.rs".into()) },
                E::TextDelta("The export has one format today. I'll add a `--json` switch next to `--format` and a test that parses its output.\n".into()),
                E::ToolStart { id: "t2".into(), name: "edit".into(), summary: "add --json flag".into() },
                E::ToolEnd { id: "t2".into(), name: "edit".into(), ok: true, path: Some("crates/ct-cli/src/export.rs".into()) },
                E::ToolStart { id: "t3".into(), name: "bash".into(), summary: "cargo test -p ct-cli".into() },
            ] {
                self.ag.runs.apply(U::Event("r1".into(), e));
            }
            self.ag_select_run("r1");
        }
        let mut settled = ag_ready && browser_ready && ready && (scene != "search" || self.search.ran.is_some());
        // Fixture scenes (see docs): g.txt line 5 has a multi-commit history across a rename.
        if scene == "history-time-travel" && ready {
            if self.tt.is_none() && self.blame.path.is_empty() {
                self.open_blame("g.txt", Some(5));
                settled = false;
            } else if self.tt.is_none() && self.blame.data.ready().is_some() && self.selection.is_none() {
                let at = RefAt::Commit(self.head.clone());
                let text = self.blame.data.ready().and_then(|d| d.lines.get(4)).map(|l| l.text.clone()).unwrap_or_default();
                self.blame.selected = Some((4, 4));
                self.set_selection(code_ref_for_lines("g.txt", 5, 5, &at), SelSource::Blame, text);
                self.request_history("g.txt", 5, 5);
                settled = false;
            } else if self.tt.is_none() && self.history.data.ready().is_some() {
                self.tt_pick(1);
                settled = false;
            } else if self.tt.is_none() {
                settled = false;
            }
        }
        if scene == "mode-sticky" && ready && self.file_sel.as_deref() != Some("other.txt") {
            self.open_file_diff("other.txt");
            settled = false;
        }
        if scene == "why" && ready {
            if let Loadable::Ready(p) = &self.prepared {
                if self.selection.is_none() {
                    let idx = p.model.lines.iter().position(|l| l.kind == LineKind::Add).unwrap_or(0);
                    let sel = RowSel::single(idx);
                    if let (Some(cur), Some(path)) = (self.cur.clone(), self.file_sel.clone()) {
                        let rows = p.model.row_lines();
                        let new_at = cur.new_at.clone().unwrap_or(RefAt::Worktree);
                        if let Some(cr) = code_ref_for_rows(&rows, sel, &path, p.model.file.old_path.as_deref(), &new_at, cur.old_at.as_ref()) {
                            let text = p.model.lines[idx].text.clone();
                            self.diff_sel = Some(sel);
                            self.set_selection(cr, SelSource::Diff, text);
                        }
                    }
                    settled = false;
                }
            }
        }
        let s = self.smoke.as_mut().unwrap();
        s.stable = if settled { s.stable + 1 } else { 0 };
        s.settled_at = if settled { s.settled_at.or(Some(Instant::now())) } else { None };
        let timed_out = s.started.elapsed() > Duration::from_secs(30);
        if !s.requested && (s.stable >= 3 && s.settled_at.is_some_and(|t| t.elapsed() > Duration::from_millis(900)) || timed_out) {
            s.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let out = s.out.clone();
            let [w, h] = img.size;
            let mut buf = Vec::with_capacity(w * h * 4);
            for p in &img.pixels {
                buf.extend_from_slice(&p.to_srgba_unmultiplied());
            }
            if let Some(parent) = out.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match image::RgbaImage::from_raw(w as u32, h as u32, buf).map(|i| i.save(&out)) {
                Some(Ok(())) => eprintln!("screenshot saved: {} ({w}x{h})", out.display()),
                other => {
                    eprintln!("screenshot failed: {other:?}");
                    std::process::exit(2);
                }
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

// ---------------------------------------------------------------- diff painting

fn kind_colors(th: &Theme, k: LineKind) -> (Option<Color32>, Color32) {
    match k {
        LineKind::Add => (Some(th.c(th.p().diff.add.bg)), th.c(th.p().diff.add.word)),
        LineKind::Del => (Some(th.c(th.p().diff.del.bg)), th.c(th.p().diff.del.word)),
        LineKind::Ctx => (None, Color32::TRANSPARENT),
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_text_cell(ui: &Ui, painter: &egui::Painter, th: &Theme, model: &DiffModel, i: usize, origin: egui::Pos2, row: Rect, mono: &FontId) {
    let l = &model.lines[i];
    let (_, word) = kind_colors(th, l.kind);
    let g = ui.fonts(|f| f.layout_job(line_job(th, &l.text, &l.spans, mono)));
    let y = row.center().y - g.size().y / 2.0;
    for (s, e) in &l.words {
        let (x0, x1) = (x_of(&g, &l.text, *s), x_of(&g, &l.text, *e));
        painter.rect_filled(Rect::from_min_max(pos2(origin.x + x0, row.min.y + 1.0), pos2(origin.x + x1.max(x0 + 2.0), row.max.y - 1.0)), 2.0, word);
    }
    painter.galley(pos2(origin.x, y), g, th.fg());
}

#[allow(clippy::too_many_arguments)]
fn paint_unified(ui: &Ui, painter: &egui::Painter, th: &Theme, model: &DiffModel, i: usize, rect: Rect, num_w: f32, cw: f32, mono: &FontId, selected: bool) {
    let _ = cw;
    let l = &model.lines[i];
    if let (Some(bg), _) = kind_colors(th, l.kind) {
        painter.rect_filled(rect, 0.0, bg);
    }
    if selected {
        painter.rect_filled(rect, 0.0, th.selection());
        painter.rect_filled(Rect::from_min_size(rect.min, vec2(2.0, rect.height())), 0.0, th.accent());
    }
    let vis_left = ui.clip_rect().left().max(rect.min.x);
    // Line-number gutters stay pinned to the visible left edge when scrolled horizontally.
    let gx = vis_left;
    painter.rect_filled(Rect::from_min_size(pos2(gx, rect.min.y), vec2(2.0 * num_w + 18.0, rect.height())), 0.0, match kind_colors(th, l.kind).0 {
        Some(bg) => crate::theme::mix(th.sunken(), bg, 0.8),
        None => th.sunken(),
    });
    if selected {
        painter.rect_filled(Rect::from_min_size(pos2(gx, rect.min.y), vec2(2.0 * num_w + 18.0, rect.height())), 0.0, th.selection());
    }
    if let Some(n) = l.old_no {
        painter.text(pos2(gx + num_w - 8.0, rect.center().y), egui::Align2::RIGHT_CENTER, n.to_string(), mono.clone(), th.muted());
    }
    if let Some(n) = l.new_no {
        painter.text(pos2(gx + 2.0 * num_w - 8.0, rect.center().y), egui::Align2::RIGHT_CENTER, n.to_string(), mono.clone(), th.muted());
    }
    let marker = match l.kind {
        LineKind::Add => "+",
        LineKind::Del => "−",
        LineKind::Ctx => " ",
    };
    let mcol = match l.kind {
        LineKind::Add => th.c(th.p().diff.add.fg),
        LineKind::Del => th.c(th.p().diff.del.fg),
        LineKind::Ctx => th.muted(),
    };
    painter.text(pos2(gx + 2.0 * num_w + 4.0, rect.center().y), egui::Align2::LEFT_CENTER, marker, mono.clone(), mcol);
    let text_x = rect.min.x + 2.0 * num_w + 18.0;
    let clip = Rect::from_min_max(pos2(gx + 2.0 * num_w + 18.0, rect.min.y), rect.max).intersect(painter.clip_rect());
    paint_text_cell(ui, &painter.with_clip_rect(clip), th, model, i, pos2(text_x, rect.min.y), rect, mono);
    if l.no_newline {
        painter.text(pos2(rect.max.x - 8.0, rect.center().y), egui::Align2::RIGHT_CENTER, "no newline at end of file", widgets::small_font(th), th.muted());
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_cell(ui: &Ui, painter: &egui::Painter, th: &Theme, model: &DiffModel, idx: Option<usize>, rect: Rect, num_w: f32, cw: f32, mono: &FontId, left: bool, in_sel: impl Fn(usize) -> bool) {
    let _ = cw;
    let Some(i) = idx else {
        painter.rect_filled(rect, 0.0, th.sunken());
        return;
    };
    let l = &model.lines[i];
    if let (Some(bg), _) = kind_colors(th, l.kind) {
        painter.rect_filled(rect, 0.0, bg);
    }
    if in_sel(i) {
        painter.rect_filled(rect, 0.0, th.selection());
    }
    let n = if left { l.old_no } else { l.new_no };
    painter.rect_filled(Rect::from_min_size(rect.min, vec2(num_w, rect.height())), 0.0, crate::theme::mix(th.sunken(), kind_colors(th, l.kind).0.unwrap_or(th.sunken()), 0.8));
    if let Some(n) = n {
        painter.text(pos2(rect.min.x + num_w - 8.0, rect.center().y), egui::Align2::RIGHT_CENTER, n.to_string(), mono.clone(), th.muted());
    }
    let text_rect = Rect::from_min_max(pos2(rect.min.x + num_w + 8.0, rect.min.y), rect.max);
    let p = painter.with_clip_rect(text_rect.intersect(painter.clip_rect()));
    paint_text_cell(ui, &p, th, model, i, text_rect.min, rect, mono);
}
