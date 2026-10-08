//! Minimal git helpers (plain `git` subprocess; ct-agent stays decoupled from ct-core).

use similar::{ChangeTag, TextDiff};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};

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

/// Absolute path of `git`, resolved once from PATH (never re-resolved per call).
static GIT_BIN: LazyLock<PathBuf> = LazyLock::new(|| {
    std::env::var_os("PATH")
        .and_then(|p| std::env::split_paths(&p).map(|d| d.join("git")).find(|c| c.is_absolute() && c.is_file()))
        .unwrap_or_else(|| PathBuf::from("/usr/bin/git"))
});

/// Process-wide deadline for the hook path: every git call made after `set_hard_deadline` is killed by then.
static HARD_DEADLINE: OnceLock<Instant> = OnceLock::new();
/// Ceiling for any git call when no hook deadline is set (CLI subcommands).
const DEFAULT_GIT_CEILING: Duration = Duration::from_secs(30);

pub fn set_hard_deadline(at: Instant) {
    let _ = HARD_DEADLINE.set(at);
}

fn call_deadline() -> Instant {
    HARD_DEADLINE.get().copied().unwrap_or_else(|| Instant::now() + DEFAULT_GIT_CEILING)
}

/// git in its own process group (killed as a group), with PDEATHSIG so it never outlives us.
fn spawn_git(root: &Path, args: &[&str], piped_stdin: bool) -> Option<Child> {
    let mut cmd = Command::new(&*GIT_BIN);
    cmd.current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .stdin(if piped_stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    // SAFETY: only async-signal-safe prctl between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    cmd.spawn().ok()
}

fn kill_group(child: &mut Child) {
    // SAFETY: plain signal to the group we created with process_group(0).
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

pub fn git_out(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    git_io(root, args, None, call_deadline())
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

/// Lexically validate a repo-relative path (no absolute, no `..`, no empty, no NUL) and join it to the
/// root, rejecting results whose real location (symlinks resolved, deepest existing ancestor) is
/// outside the repository. Use for EVERY file access derived from user / agent / git-listed paths.
pub fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.contains('\0') {
        return None;
    }
    let p = Path::new(rel);
    if !p.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)) {
        return None;
    }
    let joined = root.join(p);
    let canon_root = root.canonicalize().ok()?;
    let mut probe = joined.as_path();
    loop {
        match probe.canonicalize() {
            Ok(c) => return c.starts_with(&canon_root).then_some(joined),
            Err(_) => probe = probe.parent()?,
        }
    }
}

/// Run git with optional stdin; kills it at `deadline`. `None` on failure / timeout.
pub fn git_io(root: &Path, args: &[&str], input: Option<Vec<u8>>, deadline: Instant) -> Option<Vec<u8>> {
    let mut child = spawn_git(root, args, input.is_some())?;
    if let Some(inp) = input {
        let mut si = child.stdin.take()?;
        std::thread::spawn(move || {
            let _ = si.write_all(&inp);
        });
    }
    let mut so = child.stdout.take()?;
    let rd = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = so.read_to_end(&mut v);
        v
    });
    loop {
        match child.try_wait().ok()? {
            Some(st) => {
                let out = rd.join().ok()?;
                return st.success().then_some(out);
            }
            None if Instant::now() >= deadline => {
                kill_group(&mut child);
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

/// `git status` entries (path, deleted-in-worktree-or-index flag unused) for tracked changes + untracked files.
pub fn status_paths(root: &Path, deadline: Instant) -> Option<Vec<String>> {
    let out = git_io(root, &["-c", "core.quotepath=off", "status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames"], None, deadline)?;
    let mut v: Vec<String> = out
        .split(|c| *c == 0)
        .filter(|e| e.len() > 3)
        .map(|e| String::from_utf8_lossy(&e[3..]).into_owned())
        .collect();
    v.sort();
    v.dedup();
    Some(v)
}

/// One `hash-object --stdin-paths` for all existing files. Paths with newlines are skipped.
pub fn hash_objects(root: &Path, rels: &[String], deadline: Instant) -> HashMap<String, String> {
    let ok: Vec<&String> = rels.iter().filter(|r| !r.contains('\n')).collect();
    if ok.is_empty() {
        return HashMap::new();
    }
    let input = ok.iter().map(|r| r.as_str()).collect::<Vec<_>>().join("\n") + "\n";
    let Some(out) = git_io(root, &["hash-object", "--stdin-paths"], Some(input.into_bytes()), deadline) else {
        return HashMap::new();
    };
    let lines: Vec<&str> = std::str::from_utf8(&out).unwrap_or("").lines().collect();
    if lines.len() != ok.len() {
        return HashMap::new();
    }
    ok.into_iter().zip(lines).map(|(p, h)| (p.clone(), h.to_string())).collect()
}

/// `HEAD:<path>` blob oid + size for each path via ONE `cat-file --batch-check`.
pub fn head_blobs(root: &Path, rels: &[String], deadline: Instant) -> HashMap<String, (String, u64)> {
    let ok: Vec<&String> = rels.iter().filter(|r| !r.contains('\n')).collect();
    if ok.is_empty() {
        return HashMap::new();
    }
    let input = ok.iter().map(|r| format!("HEAD:{r}")).collect::<Vec<_>>().join("\n") + "\n";
    let Some(out) = git_io(root, &["cat-file", "--batch-check"], Some(input.into_bytes()), deadline) else {
        return HashMap::new();
    };
    let mut m = HashMap::new();
    for (p, line) in ok.iter().zip(std::str::from_utf8(&out).unwrap_or("").lines()) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() == 3 && f[1] == "blob" {
            if let Ok(sz) = f[2].parse::<u64>() {
                m.insert((*p).clone(), (f[0].to_string(), sz));
            }
        }
    }
    m
}

/// Contents of blobs via ONE `cat-file --batch`.
pub fn blob_contents(root: &Path, oids: &[String], deadline: Instant) -> HashMap<String, Vec<u8>> {
    if oids.is_empty() {
        return HashMap::new();
    }
    let input = oids.join("\n") + "\n";
    let Some(out) = git_io(root, &["cat-file", "--batch"], Some(input.into_bytes()), deadline) else {
        return HashMap::new();
    };
    let mut m = HashMap::new();
    let mut pos = 0;
    while pos < out.len() {
        let Some(nl) = out[pos..].iter().position(|c| *c == b'\n') else { break };
        let hdr = String::from_utf8_lossy(&out[pos..pos + nl]).into_owned();
        pos += nl + 1;
        let f: Vec<&str> = hdr.split_whitespace().collect();
        if f.len() != 3 {
            continue; // "<oid> missing"
        }
        let Ok(sz) = f[2].parse::<usize>() else { break };
        if pos + sz > out.len() {
            break;
        }
        m.insert(f[0].to_string(), out[pos..pos + sz].to_vec());
        pos += sz + 1;
    }
    m
}
