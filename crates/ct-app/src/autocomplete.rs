//! Path autocomplete for the folder browser (local and remote).
//!
//! The completion logic is pure (`split`, `matches`, `common_prefix`, `tab_complete`). `Auto` is
//! the small state machine behind the dropdown: it caches the children of the last parent
//! directory so continued typing within it filters in memory, debounces listings (120 ms) and
//! keeps at most one listing in flight. The caller performs the listing (a worker job) and
//! reports it back through `arrived`.

use crate::widgets;
use egui::{pos2, vec2, Id, Key, Modifiers, Rect, Ui};
use std::time::{Duration, Instant};

/// Rows shown in the dropdown.
pub const MAX_SHOWN: usize = 12;
/// Children kept per listing.
pub const MAX_CHILDREN: usize = 2000;
pub const DEBOUNCE: Duration = Duration::from_millis(120);
const ROW_H: f32 = 26.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Child {
    pub name: String,
    pub is_git: bool,
}

impl Child {
    fn hidden(&self) -> bool {
        self.name.starts_with('.')
    }
}

/// Splits typed text at the last `/` into (`parent` with its trailing slash, typed segment).
/// `~` alone means the home directory. `None` when there is no directory part to list.
pub fn split(input: &str) -> Option<(&str, &str)> {
    if input == "~" {
        return Some(("~/", ""));
    }
    let i = input.rfind('/')?;
    Some((&input[..=i], &input[i + 1..]))
}

/// Children whose name starts with `seg` (case-insensitive). Hidden folders only when `seg`
/// itself starts with `.`.
pub fn matches(children: &[Child], seg: &str) -> Vec<Child> {
    let s = seg.to_lowercase();
    let dot = seg.starts_with('.');
    children.iter().filter(|c| (dot || !c.hidden()) && c.name.to_lowercase().starts_with(&s)).cloned().collect()
}

/// Longest common prefix, compared case-insensitively, spelled like the first name.
pub fn common_prefix(names: &[&str]) -> String {
    let Some(first) = names.first() else { return String::new() };
    let mut n = first.chars().count();
    for other in &names[1..] {
        n = first.chars().zip(other.chars()).take(n).take_while(|(a, b)| a.to_lowercase().eq(b.to_lowercase())).count();
    }
    first.chars().take(n).collect()
}

/// What Tab does: unique match → `parent + name + "/"`; several → their common prefix when it is
/// longer than what was typed; otherwise nothing.
pub fn tab_complete(parent: &str, seg: &str, m: &[Child]) -> Option<String> {
    match m {
        [] => None,
        [c] => Some(accept(parent, c)),
        _ => {
            let names: Vec<&str> = m.iter().map(|c| c.name.as_str()).collect();
            let p = common_prefix(&names);
            (p.chars().count() > seg.chars().count()).then(|| format!("{parent}{p}"))
        }
    }
}

pub fn accept(parent: &str, c: &Child) -> String {
    format!("{parent}{}/", c.name)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    Idle,
    /// A listing is due in this long.
    Wait(Duration),
    /// List `parent` and answer with `arrived(token, ..)`.
    Request { parent: String, token: u64 },
}

#[derive(Default)]
pub struct Auto {
    /// Typed form of the parent whose children are cached.
    parent: Option<String>,
    children: Vec<Child>,
    /// Parent of the current input (what the listing should be for).
    cur_parent: String,
    seg: String,
    want: bool,
    edited_at: Option<Instant>,
    inflight: bool,
    token: u64,
    pub matches: Vec<Child>,
    pub sel: Option<usize>,
    pub open: bool,
    /// An IME composition is active: do not act on keys or text.
    pub composing: bool,
    /// Text changed while composing; re-evaluate once composition ends.
    stale: bool,
    rect: Option<Rect>,
}

impl Auto {
    /// The typed text changed.
    pub fn edit(&mut self, input: &str, now: Instant) {
        self.sel = None;
        let Some((p, seg)) = split(input) else {
            self.matches.clear();
            self.open = false;
            self.want = false;
            self.cur_parent.clear();
            self.seg.clear();
            return;
        };
        self.open = true;
        self.seg = seg.to_string();
        self.cur_parent = p.to_string();
        if self.parent.as_deref() == Some(p) {
            self.want = false;
            self.matches = matches(&self.children, seg);
        } else {
            self.matches.clear();
            self.want = true;
            self.edited_at = Some(now);
        }
    }

    pub fn poll(&mut self, now: Instant) -> Poll {
        if !self.want || self.inflight {
            return Poll::Idle;
        }
        let due = self.edited_at.map_or(Duration::ZERO, |t| DEBOUNCE.saturating_sub(now.saturating_duration_since(t)));
        if !due.is_zero() {
            return Poll::Wait(due);
        }
        self.inflight = true;
        self.token += 1;
        Poll::Request { parent: self.cur_parent.clone(), token: self.token }
    }

    /// A listing finished. `None` = the parent does not exist or could not be read (cached as
    /// empty so typing on does not retry).
    pub fn arrived(&mut self, token: u64, parent: &str, children: Option<Vec<Child>>) {
        if token != self.token {
            return;
        }
        self.inflight = false;
        self.parent = Some(parent.to_string());
        self.children = children.unwrap_or_default();
        self.children.truncate(MAX_CHILDREN);
        if self.cur_parent == parent {
            self.want = false;
            self.matches = matches(&self.children, &self.seg);
        }
    }

    pub fn close(&mut self) {
        self.open = false;
        self.sel = None;
    }

    pub fn visible(&self) -> bool {
        self.open && !self.matches.is_empty()
    }

    pub fn parent_str(&self) -> &str {
        &self.cur_parent
    }

    fn move_sel(&mut self, down: bool) {
        let n = self.matches.len().min(MAX_SHOWN);
        if n == 0 {
            return;
        }
        self.sel = if down { Some(self.sel.map_or(0, |s| (s + 1).min(n - 1))) } else { self.sel.and_then(|s| s.checked_sub(1)) };
    }
}

/// Keeps `Auto::composing` in step with the platform IME.
pub fn track_ime(ctx: &egui::Context, a: &mut Auto) {
    ctx.input(|i| {
        for e in &i.events {
            if let egui::Event::Ime(ime) = e {
                match ime {
                    egui::ImeEvent::Preedit(s) => a.composing = !s.is_empty(),
                    egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled => {
                        a.composing = false;
                        a.stale = true;
                    }
                    egui::ImeEvent::Enabled => {}
                }
            }
        }
    });
}

/// Result of one frame of the path field.
#[derive(Default)]
pub struct FieldOut {
    /// Enter pressed with no highlighted suggestion: navigate to the typed path.
    pub submit: bool,
}

/// Path text field with the dropdown under it. Keys while the field has focus:
/// Tab completes, Up/Down move in the dropdown, Enter accepts the highlighted row (otherwise
/// submits), Esc closes the dropdown (a second Esc is left for the dialog).
pub fn path_field(ui: &mut Ui, th: &crate::theme::Theme, text: &mut String, a: &mut Auto, salt: &str, hint: &str, width: f32, grab_focus: bool) -> FieldOut {
    let id = Id::new(("path_field", salt));
    track_ime(ui.ctx(), a);
    let mut out = FieldOut::default();
    let mut changed = false;
    if ui.memory(|m| m.has_focus(id)) && !a.composing {
        let shown = a.visible();
        if shown {
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                a.move_sel(true);
            }
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                a.move_sel(false);
            }
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                a.close();
            }
        }
        if let (true, Some(s)) = (shown, a.sel) {
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                if let Some(c) = a.matches.get(s) {
                    *text = accept(a.parent_str(), c);
                    changed = true;
                }
            }
        } else if ui.input(|i| i.key_pressed(Key::Enter)) {
            out.submit = true;
        }
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
            if let Some(t) = tab_complete(a.parent_str(), &a.seg, &a.matches) {
                *text = t;
                changed = true;
            }
        }
    }
    let r = ui.add(egui::TextEdit::singleline(text).id(id).hint_text(hint).desired_width(width).margin(vec2(8.0, 6.0)).font(widgets::mono_font(th)));
    if grab_focus {
        r.request_focus();
    }
    if r.changed() {
        a.stale = true;
    }
    if changed {
        a.stale = true;
        a.composing = false;
        r.request_focus();
        let end = text.chars().count();
        if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), id) {
            st.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(end))));
            st.store(ui.ctx(), id);
        }
    }
    if a.stale && !a.composing {
        a.stale = false;
        a.edit(text, Instant::now());
    }
    if let Some(p) = ui.input(|i| i.pointer.primary_pressed().then(|| i.pointer.interact_pos()).flatten()) {
        if !r.rect.contains(p) && !a.rect.is_some_and(|d| d.contains(p)) {
            a.close();
        }
    }
    if a.visible() {
        let n = a.matches.len().min(MAX_SHOWN);
        let more = a.matches.len() - n;
        let h = n as f32 * ROW_H + if more > 0 { ROW_H } else { 0.0 } + 8.0;
        let area_rect = Rect::from_min_size(pos2(r.rect.left(), r.rect.bottom() + 2.0), vec2(r.rect.width(), h));
        a.rect = Some(area_rect);
        let pressed = ui.input(|i| i.pointer.primary_pressed()).then(|| ui.input(|i| i.pointer.interact_pos())).flatten();
        let mut clicked = None;
        egui::Area::new(Id::new(("path_dropdown", salt))).order(egui::Order::Foreground).fixed_pos(area_rect.min).show(ui.ctx(), |ui| {
            egui::Frame::new().fill(th.raised()).stroke(egui::Stroke::new(1.0, th.border())).corner_radius(th.radius()).inner_margin(egui::Margin::same(4)).show(ui, |ui| {
                ui.set_width(area_rect.width() - 8.0);
                ui.spacing_mut().item_spacing.y = 0.0;
                let hover = ui.input(|i| i.pointer.hover_pos());
                for (i, c) in a.matches.iter().take(n).enumerate() {
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), egui::Sense::hover());
                    let hot = a.sel == Some(i) || hover.is_some_and(|p| rect.contains(p));
                    if hot {
                        ui.painter().rect_filled(rect, th.radius(), if a.sel == Some(i) { th.selection() } else { th.hover() });
                    }
                    let mut right = rect.max.x - 8.0;
                    if c.is_git {
                        let g = widgets::galley(ui, "git", widgets::small_font(th), th.accent());
                        right -= g.size().x + 6.0;
                        widgets::paint_badge(ui.painter(), th, pos2(right, rect.center().y), "git", th.accent());
                    }
                    widgets::paint_text_fit(ui, Rect::from_min_max(rect.min + vec2(8.0, 0.0), pos2(right - 8.0, rect.max.y)), &c.name, widgets::mono_font(th), th.fg());
                    if pressed.is_some_and(|p| rect.contains(p)) {
                        clicked = Some(i);
                    }
                }
                if more > 0 {
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), egui::Sense::hover());
                    ui.painter().text(rect.left_center() + vec2(8.0, 0.0), egui::Align2::LEFT_CENTER, format!("+{more} more — keep typing"), widgets::small_font(th), th.muted());
                }
            });
        });
        if let Some(i) = clicked {
            if let Some(c) = a.matches.get(i) {
                *text = accept(a.parent_str(), c);
                a.edit(text, Instant::now());
                ui.memory_mut(|m| m.request_focus(id));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(n: &str, git: bool) -> Child {
        Child { name: n.into(), is_git: git }
    }

    fn kids() -> Vec<Child> {
        vec![c(".cache", false), c("Alpha", true), c("alps", false), c("beta", false), c("docs", false)]
    }

    #[test]
    fn split_at_the_last_slash_and_tilde_alone_lists_home() {
        assert_eq!(split("~/pro/al"), Some(("~/pro/", "al")));
        assert_eq!(split("/tmp/"), Some(("/tmp/", "")));
        assert_eq!(split("/x"), Some(("/", "x")));
        assert_eq!(split("~"), Some(("~/", "")));
        assert_eq!(split("relative"), None);
        assert_eq!(split(""), None);
    }

    #[test]
    fn matching_is_case_insensitive_prefix_and_hides_dot_folders_until_a_dot_is_typed() {
        let k = kids();
        let names = |v: Vec<Child>| v.into_iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(names(matches(&k, "al")), ["Alpha", "alps"]);
        assert_eq!(names(matches(&k, "AL")), ["Alpha", "alps"]);
        assert_eq!(names(matches(&k, "")), ["Alpha", "alps", "beta", "docs"]);
        assert_eq!(names(matches(&k, ".")), [".cache"]);
        assert!(matches(&k, "zz").is_empty());
    }

    #[test]
    fn common_prefix_ignores_case_and_is_spelled_like_the_first() {
        assert_eq!(common_prefix(&["Alpha", "alps"]), "Alp");
        assert_eq!(common_prefix(&["한글a", "한글b"]), "한글");
        assert_eq!(common_prefix(&["x", "y"]), "");
        assert_eq!(common_prefix(&[]), "");
    }

    #[test]
    fn tab_completes_unique_with_a_slash_and_several_to_the_common_prefix() {
        let k = kids();
        assert_eq!(tab_complete("/p/", "be", &matches(&k, "be")), Some("/p/beta/".into()));
        assert_eq!(tab_complete("/p/", "a", &matches(&k, "a")), Some("/p/Alp".into()));
        assert_eq!(tab_complete("/p/", "alp", &matches(&k, "alp")), None);
        assert_eq!(tab_complete("/p/", "zz", &matches(&k, "zz")), None);
    }

    #[test]
    fn typing_inside_the_cached_parent_filters_in_memory_without_a_new_listing() {
        let t0 = Instant::now();
        let mut a = Auto::default();
        a.edit("/p/", t0);
        assert_eq!(a.poll(t0), Poll::Wait(DEBOUNCE));
        let Poll::Request { parent, token } = a.poll(t0 + DEBOUNCE) else { panic!("due") };
        assert_eq!(parent, "/p/");
        // Never a second request while one is in flight, however much is typed.
        a.edit("/p/a", t0 + DEBOUNCE);
        assert_eq!(a.poll(t0 + DEBOUNCE * 5), Poll::Idle);
        a.arrived(token, "/p/", Some(kids()));
        assert_eq!(a.matches.len(), 2);
        a.edit("/p/alph", t0 + DEBOUNCE * 6);
        assert_eq!(a.matches.len(), 1);
        assert_eq!(a.poll(t0 + DEBOUNCE * 9), Poll::Idle, "same parent: no IO");
        // A different parent needs one more listing, and stale answers are ignored.
        a.edit("/p/Alpha/", t0 + DEBOUNCE * 10);
        assert!(a.matches.is_empty());
        let Poll::Request { token: t2, .. } = a.poll(t0 + DEBOUNCE * 12) else { panic!("due") };
        a.arrived(token, "/p/Alpha/", Some(vec![c("x", false)]));
        assert!(a.matches.is_empty(), "old token ignored");
        a.arrived(t2, "/p/Alpha/", None);
        assert!(a.matches.is_empty());
        a.edit("/p/Alpha/q", t0 + DEBOUNCE * 13);
        assert_eq!(a.poll(t0 + DEBOUNCE * 20), Poll::Idle, "missing parent is cached, no retry spam");
    }

    #[test]
    fn a_listing_is_capped_and_the_dropdown_never_lists_more_than_twelve() {
        let t0 = Instant::now();
        let mut a = Auto::default();
        a.edit("/p/", t0);
        let Poll::Request { token, .. } = a.poll(t0 + DEBOUNCE) else { panic!() };
        a.arrived(token, "/p/", Some((0..MAX_CHILDREN + 50).map(|i| c(&format!("d{i:05}"), false)).collect()));
        assert_eq!(a.matches.len(), MAX_CHILDREN);
        for _ in 0..40 {
            a.move_sel(true);
        }
        assert_eq!(a.sel, Some(MAX_SHOWN - 1));
        a.move_sel(false);
        assert_eq!(a.sel, Some(MAX_SHOWN - 2));
    }

    #[test]
    fn no_directory_part_means_no_dropdown_and_no_listing() {
        let mut a = Auto::default();
        a.edit("abc", Instant::now());
        assert!(!a.visible());
        assert_eq!(a.poll(Instant::now() + DEBOUNCE * 3), Poll::Idle);
    }
}
