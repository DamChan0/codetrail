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

/// One `--numstat -z` record.
pub(crate) struct NumEntry {
    pub add: u32,
    pub del: u32,
    pub binary: bool,
    pub path: String,
    pub old_path: Option<String>,
}

fn trunc(what: &str) -> Error {
    Error::Invalid(format!("truncated {what} in git diff output"))
}

fn path_tok(t: Option<&[u8]>, what: &str) -> Result<String> {
    match t {
        Some(t) if !t.is_empty() => Ok(lossy(t)),
        Some(_) => Err(Error::Invalid(format!("empty path in {what}"))),
        None => Err(trunc(what)),
    }
}

/// Strict numstat count: ASCII digits fitting u32.
fn count(x: &[u8], rec: &[u8]) -> Result<u32> {
    if x.is_empty() || !x.iter().all(u8::is_ascii_digit) {
        return Err(Error::Invalid(format!("bad numstat count in record {:?}", lossy(rec))));
    }
    lossy(x).parse::<u32>().map_err(|_| Error::Invalid(format!("numstat count out of range in record {:?}", lossy(rec))))
}

/// Parses `--raw -z` records followed by optional `--numstat -z` records. Any structural
/// problem (missing path tokens, bad counts, unknown record) is an error.
pub(crate) fn parse_raw_numstat(buf: &[u8]) -> Result<(Vec<RawEntry>, Vec<NumEntry>)> {
    let mut toks = buf.split(|&b| b == 0);
    let mut raws = Vec::new();
    let mut nums = Vec::new();
    while let Some(t) = toks.next() {
        if t.is_empty() {
            continue;
        }
        if t[0] == b':' {
            if !nums.is_empty() {
                return Err(Error::Invalid("raw record after numstat records".into()));
            }
            let meta = lossy(&t[1..]);
            let mut f = meta.split(' ');
            let (om, nm, oo, no, st) = (f.next(), f.next(), f.next(), f.next(), f.next());
            let (om, nm, oo, no, st) = match (om, nm, oo, no, st) {
                (Some(a), Some(b), Some(c), Some(d), Some(e)) if f.next().is_none() => (a, b, c, d, e),
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
                'M' => Status::Modified,
                _ => return Err(Error::Invalid(format!("unsupported status {st:?} in raw diff record"))),
            };
            let (path, old_path) = if matches!(status, Status::Renamed | Status::Copied) {
                let o = path_tok(toks.next(), "raw rename/copy source")?;
                let n = path_tok(toks.next(), "raw rename/copy target")?;
                (n, Some(o))
            } else {
                (path_tok(toks.next(), "raw path")?, None)
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
            // numstat: `add\tdel\tpath` or `add\tdel\t` NUL old NUL new (rename/copy)
            let mut parts = t.splitn(3, |&b| b == b'\t');
            let (a, d, rest) = match (parts.next(), parts.next(), parts.next()) {
                (Some(a), Some(d), Some(r)) => (a, d, r),
                _ => return Err(Error::Invalid(format!("unparsable numstat record {:?}", lossy(t)))),
            };
            let binary = a == b"-" && d == b"-";
            let (add, del) = if binary { (0, 0) } else { (count(a, t)?, count(d, t)?) };
            let (path, old_path) = if rest.is_empty() {
                let o = path_tok(toks.next(), "numstat rename/copy source")?;
                let n = path_tok(toks.next(), "numstat rename/copy target")?;
                (n, Some(o))
            } else {
                (lossy(rest), None)
            };
            nums.push(NumEntry { add, del, binary, path, old_path });
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

/// Joins raw and numstat records in order, requiring identical paths.
///
/// With `whitespace_ignored` (`git diff -w`), `--raw` still lists blob-level changes that
/// numstat omits; such a raw entry may be absent from numstat only if it is a plain
/// content modification, and it is dropped. Anything else unmatched is an error.
pub(crate) fn to_stats(
    raws: Vec<RawEntry>,
    nums: Vec<NumEntry>,
    whitespace_ignored: bool,
) -> Result<Vec<FileStat>> {
    if !whitespace_ignored && raws.len() != nums.len() {
        return Err(Error::Invalid(format!(
            "git diff produced {} raw records but {} numstat records",
            raws.len(),
            nums.len()
        )));
    }
    let mut nums = nums.into_iter().peekable();
    let mut out = Vec::with_capacity(raws.len());
    for e in raws {
        let matches = nums.peek().is_some_and(|n| e.path == n.path && e.old_path == n.old_path);
        if !matches {
            let droppable = whitespace_ignored && e.status == Status::Modified && e.old_mode == e.new_mode;
            if droppable {
                continue;
            }
            return Err(Error::Invalid(format!(
                "raw/numstat records disagree at {:?} (numstat has {:?})",
                e.path,
                nums.peek().map(|n| n.path.as_str())
            )));
        }
        let n = nums.next().expect("peeked");
        let kind = kind_of(&e, n.binary);
        out.push(FileStat {
            path: e.path,
            old_path: e.old_path,
            status: e.status,
            similarity: e.similarity,
            add: n.add,
            del: n.del,
            binary: n.binary,
            kind,
            old_mode: e.old_mode,
            new_mode: e.new_mode,
        });
    }
    if nums.next().is_some() {
        return Err(Error::Invalid("numstat has records with no raw entry".into()));
    }
    Ok(out)
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

    fn raw(status: &str, mode: &str, paths: &[&str]) -> Vec<u8> {
        let mut v = format!(":100644 {mode} {} {} {status}", "1".repeat(40), "2".repeat(40)).into_bytes();
        for p in paths {
            v.push(0);
            v.extend_from_slice(p.as_bytes());
        }
        v.push(0);
        v
    }
    fn num(a: &str, d: &str, paths: &[&str]) -> Vec<u8> {
        let mut v = format!("{a}\t{d}\t").into_bytes();
        if paths.len() == 1 {
            v.extend_from_slice(paths[0].as_bytes());
            v.push(0);
        } else {
            v.push(0);
            for p in paths {
                v.extend_from_slice(p.as_bytes());
                v.push(0);
            }
        }
        v
    }
    fn stats(buf: &[u8]) -> Result<Vec<FileStat>> {
        let (r, n) = parse_raw_numstat(buf)?;
        to_stats(r, n, false)
    }

    #[test]
    fn mixed_stream_rename_copy_binary_korean() {
        let mut b = Vec::new();
        b.extend(raw("R090", "100644", &["옛 이름.txt", "새 이름.txt"]));
        b.extend(raw("C075", "100644", &["원본.rs", "복사 본.rs"]));
        b.extend(raw("M", "100644", &["img.bin"]));
        b.extend(raw("A", "100644", &["한글/파일 a.txt"]));
        b.extend(num("3", "1", &["옛 이름.txt", "새 이름.txt"]));
        b.extend(num("5", "0", &["원본.rs", "복사 본.rs"]));
        b.extend(num("-", "-", &["img.bin"]));
        b.extend(num("7", "2", &["한글/파일 a.txt"]));
        let s = stats(&b).unwrap();
        let got: Vec<_> = s.iter().map(|f| (f.path.as_str(), f.old_path.as_deref(), f.status, f.add, f.del, f.binary)).collect();
        assert_eq!(
            got,
            vec![
                ("새 이름.txt", Some("옛 이름.txt"), Status::Renamed, 3, 1, false),
                ("복사 본.rs", Some("원본.rs"), Status::Copied, 5, 0, false),
                ("img.bin", None, Status::Modified, 0, 0, true),
                ("한글/파일 a.txt", None, Status::Added, 7, 2, false),
            ]
        );
        assert_eq!(s[0].similarity, Some(90));
    }

    #[test]
    fn malformed_streams_are_errors_never_zeros() {
        let base = |extra: Vec<u8>| {
            let mut b = raw("R090", "100644", &["a.txt", "b.txt"]);
            b.extend(raw("M", "100644", &["c.txt"]));
            b.extend(extra);
            b
        };
        let ok_c = num("2", "2", &["c.txt"]);
        // rename numstat missing new-path token (stream ends)
        let mut m = base(Vec::new());
        m.extend(b"1\t1\t\0a.txt\0");
        assert!(stats(&m).is_err(), "truncated rename numstat");
        // rename numstat swallowing the next record's tokens
        let mut m = base(Vec::new());
        m.extend(b"1\t1\t\0");
        m.extend(&ok_c);
        assert!(stats(&m).is_err(), "rename numstat with too few tokens");
        // numstat for a different path
        let mut m = base(Vec::new());
        m.extend(num("1", "1", &["a.txt", "b.txt"]));
        m.extend(num("2", "2", &["zzz.txt"]));
        assert!(stats(&m).is_err(), "path disagreement");
        // count mismatch: fewer numstat records than raw
        let mut m = base(Vec::new());
        m.extend(num("1", "1", &["a.txt", "b.txt"]));
        assert!(stats(&m).is_err(), "count mismatch (previously silent zeros)");
        // count mismatch: extra numstat
        let mut m = base(Vec::new());
        m.extend(num("1", "1", &["a.txt", "b.txt"]));
        m.extend(&ok_c);
        m.extend(num("1", "1", &["d.txt"]));
        assert!(stats(&m).is_err(), "extra numstat");
        // non-numeric / half-binary counts
        for (a, d) in [("x", "1"), ("1", "-"), ("-", "1"), ("", "1"), ("-1", "1"), ("99999999999", "1")] {
            let mut m = base(Vec::new());
            m.extend(num("1", "1", &["a.txt", "b.txt"]));
            m.extend(num(a, d, &["c.txt"]));
            assert!(stats(&m).is_err(), "counts {a:?}/{d:?}");
        }
        // no numstat tab structure
        let mut m = base(Vec::new());
        m.extend(b"garbage\0");
        assert!(stats(&m).is_err());
        // raw rename missing paths
        assert!(parse_raw_numstat(&format!(":100644 100644 {} {} R090\0old\0", "1".repeat(40), "2".repeat(40)).into_bytes()).is_err());
        // empty rename path tokens
        let mut m = raw("R090", "100644", &["", "b.txt"]);
        m.extend(num("1", "1", &["", "b.txt"]));
        assert!(stats(&m).is_err(), "empty path token");
        // valid stream still fine
        let mut m = base(Vec::new());
        m.extend(num("1", "1", &["a.txt", "b.txt"]));
        m.extend(&ok_c);
        assert_eq!(stats(&m).unwrap().len(), 2);
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
