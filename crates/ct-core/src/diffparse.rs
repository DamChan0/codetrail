//! Parsers for `git diff --raw/--numstat -z` and unified patch output.

use crate::{DiffLine, FileKind, FileStat, Hunk, LineKind, Result, Status, Error};

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

pub(crate) struct RawEntry {
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
    pub old_oid: Option<String>,
    pub new_oid: Option<String>,
    pub status: Status,
    pub similarity: Option<u8>,
    pub path: String,
    pub old_path: Option<String>,
}

fn mode(s: &str) -> Option<u32> {
    let m = u32::from_str_radix(s, 8).ok()?;
    if m == 0 {
        None
    } else {
        Some(m)
    }
}

fn oid(s: &str) -> Option<String> {
    if s.bytes().all(|c| c == b'0') {
        None
    } else {
        Some(s.to_string())
    }
}

/// Parses `--raw -z` records followed by optional `--numstat -z` records.
/// Returns raw entries plus numstat (add, del, binary) aligned positionally.
pub(crate) fn parse_raw_numstat(buf: &[u8]) -> Result<(Vec<RawEntry>, Vec<(u32, u32, bool)>)> {
    let mut toks = buf.split(|&b| b == 0).peekable();
    let mut raws = Vec::new();
    let mut nums = Vec::new();
    while let Some(t) = toks.next() {
        if t.is_empty() {
            continue;
        }
        if t[0] == b':' {
            let meta = lossy(&t[1..]);
            let mut f = meta.split(' ');
            let (om, nm, oo, no, st) = (f.next(), f.next(), f.next(), f.next(), f.next());
            let (om, nm, oo, no, st) = match (om, nm, oo, no, st) {
                (Some(a), Some(b), Some(c), Some(d), Some(e)) => (a, b, c, d, e),
                _ => return Err(Error::Invalid(format!("unparsable raw diff record {meta:?}"))),
            };
            let letter = st.chars().next().unwrap_or('M');
            let similarity = st[letter.len_utf8()..].parse::<u8>().ok();
            let status = match letter {
                'A' => Status::Added,
                'D' => Status::Deleted,
                'R' => Status::Renamed,
                'C' => Status::Copied,
                'T' => Status::TypeChanged,
                _ => Status::Modified,
            };
            let (path, old_path) = if matches!(status, Status::Renamed | Status::Copied) {
                let o = toks.next().ok_or_else(|| Error::Invalid("truncated raw diff".into()))?;
                let n = toks.next().ok_or_else(|| Error::Invalid("truncated raw diff".into()))?;
                (lossy(n), Some(lossy(o)))
            } else {
                let p = toks.next().ok_or_else(|| Error::Invalid("truncated raw diff".into()))?;
                (lossy(p), None)
            };
            raws.push(RawEntry {
                old_mode: mode(om),
                new_mode: mode(nm),
                old_oid: oid(oo),
                new_oid: oid(no),
                status,
                similarity,
                path,
                old_path,
            });
        } else {
            // numstat: `add\tdel\tpath` or `add\tdel\t` + old + new (rename/copy)
            let mut parts = t.splitn(3, |&b| b == b'\t');
            let (a, d, rest) = (parts.next(), parts.next(), parts.next());
            let (a, d, rest) = match (a, d, rest) {
                (Some(a), Some(d), Some(r)) => (a, d, r),
                _ => return Err(Error::Invalid(format!("unparsable numstat record {:?}", lossy(t)))),
            };
            if rest.is_empty() {
                toks.next();
                toks.next();
            }
            let binary = a == b"-" && d == b"-";
            let num = |x: &[u8]| lossy(x).parse::<u32>().unwrap_or(0);
            nums.push((num(a), num(d), binary));
        }
    }
    Ok((raws, nums))
}

pub(crate) fn kind_of(e: &RawEntry, binary: bool) -> FileKind {
    let m = if e.status == Status::Deleted { e.old_mode } else { e.new_mode.or(e.old_mode) };
    match m.map(|m| m & 0o170000) {
        Some(0o120000) => FileKind::Symlink,
        Some(0o160000) => FileKind::Submodule,
        _ if binary => FileKind::Binary,
        _ => FileKind::Text,
    }
}

pub(crate) fn to_stats(raws: Vec<RawEntry>, nums: Vec<(u32, u32, bool)>) -> Vec<FileStat> {
    let aligned = raws.len() == nums.len();
    raws.into_iter()
        .enumerate()
        .map(|(i, e)| {
            let (add, del, binary) = if aligned { nums[i] } else { (0, 0, false) };
            let kind = kind_of(&e, binary);
            FileStat {
                path: e.path,
                old_path: e.old_path,
                status: e.status,
                similarity: e.similarity,
                add,
                del,
                binary,
                kind,
                old_mode: e.old_mode,
                new_mode: e.new_mode,
            }
        })
        .collect()
}

pub(crate) struct ParsedPatch {
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// `@@ -a,b +c,d @@ section`
fn parse_hunk_header(line: &[u8]) -> Option<Hunk> {
    let s = lossy(line);
    let rest = s.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, section) = rest.split_once(" @@")?;
    let (old_start, old_len) = parse_range(old)?;
    let (new_start, new_len) = parse_range(new)?;
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
        section: section.strip_prefix(' ').unwrap_or(section).to_string(),
        lines: Vec::new(),
    })
}

/// Parses the output of `git diff -U<n>` for a single file.
pub(crate) fn parse_patch(buf: &[u8]) -> ParsedPatch {
    let mut out = ParsedPatch { binary: false, hunks: Vec::new() };
    let mut cur: Option<(Hunk, u32, u32, u32, u32)> = None; // hunk, old_rem, new_rem, old_no, new_no
    let mut lines = buf.split(|&b| b == b'\n').peekable();
    while let Some(line) = lines.next() {
        if lines.peek().is_none() && line.is_empty() {
            break; // trailing newline
        }
        let in_body = cur.as_ref().is_some_and(|c| c.1 > 0 || c.2 > 0);
        if !in_body {
            if line.starts_with(b"@@ ") {
                if let Some((h, ..)) = cur.take() {
                    out.hunks.push(h);
                }
                if let Some(h) = parse_hunk_header(line) {
                    let (o, n) = (h.old_len, h.new_len);
                    let (os, ns) = (h.old_start, h.new_start);
                    cur = Some((h, o, n, os, ns));
                }
                continue;
            }
            if line.starts_with(b"\\") {
                if let Some((h, ..)) = cur.as_mut() {
                    if let Some(l) = h.lines.last_mut() {
                        l.no_newline_at_eof = true;
                    }
                }
                continue;
            }
            if cur.is_none() && (line.starts_with(b"Binary files ") || line.starts_with(b"GIT binary patch")) {
                out.binary = true;
            }
            continue;
        }
        let (h, orem, nrem, ono, nno) = cur.as_mut().unwrap();
        match line.first() {
            Some(b'\\') => {
                if let Some(l) = h.lines.last_mut() {
                    l.no_newline_at_eof = true;
                }
            }
            Some(b'+') => {
                h.lines.push(DiffLine {
                    kind: LineKind::Add,
                    old_no: None,
                    new_no: Some(*nno),
                    text: lossy(&line[1..]),
                    no_newline_at_eof: false,
                });
                *nno += 1;
                *nrem = nrem.saturating_sub(1);
            }
            Some(b'-') => {
                h.lines.push(DiffLine {
                    kind: LineKind::Del,
                    old_no: Some(*ono),
                    new_no: None,
                    text: lossy(&line[1..]),
                    no_newline_at_eof: false,
                });
                *ono += 1;
                *orem = orem.saturating_sub(1);
            }
            _ => {
                // context line (leading space); tolerate a bare empty line
                let text = if line.is_empty() { String::new() } else { lossy(&line[1..]) };
                h.lines.push(DiffLine {
                    kind: LineKind::Ctx,
                    old_no: Some(*ono),
                    new_no: Some(*nno),
                    text,
                    no_newline_at_eof: false,
                });
                *ono += 1;
                *nno += 1;
                *orem = orem.saturating_sub(1);
                *nrem = nrem.saturating_sub(1);
            }
        }
    }
    if let Some((h, ..)) = cur.take() {
        out.hunks.push(h);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_header_variants() {
        let h = parse_hunk_header(b"@@ -1 +1,2 @@ fn main()").unwrap();
        assert_eq!((h.old_start, h.old_len, h.new_start, h.new_len), (1, 1, 1, 2));
        assert_eq!(h.section, "fn main()");
        let h = parse_hunk_header(b"@@ -0,0 +1,3 @@").unwrap();
        assert_eq!((h.old_start, h.old_len, h.new_start, h.new_len), (0, 0, 1, 3));
        assert!(h.section.is_empty());
        assert!(parse_hunk_header(b"@@ garbage").is_none());
    }

    #[test]
    fn patch_with_dashes_in_body_and_no_newline() {
        let p = b"diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n--- b\n+++ c\n d\n\\ No newline at end of file\n";
        let r = parse_patch(p);
        assert_eq!(r.hunks.len(), 1);
        let l = &r.hunks[0].lines;
        assert_eq!(l.len(), 4);
        assert_eq!(l[1].kind, LineKind::Del);
        assert_eq!(l[1].text, "-- b");
        assert_eq!(l[2].kind, LineKind::Add);
        assert!(l[3].no_newline_at_eof);
        assert_eq!((l[3].old_no, l[3].new_no), (Some(3), Some(3)));
    }
}
