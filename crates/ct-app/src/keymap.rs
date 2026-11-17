//! User-editable shortcuts: `"Ctrl+Shift+F"` strings <-> egui shortcuts, and the effective
//! binding table (built-in defaults from `ui::COMMANDS`, overridden by `[keys]` in config.toml).

use crate::ui::{Cmd, COMMANDS};
use egui::{Key, KeyboardShortcut, Modifiers};
use std::collections::BTreeMap;

/// Config name of a command (`[keys]` table key).
pub fn cmd_id(c: Cmd) -> String {
    format!("{c:?}")
}

/// `"Ctrl+Shift+F"` → shortcut. `Ctrl` and `Cmd` both mean the platform command key.
pub fn parse(s: &str) -> Option<KeyboardShortcut> {
    let mut parts: Vec<&str> = s.split('+').map(str::trim).collect();
    // "Ctrl++" = Ctrl and the Plus key
    if s.trim_end().ends_with("++") {
        parts.pop();
        parts.pop();
        parts.push("Plus");
    }
    let key = Key::from_name(parts.pop()?)?;
    let mut m = Modifiers::NONE;
    for p in parts {
        match p.to_ascii_lowercase().as_str() {
            "ctrl" | "cmd" | "command" => m.command = true,
            "shift" => m.shift = true,
            "alt" | "option" => m.alt = true,
            _ => return None,
        }
    }
    Some(KeyboardShortcut::new(m, key))
}

pub fn format(sc: &KeyboardShortcut) -> String {
    let mut s = String::new();
    if sc.modifiers.command || sc.modifiers.ctrl || sc.modifiers.mac_cmd {
        s.push_str("Ctrl+");
    }
    if sc.modifiers.alt {
        s.push_str("Alt+");
    }
    if sc.modifiers.shift {
        s.push_str("Shift+");
    }
    s.push_str(match sc.logical_key {
        Key::Comma => ",",
        Key::Period => ".",
        Key::Slash => "/",
        Key::Semicolon => ";",
        Key::OpenBracket => "[",
        Key::CloseBracket => "]",
        Key::Backslash => "\\",
        Key::Minus => "-",
        Key::Equals => "=",
        k => k.name(),
    });
    s
}

/// A global shortcut needs Ctrl/Alt or a function key so it never eats typed text.
pub fn usable(sc: &KeyboardShortcut) -> bool {
    let m = sc.modifiers;
    m.command || m.ctrl || m.mac_cmd || m.alt || sc.logical_key.name().strip_prefix('F').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn default_for(c: Cmd) -> Option<KeyboardShortcut> {
    COMMANDS.iter().find(|(x, _, _)| *x == c).and_then(|(_, _, k)| parse(k))
}

/// Effective binding: an override (`""` = unbound) wins over the default.
pub fn binding(keys: &BTreeMap<String, String>, c: Cmd) -> Option<KeyboardShortcut> {
    match keys.get(&cmd_id(c)) {
        Some(s) => parse(s),
        None => default_for(c),
    }
}

/// All bound commands, most modifiers first (egui ignores extra Shift/Alt when matching, so
/// Ctrl+Shift+F must be tried before Ctrl+F).
pub fn table(keys: &BTreeMap<String, String>) -> Vec<(KeyboardShortcut, Cmd)> {
    let mut v: Vec<(KeyboardShortcut, Cmd)> = COMMANDS.iter().filter_map(|(c, _, _)| binding(keys, *c).map(|k| (k, *c))).collect();
    v.sort_by_key(|(k, _)| std::cmp::Reverse(u8::from(k.modifiers.shift) + u8::from(k.modifiers.alt)));
    v
}

/// Binds `sc` to `c` (None = unbind). Any other command holding the same shortcut is unbound;
/// a binding equal to the default is stored as no override.
pub fn assign(keys: &mut BTreeMap<String, String>, c: Cmd, sc: Option<KeyboardShortcut>) {
    if let Some(sc) = sc {
        for (o, _, _) in COMMANDS.iter().filter(|(o, _, _)| *o != c) {
            if binding(keys, *o) == Some(sc) {
                keys.insert(cmd_id(*o), String::new());
            }
        }
    }
    if sc == default_for(c) {
        keys.remove(&cmd_id(c));
    } else {
        keys.insert(cmd_id(c), sc.map(|k| format(&k)).unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format_round_trip() {
        for s in ["Ctrl+O", "Ctrl+Shift+F", "Ctrl+,", "Alt+F5", "F2", "Ctrl+Alt+Shift+K", "Ctrl+["] {
            assert_eq!(format(&parse(s).unwrap()), s);
        }
        assert_eq!(parse("cmd+shift+f"), parse("Ctrl+Shift+F"));
        assert_eq!(parse("Ctrl++").map(|k| k.logical_key), Some(Key::Plus));
        assert!(parse("").is_none() && parse("Hyper+O").is_none() && parse("Ctrl+Nope").is_none());
    }

    #[test]
    fn only_modified_or_function_keys_are_usable() {
        assert!(usable(&parse("Ctrl+O").unwrap()) && usable(&parse("Alt+X").unwrap()) && usable(&parse("F12").unwrap()));
        assert!(!usable(&parse("O").unwrap()) && !usable(&parse("Shift+F").unwrap()));
    }

    #[test]
    fn overrides_unbind_conflicts_and_defaults_drop_out() {
        let mut keys = BTreeMap::new();
        assert_eq!(binding(&keys, Cmd::OpenProject), parse("Ctrl+O"));
        assign(&mut keys, Cmd::OpenProject, parse("Ctrl+P"));
        assert_eq!(binding(&keys, Cmd::OpenProject), parse("Ctrl+P"));
        assert_eq!(binding(&keys, Cmd::PickFile), None, "the old holder of Ctrl+P is unbound");
        assign(&mut keys, Cmd::OpenProject, parse("Ctrl+O"));
        assert!(!keys.contains_key("OpenProject"), "back to the default = no override");
        assign(&mut keys, Cmd::Save, None);
        assert_eq!(binding(&keys, Cmd::Save), None);
        let t = table(&keys);
        assert!(t.windows(2).all(|w| w[0].0.modifiers.shift >= w[1].0.modifiers.shift), "shifted shortcuts first");
    }
}
