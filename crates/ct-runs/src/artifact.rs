//! Git plumbing for runs: worktree create/remove, artifact commits, change listing.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::gitrun;

const NO_HOOKS: [&str; 4] = ["-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub pre_blob: Option<String>,
    pub post_blob: Option<String>,
}

pub fn run_ref(id: &str) -> String {
    format!("refs/codetrail/runs/{id}")
}

fn fnv32(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Stable per-repo directory name: sanitized basename + hash of the canonical root.
pub fn repo_id(root: &Path) -> String {
    let name: String = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(32)
        .collect();
    format!("{}-{:08x}", if name.is_empty() { "repo" } else { &name }, fnv32(&root.to_string_lossy()))
}

pub fn slug(prompt: &str) -> String {
    let words: Vec<String> = prompt
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(5)
        .map(|w| w.to_ascii_lowercase())
        .collect();
    let mut s = words.join("-");
    s.truncate(24);
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "run".into()
    } else {
        s
    }
}

pub fn worktrees_root(data: &Path) -> PathBuf {
    data.join("worktrees")
}

/// The only place a run's worktree may live.
pub fn worktree_path(data: &Path, repo_root: &Path, id: &str) -> PathBuf {
    worktrees_root(data).join(repo_id(repo_root)).join(id)
}

fn lexically_clean(p: &Path) -> bool {
    p.is_absolute() && p.components().all(|c| matches!(c, Component::RootDir | Component::Prefix(_) | Component::Normal(_)))
}

/// A worktree path may be touched (removed) only when it is strictly below `<data>/worktrees`, has
/// no `..`, and (after resolving symlinks of its existing part) is still below it.
pub fn check_worktree_path(data: &Path, p: &Path) -> Result<()> {
    let root = worktrees_root(data);
    if !lexically_clean(p) || !p.starts_with(&root) || p == root {
        return Err(anyhow!("refusing to touch {}: not inside {}", p.display(), root.display()));
    }
    let croot = root.canonicalize().with_context(|| format!("{} missing", root.display()))?;
    let mut probe = p;
    let real = loop {
        match probe.canonicalize() {
            Ok(c) => break c,
            Err(_) => probe = probe.parent().ok_or_else(|| anyhow!("unresolvable path {}", p.display()))?,
        }
    };
    if !real.starts_with(&croot) || real == croot {
        return Err(anyhow!("refusing to touch {}: resolves outside {}", p.display(), croot.display()));
    }
    Ok(())
}

/// Creates the isolated worktree + branch. On failure nothing is left behind.
pub fn create_worktree(data: &Path, repo_root: &Path, id: &str, branch: &str, base_sha: &str) -> Result<PathBuf> {
    let wt = worktree_path(data, repo_root, id);
    let parent = wt.parent().ok_or_else(|| anyhow!("bad worktree path"))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    check_worktree_path(data, &wt)?;
    let wt_s = wt.to_string_lossy().into_owned();
    let ref_name = format!("refs/heads/{branch}");
    let preexisting = gitrun::test(repo_root, &["show-ref", "--verify", "--quiet", &ref_name])?;
    let mut args: Vec<&str> = NO_HOOKS.to_vec();
    args.extend(["worktree", "add", "-q", "-b", branch, &wt_s, base_sha]);
    if let Err(e) = gitrun::run(repo_root, &args) {
        let _ = remove_worktree(data, repo_root, &wt, None);
        if !preexisting {
            // only a ref this call created, and only while it still points at the base
            let _ = gitrun::run(repo_root, &["update-ref", "-d", &ref_name, base_sha]);
        }
        return Err(e.context("git worktree add"));
    }
    Ok(wt)
}

/// Removes the worktree dir and branch of a run. Both paths are validated first; nothing outside
/// `<data>/worktrees` is ever deleted.
pub fn remove_worktree(data: &Path, repo_root: &Path, wt: &Path, branch: Option<&str>) -> Result<()> {
    check_worktree_path(data, wt)?;
    if let Some(b) = branch {
        if !b.starts_with("ct/") || b.contains("..") || b.starts_with('-') {
            return Err(anyhow!("refusing to delete branch {b:?}"));
        }
    }
    let wt_s = wt.to_string_lossy().into_owned();
    let mut errs: Vec<String> = Vec::new();
    if repo_root.is_dir() {
        if wt.exists() {
            if let Err(e) = gitrun::run(repo_root, &["worktree", "remove", "--force", &wt_s]) {
                errs.push(format!("{e:#}"));
            }
        }
        let _ = gitrun::run(repo_root, &["worktree", "prune"]);
    }
    if wt.exists() {
        // `git worktree remove` failed or repo is gone: delete the validated dir ourselves.
        fs::remove_dir_all(wt).with_context(|| format!("remove {}", wt.display()))?;
    }
    if let Some(b) = branch {
        if repo_root.is_dir() {
            let exists = gitrun::test(repo_root, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{b}")]);
            if let Ok(true) = exists {
                gitrun::run(repo_root, &["branch", "-D", b]).with_context(|| format!("delete branch {b}"))?;
            }
        }
    }
    if wt.exists() {
        return Err(anyhow!("could not remove {}: {}", wt.display(), errs.join("; ")));
    }
    Ok(())
}

fn author_env(id: &str) -> [(&'static str, String); 4] {
    let email = format!("{id}@codetrail.local");
    [
        ("GIT_AUTHOR_NAME", "codetrail-run".into()),
        ("GIT_AUTHOR_EMAIL", email.clone()),
        ("GIT_COMMITTER_NAME", "codetrail-run".into()),
        ("GIT_COMMITTER_EMAIL", email),
    ]
}

fn env_refs<'a>(v: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    v.iter().map(|(k, s)| (*k, s.as_str())).collect()
}

/// Isolated mode: commits whatever the agent left uncommitted in the worktree (`git add -A &&
/// git commit --no-verify`). Returns the branch head afterwards (unchanged if there was nothing).
pub fn commit_worktree(wt: &Path, id: &str) -> Result<String> {
    gitrun::run(wt, &["add", "-A"])?;
    let dirty = !gitrun::run(wt, &["diff", "--cached", "--name-only", "-z"])?.is_empty();
    if dirty {
        let env = author_env(id);
        let mut args: Vec<&str> = NO_HOOKS.to_vec();
        args.extend(["commit", "-q", "--no-verify", "--allow-empty-message", "-m"]);
        let msg = format!("codetrail run {id}");
        args.push(&msg);
        gitrun::run_env(wt, &args, &env_refs(&env))?;
    }
    gitrun::out(wt, &["rev-parse", "HEAD"])
}

/// Non-isolated mode: records `base..current work tree` as a commit on `refs/codetrail/runs/<id>`
/// using a private temporary index and plumbing only; the user's index, HEAD and branches are never
/// written. Returns the commit (or `base_sha` if the tree equals base).
pub fn snapshot_worktree(repo_root: &Path, scratch: &Path, id: &str, base_sha: &str) -> Result<String> {
    fs::create_dir_all(scratch)?;
    let idx = scratch.join("index.tmp");
    let _ = fs::remove_file(&idx);
    let idx_s = idx.to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", idx_s.as_str())];
    let res = (|| -> Result<String> {
        gitrun::run_env(repo_root, &["read-tree", base_sha], &env)?;
        gitrun::run_env(repo_root, &["add", "-A"], &env)?;
        let tree = String::from_utf8_lossy(&gitrun::run_env(repo_root, &["write-tree"], &env)?).trim().to_string();
        let base_tree = gitrun::out(repo_root, &["rev-parse", &format!("{base_sha}^{{tree}}")])?;
        if tree == base_tree {
            return Ok(base_sha.to_string());
        }
        let aenv = author_env(id);
        let msg = format!("codetrail run {id} (in-place snapshot)");
        let commit = String::from_utf8_lossy(&gitrun::run_env(
            repo_root,
            &["commit-tree", &tree, "-p", base_sha, "-m", &msg],
            &env_refs(&aenv),
        )?)
        .trim()
        .to_string();
        gitrun::run(repo_root, &["update-ref", &run_ref(id), &commit])?;
        Ok(commit)
    })();
    let _ = fs::remove_file(&idx);
    res
}

/// `base..head` file changes with blob ids (renames reported as delete + add).
pub fn changes(repo_root: &Path, base: &str, head: &str) -> Result<Vec<Change>> {
    if base == head {
        return Ok(vec![]);
    }
    let raw = gitrun::run(repo_root, &["diff", "--raw", "-z", "--no-renames", "--no-abbrev", base, head])?;
    let mut toks = raw.split(|b| *b == 0).filter(|t| !t.is_empty());
    let mut out = Vec::new();
    while let Some(meta) = toks.next() {
        let Some(path) = toks.next() else { break };
        // ":<mode> <mode> <oid> <oid> <status>"
        let m = String::from_utf8_lossy(meta);
        let f: Vec<&str> = m.trim_start_matches(':').split_whitespace().collect();
        if f.len() < 5 {
            continue;
        }
        let oid = |s: &str| (!s.bytes().all(|c| c == b'0')).then(|| s.to_string());
        out.push(Change { path: String::from_utf8_lossy(path).into_owned(), pre_blob: oid(f[2]), post_blob: oid(f[3]) });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(dir: &Path, args: &[&str]) -> String {
        let env = [("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"), ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t")];
        String::from_utf8_lossy(&gitrun::run_env(dir, args, &env).unwrap()).trim().to_string()
    }

    #[test]
    fn failed_create_never_deletes_a_preexisting_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap().join("repo");
        let data = tmp.path().canonicalize().unwrap().join("data");
        fs::create_dir_all(&repo).unwrap();
        g(&repo, &["init", "-q", "-b", "main"]);
        g(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let base = g(&repo, &["rev-parse", "HEAD"]);
        g(&repo, &["commit", "-q", "--allow-empty", "-m", "second"]);
        g(&repo, &["branch", "ct/mine", "HEAD"]); // user's own branch that happens to match the name
        let tip = g(&repo, &["rev-parse", "ct/mine"]);
        assert_ne!(tip, base);
        let err = create_worktree(&data, &repo, "18c00000-00000001", "ct/mine", &base);
        assert!(err.is_err());
        assert_eq!(g(&repo, &["rev-parse", "refs/heads/ct/mine"]), tip, "pre-existing branch was touched");
        assert!(!worktree_path(&data, &repo, "18c00000-00000001").exists());
        // a fresh name works and is removable
        let wt = create_worktree(&data, &repo, "18c00000-00000002", "ct/fresh", &base).unwrap();
        assert!(wt.exists());
        remove_worktree(&data, &repo, &wt, Some("ct/fresh")).unwrap();
        assert!(!wt.exists());
    }
}
