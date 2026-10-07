//! Small helpers: secret masking and LF-only line framing.

use std::io::{BufRead, Read};
use std::sync::LazyLock;

/// Maximum protocol line kept in memory; longer lines are dropped (and reported once).
pub(crate) const MAX_LINE: usize = 32 * 1024 * 1024;

static SECRET_RES: LazyLock<Vec<regex::Regex>> = LazyLock::new(|| {
    [
            // JWT-like
            r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}(?:\.[A-Za-z0-9_-]*)?",
            // well-known key / token prefixes
            r"\b(?:sk|rk|pk)-[A-Za-z0-9_-]{12,}",
            r"\bgh[pousr]_[A-Za-z0-9]{16,}",
            r"\bgithub_pat_[A-Za-z0-9_]{16,}",
            r"\bAIza[0-9A-Za-z_-]{20,}",
            r"\bxox[abprs]-[A-Za-z0-9-]{10,}",
            // Authorization headers
            r"(?i)\b(?:bearer|basic)\s+[A-Za-z0-9._~+/=-]{8,}",
            // key/value pairs: "access_token": "...", token=..., api_key: ...
            r#"(?i)("?(?:access|refresh|id|api|auth|bearer|session|client)[_-]?(?:token|key|secret)"?|"?(?:token|secret|password|passwd|authorization|apikey)"?)\s*[:=]\s*"?[^\s",;&}]{6,}"#,
            // long opaque runs that mix letters and digits (not plain hex hashes)
            r"\b[A-Za-z0-9_-]{40,}\b",
        ]
    .iter()
    .map(|p| regex::Regex::new(p).expect("static regex"))
    .collect()
});

/// Replaces token-like strings with `[REDACTED]`. Applied to every error / stderr text
/// that leaves this crate.
pub fn mask_secrets(s: &str) -> String {
    let mut out = s.to_string();
    let res = &*SECRET_RES;
    for (i, re) in res.iter().enumerate() {
        let last = i + 1 == res.len();
        out = re
            .replace_all(&out, |c: &regex::Captures| {
                let m = c.get(0).unwrap().as_str();
                if last {
                    // keep plain hex digests (git shas etc.) and long paths-like runs
                    let all_hex = m.chars().all(|ch| ch.is_ascii_hexdigit());
                    let has_digit = m.chars().any(|ch| ch.is_ascii_digit());
                    let has_alpha = m.chars().any(|ch| ch.is_ascii_alphabetic());
                    if all_hex || !(has_digit && has_alpha) {
                        return m.to_string();
                    }
                    return "[REDACTED]".to_string();
                }
                if let Some(key) = c.get(1) {
                    // keep the key name, hide the value
                    return format!("{}=[REDACTED]", key.as_str().trim_matches('"'));
                }
                "[REDACTED]".to_string()
            })
            .into_owned();
    }
    out
}

pub(crate) fn lossy_masked(b: &[u8]) -> String {
    mask_secrets(&String::from_utf8_lossy(b))
}

/// Truncates on a char boundary.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Reads LF-delimited records (strips one trailing `\r`). Never splits on U+2028/2029 or any
/// other byte. Records over `MAX_LINE` are skipped and reported through `on_oversize`.
pub(crate) fn read_lf_lines<R: Read>(
    r: R,
    mut on_line: impl FnMut(Vec<u8>),
    mut on_oversize: impl FnMut(usize),
) {
    let mut r = std::io::BufReader::with_capacity(64 * 1024, r);
    let mut acc: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut overflow = false;
    loop {
        let buf = match r.fill_buf() {
            Ok([]) => break,
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => {
                if !overflow {
                    acc.extend_from_slice(&buf[..i]);
                }
                total += i;
                r.consume(i + 1);
                if overflow || total > MAX_LINE {
                    on_oversize(total);
                    acc.clear();
                } else {
                    if acc.last() == Some(&b'\r') {
                        acc.pop();
                    }
                    on_line(std::mem::take(&mut acc));
                }
                total = 0;
                overflow = false;
            }
            None => {
                let n = buf.len();
                total += n;
                if total > MAX_LINE {
                    overflow = true;
                    acc.clear();
                } else if !overflow {
                    acc.extend_from_slice(buf);
                }
                r.consume(n);
            }
        }
    }
    // trailing partial record without LF: protocol-incomplete, but surface it
    if !overflow && !acc.is_empty() {
        if acc.last() == Some(&b'\r') {
            acc.pop();
        }
        on_line(acc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lf_framing_keeps_unicode_separators_inside_a_record() {
        let data = "{\"a\":\"x\u{2028}y\u{2029}z\"}\r\n{\"b\":1}\n\n{\"c\":2}".as_bytes();
        let mut got = Vec::new();
        read_lf_lines(data, |l| got.push(String::from_utf8(l).unwrap()), |_| {});
        assert_eq!(got, vec!["{\"a\":\"x\u{2028}y\u{2029}z\"}", "{\"b\":1}", "", "{\"c\":2}"]);
    }

    #[test]
    fn masks_tokens_but_not_paths_or_shas() {
        let s = "token sk-abcdefghijklmnop1234 and Bearer abcdef1234567890 and \"access_token\": \"zzzz1111yyyy\" eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.sig_abcdefghij";
        let m = mask_secrets(s);
        assert!(!m.contains("abcdefghijklmnop1234"), "{m}");
        assert!(!m.contains("abcdef1234567890"), "{m}");
        assert!(!m.contains("zzzz1111yyyy"), "{m}");
        assert!(!m.contains("eyJzdWIi"), "{m}");
        let keep = "commit 0123456789abcdef0123456789abcdef01234567 in /home/user/project/src/main.rs";
        assert_eq!(mask_secrets(keep), keep);
    }
}
