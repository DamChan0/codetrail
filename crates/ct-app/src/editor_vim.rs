//! Vim-style navigation for the focused file editor; native app shortcuts are handled first.

use egui::{Event, Key};
use std::collections::BTreeSet;

pub(crate) const BINDINGS: [(&str, &str); 11] = [
    ("h", "Left"), ("j", "Down"), ("k", "Up"), ("l", "Right"),
    ("0", "Start of line"), ("$", "End of line"), ("gg", "First line"), ("G", "Last line"),
    ("i", "Insert here"), ("a", "Insert after cursor"), ("Esc", "Return to Normal"),
];

#[derive(Default)]
pub(crate) struct Vim {
    pub(crate) insert: bool,
    pending_g: bool,
    column: Option<usize>,
}

impl Vim {
    pub(crate) fn handle(
        &mut self,
        events: &mut Vec<Event>,
        text: &str,
        lines: &[(usize, usize)],
        cursor: usize,
        excluded: &BTreeSet<String>,
        editable: bool,
    ) -> Option<usize> {
        let mut cursor = cursor;
        let mut moved = None;
        let end = |row: usize| lines.get(row + 1).map_or_else(
            || lines[row].1 + text[lines[row].0..].chars().count(),
            |&(_, next)| next - 1,
        );
        events.retain(|event| {
            let key = match event {
                Event::Key { key: Key::Escape, pressed: true, modifiers, .. } if modifiers.is_none() => "Esc",
                Event::Text(key) if !self.insert => key.as_str(),
                Event::Key { key: Key::Backspace | Key::Delete | Key::Enter, pressed: true, modifiers, .. }
                    if !self.insert && !modifiers.command && !modifiers.ctrl && !modifiers.alt => return false,
                Event::Ime(_) if !self.insert => return false,
                Event::PointerButton { pressed: true, .. }
                | Event::Key { key: Key::ArrowLeft | Key::ArrowRight | Key::ArrowUp | Key::ArrowDown | Key::Home | Key::End | Key::PageUp | Key::PageDown, pressed: true, .. } => {
                    self.column = None;
                    self.pending_g = false;
                    return true;
                }
                _ => return true,
            };
            let pending_g = std::mem::take(&mut self.pending_g);
            if !matches!(key, "j" | "k") {
                self.column = None;
            }
            if excluded.contains(if key == "g" { "gg" } else { key }) {
                return true;
            }
            let row = lines.partition_point(|&(_, start)| start <= cursor) - 1;
            let start = lines[row].1;
            let last = |row: usize| end(row).saturating_sub(1).max(lines[row].1);
            cursor = match key {
                "Esc" if self.insert => {
                    self.insert = false;
                    cursor.min(end(row)).saturating_sub(1).max(start)
                }
                "Esc" => return false,
                "i" | "a" if editable => {
                    self.insert = true;
                    (cursor + usize::from(key == "a")).min(end(row))
                }
                "h" => cursor.min(end(row)).saturating_sub(1).max(start),
                "l" => (cursor + 1).min(last(row)),
                "j" | "k" => {
                    let target = if key == "j" { (row + 1).min(lines.len() - 1) } else { row.saturating_sub(1) };
                    let column = *self.column.get_or_insert(cursor.min(last(row)) - start);
                    (lines[target].1 + column).min(last(target))
                }
                "0" => start,
                "$" => last(row),
                "g" if pending_g => 0,
                "g" => {
                    self.pending_g = true;
                    return false;
                }
                "G" => lines.last().unwrap().1,
                _ => return false,
            };
            moved = Some(cursor);
            false
        });
        moved
    }
}
