//! Lightweight lexical highlighter. Token colours come from the active theme palette
//! (keyword=accent, string=diff.add.fg, comment=fg.muted, number=warn, type=ai_badge), so
//! syntax colouring follows theme.toml.
//!
//! Size caps: files above `MAX_LINES` lines are not highlighted, and a line longer than
//! `MAX_LINE_BYTES` is a single plain span.

use crate::theme::Theme;
use egui::Color32;

pub const MAX_LINES: usize = 20_000;
pub const MAX_LINE_BYTES: usize = 2_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tok {
    Plain,
    Keyword,
    Str,
    Comment,
    Number,
    Type,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub tok: Tok,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Rust,
    CLike,
    Python,
    Script, // js/ts
    Go,
    Shell,
    Config, // toml/yaml/ini/conf: # comments
    Json,
    Plain,
}

struct Cfg {
    line_comment: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    keywords: &'static [&'static str],
    types_capitalized: bool,
    triple_quotes: bool,
    backtick: bool,
    single_quote_strings: bool,
}

const RUST_KW: &[&str] = &["as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while"];
const C_KW: &[&str] = &["auto", "break", "case", "catch", "class", "const", "constexpr", "continue", "default", "delete", "do", "else", "enum", "explicit", "extern", "false", "for", "friend", "goto", "if", "inline", "namespace", "new", "noexcept", "nullptr", "operator", "private", "protected", "public", "return", "sizeof", "static", "struct", "switch", "template", "this", "throw", "true", "try", "typedef", "typename", "union", "using", "virtual", "void", "volatile", "while", "int", "char", "float", "double", "long", "short", "unsigned", "signed", "bool"];
const PY_KW: &[&str] = &["and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while", "with", "yield", "self"];
const JS_KW: &[&str] = &["async", "await", "break", "case", "catch", "class", "const", "continue", "default", "delete", "do", "else", "export", "extends", "false", "finally", "for", "from", "function", "if", "import", "in", "instanceof", "interface", "let", "new", "null", "of", "return", "static", "super", "switch", "this", "throw", "true", "try", "type", "typeof", "undefined", "var", "void", "while", "yield"];
const GO_KW: &[&str] = &["break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough", "false", "for", "func", "go", "goto", "if", "import", "interface", "map", "nil", "package", "range", "return", "select", "struct", "switch", "true", "type", "var"];
const SH_KW: &[&str] = &["if", "then", "else", "elif", "fi", "for", "while", "do", "done", "case", "esac", "function", "in", "return", "export", "local", "echo", "set", "exit"];
const CFG_KW: &[&str] = &["true", "false", "null"];

fn cfg(l: Lang) -> Cfg {
    match l {
        Lang::Rust => Cfg { line_comment: &["//"], block: Some(("/*", "*/")), keywords: RUST_KW, types_capitalized: true, triple_quotes: false, backtick: false, single_quote_strings: false },
        Lang::CLike => Cfg { line_comment: &["//"], block: Some(("/*", "*/")), keywords: C_KW, types_capitalized: true, triple_quotes: false, backtick: false, single_quote_strings: true },
        Lang::Python => Cfg { line_comment: &["#"], block: None, keywords: PY_KW, types_capitalized: true, triple_quotes: true, backtick: false, single_quote_strings: true },
        Lang::Script => Cfg { line_comment: &["//"], block: Some(("/*", "*/")), keywords: JS_KW, types_capitalized: true, triple_quotes: false, backtick: true, single_quote_strings: true },
        Lang::Go => Cfg { line_comment: &["//"], block: Some(("/*", "*/")), keywords: GO_KW, types_capitalized: true, triple_quotes: false, backtick: true, single_quote_strings: true },
        Lang::Shell => Cfg { line_comment: &["#"], block: None, keywords: SH_KW, types_capitalized: false, triple_quotes: false, backtick: false, single_quote_strings: true },
        Lang::Config => Cfg { line_comment: &["#", ";"], block: None, keywords: CFG_KW, types_capitalized: false, triple_quotes: false, backtick: false, single_quote_strings: true },
        Lang::Json => Cfg { line_comment: &[], block: None, keywords: CFG_KW, types_capitalized: false, triple_quotes: false, backtick: false, single_quote_strings: false },
        Lang::Plain => Cfg { line_comment: &[], block: None, keywords: &[], types_capitalized: false, triple_quotes: false, backtick: false, single_quote_strings: false },
    }
}

pub fn lang_for_path(path: &str) -> Lang {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let ext = name.rsplit_once('.').map(|x| x.1).unwrap_or("");
    match ext {
        "rs" => Lang::Rust,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "java" | "cs" | "kt" | "swift" | "proto" => Lang::CLike,
        "py" | "pyi" => Lang::Python,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Lang::Script,
        "go" => Lang::Go,
        "sh" | "bash" | "zsh" => Lang::Shell,
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" => Lang::Config,
        "json" => Lang::Json,
        _ if name == "makefile" || name == "dockerfile" => Lang::Config,
        _ => Lang::Plain,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Normal,
    Block,
    Triple(char),
}

/// Highlight `lines`; state (block comments, triple-quoted strings) carries across lines.
pub fn highlight(lang: Lang, lines: &[&str]) -> Vec<Vec<Span>> {
    if lang == Lang::Plain || lines.len() > MAX_LINES {
        return lines.iter().map(|l| plain(l)).collect();
    }
    let c = cfg(lang);
    let mut st = State::Normal;
    lines.iter().map(|l| if l.len() > MAX_LINE_BYTES { plain(l) } else { line(&c, l, &mut st) }).collect()
}

fn plain(l: &str) -> Vec<Span> {
    if l.is_empty() {
        vec![]
    } else {
        vec![Span { start: 0, end: l.len(), tok: Tok::Plain }]
    }
}

fn is_ident(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn line(c: &Cfg, l: &str, st: &mut State) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut push = |start: usize, end: usize, tok: Tok| {
        if end > start {
            if let Some(last) = out.last_mut() {
                if last.tok == tok && last.end == start {
                    last.end = end;
                    return;
                }
            }
            out.push(Span { start, end, tok });
        }
    };
    let b = l.as_bytes();
    let mut i = 0;
    while i < l.len() {
        match *st {
            State::Block => {
                let close = c.block.map(|x| x.1).unwrap_or("*/");
                match l[i..].find(close) {
                    Some(p) => {
                        push(i, i + p + close.len(), Tok::Comment);
                        i += p + close.len();
                        *st = State::Normal;
                    }
                    None => {
                        push(i, l.len(), Tok::Comment);
                        i = l.len();
                    }
                }
                continue;
            }
            State::Triple(q) => {
                let close: String = std::iter::repeat(q).take(3).collect();
                match l[i..].find(&close) {
                    Some(p) => {
                        push(i, i + p + 3, Tok::Str);
                        i += p + 3;
                        *st = State::Normal;
                    }
                    None => {
                        push(i, l.len(), Tok::Str);
                        i = l.len();
                    }
                }
                continue;
            }
            State::Normal => {}
        }
        let rest = &l[i..];
        let ch = rest.chars().next().unwrap();
        if c.line_comment.iter().any(|p| rest.starts_with(p)) {
            push(i, l.len(), Tok::Comment);
            break;
        }
        if let Some((open, _)) = c.block {
            if rest.starts_with(open) {
                *st = State::Block;
                push(i, i + open.len(), Tok::Comment);
                i += open.len();
                continue;
            }
        }
        if c.triple_quotes && (rest.starts_with("\"\"\"") || rest.starts_with("'''")) {
            *st = State::Triple(ch);
            push(i, i + 3, Tok::Str);
            i += 3;
            continue;
        }
        let quote = ch == '"' || (ch == '`' && c.backtick) || (ch == '\'' && c.single_quote_strings);
        let rust_char = ch == '\'' && !c.single_quote_strings && c.line_comment == ["//"] && c.keywords == RUST_KW;
        if quote || rust_char {
            let mut j = i + 1;
            let mut closed = false;
            // rust: 'a (lifetime) has no closing quote within 2-4 bytes; treat 'x' / '\n' as char
            if rust_char {
                let tail = &l[j..];
                let esc = tail.starts_with('\\');
                let n = if esc { tail.chars().take(2).map(|c| c.len_utf8()).sum::<usize>() } else { tail.chars().next().map_or(0, |c| c.len_utf8()) };
                if l[j + n.min(tail.len())..].starts_with('\'') && n > 0 {
                    push(i, j + n + 1, Tok::Str);
                    i = j + n + 1;
                } else {
                    push(i, i + 1, Tok::Plain);
                    i += 1;
                }
                continue;
            }
            while j < l.len() {
                let cj = b[j];
                if cj == b'\\' {
                    j = (j + 2).min(l.len());
                    continue;
                }
                if l[j..].starts_with(ch) {
                    j += ch.len_utf8();
                    closed = true;
                    break;
                }
                j += l[j..].chars().next().map_or(1, |c| c.len_utf8());
            }
            let _ = closed;
            push(i, j.min(l.len()), Tok::Str);
            i = j.min(l.len());
            continue;
        }
        if ch.is_ascii_digit() {
            let mut j = i;
            while j < l.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'.') {
                j += 1;
            }
            push(i, j, Tok::Number);
            i = j;
            continue;
        }
        if is_ident(ch) {
            let mut j = i;
            while j < l.len() {
                let cj = l[j..].chars().next().unwrap();
                if !is_ident(cj) {
                    break;
                }
                j += cj.len_utf8();
            }
            let w = &l[i..j];
            let tok = if c.keywords.contains(&w) {
                Tok::Keyword
            } else if c.types_capitalized && ch.is_ascii_uppercase() && w.chars().any(|x| x.is_ascii_lowercase()) {
                Tok::Type
            } else {
                Tok::Plain
            };
            push(i, j, tok);
            i = j;
            continue;
        }
        push(i, i + ch.len_utf8(), Tok::Plain);
        i += ch.len_utf8();
    }
    out
}

pub fn color(th: &Theme, t: Tok) -> Color32 {
    let p = th.p();
    match t {
        Tok::Plain => th.fg(),
        Tok::Keyword => p.accent.0,
        Tok::Str => p.diff.add.fg.0,
        Tok::Comment => p.fg.muted.0,
        Tok::Number => p.warn.0,
        Tok::Type => p.ai_badge.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(lang: Lang, l: &str) -> Vec<(&str, Tok)> {
        highlight(lang, &[l]).remove(0).into_iter().map(|s| (&l[s.start..s.end], s.tok)).collect()
    }

    #[test]
    fn rust_basics() {
        let t = toks(Lang::Rust, "let x: Vec<u8> = 42; // hi");
        assert!(t.contains(&("let", Tok::Keyword)));
        assert!(t.contains(&("Vec", Tok::Type)));
        assert!(t.contains(&("42", Tok::Number)));
        assert_eq!(t.last().unwrap(), &("// hi", Tok::Comment));
    }

    #[test]
    fn spans_cover_line_exactly_in_order_and_utf8_safe() {
        let l = "let s = \"한글 문자열\"; // 주석 ✓";
        let v = highlight(Lang::Rust, &[l]).remove(0);
        let mut pos = 0;
        for s in &v {
            assert_eq!(s.start, pos);
            assert!(l.is_char_boundary(s.start) && l.is_char_boundary(s.end));
            pos = s.end;
        }
        assert_eq!(pos, l.len());
    }

    #[test]
    fn rust_lifetime_is_not_a_string() {
        let t = toks(Lang::Rust, "fn f<'a>(x: &'a str) -> char { 'z' }");
        assert!(t.iter().all(|(s, k)| !(*k == Tok::Str && s.contains("a>"))), "{t:?}");
        assert!(t.contains(&("'z'", Tok::Str)));
    }

    #[test]
    fn block_comment_state_carries_across_lines() {
        let v = highlight(Lang::CLike, &["int a; /* start", "still comment", "end */ int b;"]);
        assert_eq!(v[1], vec![Span { start: 0, end: 13, tok: Tok::Comment }]);
        assert_eq!(v[2][0].tok, Tok::Comment);
        assert!(v[2].iter().any(|s| s.tok == Tok::Keyword));
    }

    #[test]
    fn python_triple_quote_carries() {
        let v = highlight(Lang::Python, &["x = \"\"\"abc", "def\"\"\" + y"]);
        assert_eq!(v[1][0].tok, Tok::Str);
        assert_eq!(v[1][0].end, 6);
    }

    #[test]
    fn caps_leave_giant_input_plain() {
        let long = "a".repeat(MAX_LINE_BYTES + 1);
        assert_eq!(highlight(Lang::Rust, &[long.as_str()])[0].len(), 1);
        let many: Vec<&str> = std::iter::repeat("let x = 1;").take(MAX_LINES + 1).collect();
        assert!(highlight(Lang::Rust, &many).iter().all(|s| s.len() == 1));
    }

    #[test]
    fn extension_mapping() {
        assert_eq!(lang_for_path("a/b/c.rs"), Lang::Rust);
        assert_eq!(lang_for_path("한글/파일.py"), Lang::Python);
        assert_eq!(lang_for_path("Makefile"), Lang::Config);
        assert_eq!(lang_for_path("README"), Lang::Plain);
    }
}
