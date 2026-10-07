//! Minimal git helpers (plain `git` subprocess; ct-agent stays decoupled from ct-core).

use similar::{ChangeTag, TextDiff};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const MAX_DIFF_BYTES: usize = 1 << 20;

#[derive(Clone, Debug)]
pub struct RepoCtx {
    /// Work tree root (canonical).
    pub root: PathBuf,
    /// Common git dir (shared by linked worktrees) - where `codetrail/` lives.
    pub git_dir: PathBuf,
}

/// Walk up from `start` looking for `.git` (dir or `gitdir:` file). No subprocess.
pub fn find_repo(start: &Path) -> Option<RepoCtx> {
    let mut cur = Some(start);
    while let Some(d) = cur {
        if d.is_dir() {
            let g = d.join(".git");
            if g.is_dir() {
                return Some(RepoCtx { root: d.canonicalize().ok()?, git_dir: g.canonicalize().ok()? });
            }
            if g.is_file() {
                let txt = std::fs::read_to_string(&g).ok()?;
                let target = txt.trim().strip_prefix("gitdir:")?.trim();
                let mut gd = PathBuf::from(target);
                if gd.is_relative() {
                    gd = d.join(gd);
                }
                let gd = gd.canonicalize().ok()?;
                let common = match std::fs::read_to_string(gd.join("commondir")) {
                    Ok(c) => gd.join(c.trim()).canonicalize().ok()?,
                    Err(_) => gd,
                };
                return Some(RepoCtx { root: d.canonicalize().ok()?, git_dir: common });
            }
        }
        cur = d.parent();
    }
    None
}

pub fn git_out(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let o = Command::new("git")
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    o.status.success().then_some(o.stdout)
}

pub fn git_str(root: &Path, args: &[&str]) -> Option<String> {
    git_out(root, args).map(|b| String::from_utf8_lossy(&b).trim().to_string())
}

pub fn head(root: &Path) -> String {
    git_str(root, &["rev-parse", "--verify", "-q", "HEAD"]).unwrap_or_default()
}

/// Blob oid of the file as git would store it (filters applied, nothing written).
pub fn hash_object(root: &Path, rel: &str) -> Option<String> {
    git_str(root, &["hash-object", "--", rel]).filter(|s| s.len() >= 40)
}

pub fn blob_at(root: &Path, rev: &str, rel: &str) -> Option<String> {
    git_str(root, &["rev-parse", "--verify", "-q", &format!("{rev}:{rel}")])
}

/// Repo-relative `/` path for `p` (absolute, or relative to `cwd`). `None` if outside the repo.
pub fn rel_path(ctx: &RepoCtx, p: &Path, cwd: &Path) -> Option<String> {
    let abs = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    let name = abs.file_name()?;
    let parent = abs.parent()?;
    let parent = parent.canonicalize().unwrap_or_else(|_| parent.to_path_buf());
    let full = parent.join(name);
    let rel = full.strip_prefix(&ctx.root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    (!s.is_empty()).then_some(s)
}

pub fn read_text_limited(p: &Path) -> Option<Vec<u8>> {
    let m = std::fs::metadata(p).ok()?;
    if m.len() as usize > MAX_DIFF_BYTES {
        return None;
    }
    std::fs::read(p).ok()
}

/// Added blocks of `post` relative to `pre` as store spans. Binary / oversized input -> no spans.
pub fn spans_between(pre: &[u8], post: &[u8]) -> Vec<ct_store::Span> {
    if pre.contains(&0) || post.contains(&0) {
        return vec![];
    }
    let (a, b) = (String::from_utf8_lossy(pre), String::from_utf8_lossy(post));
    let diff = TextDiff::from_lines(a.as_ref(), b.as_ref());
    let new_lines: Vec<&str> = b.lines().collect();
    let mut spans: Vec<ct_store::Span> = Vec::new();
    let mut run: Option<(usize, usize)> = None; // (start idx, len) in new
    let flush = |run: &mut Option<(usize, usize)>, spans: &mut Vec<ct_store::Span>| {
        if let Some((s, l)) = run.take() {
            let fp = ct_store::fingerprint_lines(&new_lines[s..(s + l).min(new_lines.len())]);
            let occ = spans.iter().filter(|x| x.fingerprint == fp).count() as u16;
            spans.push(ct_store::Span { new_start: s as u32 + 1, new_len: l as u32, fingerprint: fp, occurrence: occ });
        }
    };
    for ch in diff.iter_all_changes() {
        match ch.tag() {
            ChangeTag::Insert => {
                let i = ch.new_index().unwrap_or(0);
                match run.as_mut() {
                    Some((s, l)) if *s + *l == i => *l += 1,
                    _ => {
                        flush(&mut run, &mut spans);
                        run = Some((i, 1));
                    }
                }
            }
            _ => flush(&mut run, &mut spans),
        }
    }
    flush(&mut run, &mut spans);
    spans
}

pub fn whole_file_spans(post: &[u8]) -> Vec<ct_store::Span> {
    spans_between(b"", post)
}
