//! Working-tree changes: ct-core's worktree comparison covers tracked files (staged + unstaged
//! vs the old side) but not untracked ones. This module adds untracked, non-ignored files as
//! added files, and computes the cheap live summary shown in the Commits rail and the header.

use ct_core::{Comparison, DiffFile, DiffLine, DiffOpts, DiffSet, FileKind, FileStat, Hunk, LineKind, Repo, Status, Treeish};
use crate::proc::run_bounded;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Largest untracked file whose lines are listed; bigger ones are shown as added without a body.
const MAX_UNTRACKED_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub files: usize,
    pub add: u32,
    pub del: u32,
    pub untracked: usize,
    /// Changes whenever the set of changes or any changed file's size/mtime changes.
    pub sig: u64,
}

impl Summary {
    pub fn dirty(&self) -> bool {
        self.files > 0
    }

    /// "3 files +12 −4", or "clean".
    pub fn text(&self) -> String {
        if !self.dirty() {
            return "clean".into();
        }
        format!("{} {} +{} −{}", self.files, if self.files == 1 { "file" } else { "files" }, self.add, self.del)
    }
}

pub fn is_worktree(c: &Comparison) -> bool {
    c.new == Treeish::Worktree
}

const GIT_TIMEOUT: Duration = Duration::from_secs(20);

/// `git -C root --no-optional-locks <args>`: never takes the index lock, bounded in time.
fn git(root: &Path, args: &[&str], cancel: Option<&AtomicBool>) -> Result<Vec<u8>, String> {
    let mut c = Command::new("git");
    c.arg("-C").arg(root).arg("--no-optional-locks").args(args);
    let o = run_bounded(c, GIT_TIMEOUT, cancel)?;
    if !o.ok {
        return Err(o.stderr.trim().to_string());
    }
    Ok(o.stdout)
}

pub fn untracked(root: &Path) -> Result<Vec<String>, String> {
    let out = git(root, &["ls-files", "--others", "--exclude-standard", "-z"], None)?;
    Ok(out.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect())
}

/// Cheap change signature: one `git status` plus size/mtime of every listed file. No diff work.
fn status_sig(root: &Path, head: &str, cancel: Option<&AtomicBool>) -> Result<u64, String> {
    let out = git(root, &["status", "--porcelain=v1", "-z", "-uall", "--no-renames"], cancel)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    head.hash(&mut h);
    for e in out.split(|b| *b == 0).filter(|s| s.len() > 3) {
        e.hash(&mut h);
        let path = String::from_utf8_lossy(&e[3..]).into_owned();
        if let Ok(m) = std::fs::metadata(root.join(&path)) {
            m.len().hash(&mut h);
            m.modified().ok().hash(&mut h);
        }
    }
    Ok(h.finish())
}

fn read_untracked(root: &Path, rel: &str) -> (FileKind, Vec<String>, bool, bool) {
    let abs = root.join(rel);
    let Ok(meta) = std::fs::symlink_metadata(&abs) else { return (FileKind::Text, Vec::new(), false, false) };
    if meta.file_type().is_symlink() {
        return (FileKind::Symlink, Vec::new(), false, false);
    }
    if meta.is_dir() {
        return (FileKind::Submodule, Vec::new(), false, false);
    }
    if meta.len() > MAX_UNTRACKED_BYTES {
        return (FileKind::Binary, Vec::new(), false, false);
    }
    let Ok(bytes) = std::fs::read(&abs) else { return (FileKind::Text, Vec::new(), false, false) };
    if bytes.contains(&0) {
        return (FileKind::Binary, Vec::new(), false, false);
    }
    let text = String::from_utf8_lossy(&bytes);
    let no_nl = !text.is_empty() && !text.ends_with('\n');
    let lines = text.split('\n').map(str::to_string).collect::<Vec<_>>();
    let lines = if text.ends_with('\n') || text.is_empty() { lines[..lines.len() - 1].to_vec() } else { lines };
    (FileKind::Text, lines, no_nl, true)
}

/// Appends untracked files (as `Added`) to a worktree diff set.
pub fn add_untracked(repo: &Repo, set: &mut DiffSet) -> Result<(), String> {
    for rel in untracked(&repo.root)? {
        if set.files.iter().any(|f| f.path == rel) {
            continue;
        }
        let (kind, lines, _, _) = read_untracked(&repo.root, &rel);
        set.files.push(FileStat {
            path: rel,
            old_path: None,
            status: Status::Added,
            similarity: None,
            add: lines.len() as u32,
            del: 0,
            binary: kind == FileKind::Binary,
            kind,
            old_mode: None,
            new_mode: Some(0o100644),
        });
    }
    set.files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(())
}

/// `Repo::file_diff`, extended to untracked files of a worktree comparison.
pub fn file_diff(repo: &Repo, c: &Comparison, path: &str, o: &DiffOpts) -> Result<DiffFile, String> {
    match repo.file_diff(c, path, o) {
        Ok(f) => Ok(f),
        Err(e) => {
            if is_worktree(c) && untracked(&repo.root).is_ok_and(|u| u.iter().any(|p| p == path)) {
                Ok(untracked_file(&repo.root, path))
            } else {
                Err(e.to_string())
            }
        }
    }
}

fn untracked_file(root: &Path, rel: &str) -> DiffFile {
    let (kind, lines, no_nl, _) = read_untracked(root, rel);
    let n = lines.len();
    let hunks = if n == 0 {
        Vec::new()
    } else {
        vec![Hunk {
            old_start: 0,
            old_len: 0,
            new_start: 1,
            new_len: n as u32,
            section: String::new(),
            lines: lines.into_iter().enumerate().map(|(i, text)| DiffLine { kind: LineKind::Add, old_no: None, new_no: Some(i as u32 + 1), text, no_newline_at_eof: no_nl && i + 1 == n }).collect(),
        }]
    };
    DiffFile { path: rel.to_string(), old_path: None, old_oid: None, new_oid: None, old_mode: None, new_mode: Some(0o100644), kind, status: Status::Added, hunks }
}

/// Live summary of everything uncommitted: staged + unstaged vs HEAD, plus untracked files.
/// The numstat/diff work only runs when the cheap signature differs from `prev`.
pub fn summary(repo: &Repo, head: &str, prev: Option<&Summary>, cancel: Option<&AtomicBool>) -> Result<Summary, String> {
    let sig = status_sig(&repo.root, head, cancel)?;
    if let Some(p) = prev.filter(|p| p.sig == sig) {
        return Ok(p.clone());
    }
    let mut set = if head.is_empty() {
        DiffSet::default()
    } else {
        let cmp = Comparison::commit_vs_worktree(head).map_err(|e| e.to_string())?;
        repo.diff(&cmp, &DiffOpts::default()).map_err(|e| e.to_string())?
    };
    let tracked = set.files.len();
    add_untracked(repo, &mut set)?;
    Ok(Summary {
        files: set.files.len(),
        add: set.files.iter().map(|f| f.add).sum(),
        del: set.files.iter().map(|f| f.del).sum(),
        untracked: set.files.len() - tracked,
        sig,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let o = Command::new("git").current_dir(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap();
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }

    /// Repo with: staged edit (a), unstaged edit (b), staged+unstaged edit (c), untracked (n), ignored (x.log).
    fn dirty_repo() -> (tempfile::TempDir, Repo, String) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        git(p, &["init", "-q"]);
        for f in ["a", "b", "c"] {
            std::fs::write(p.join(f), "1\n2\n3\n").unwrap();
        }
        std::fs::write(p.join(".gitignore"), "*.log\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-q", "-m", "init"]);
        std::fs::write(p.join("a"), "1\n2\n3\n4\n").unwrap();
        git(p, &["add", "a"]);
        std::fs::write(p.join("b"), "1\n3\n").unwrap();
        std::fs::write(p.join("c"), "1\n2\n3\nX\n").unwrap();
        git(p, &["add", "c"]);
        std::fs::write(p.join("c"), "1\n2\n3\nX\nY\n").unwrap();
        std::fs::write(p.join("n"), "new1\nnew2\n").unwrap();
        std::fs::write(p.join("x.log"), "ignored\n").unwrap();
        let repo = Repo::open(p).unwrap();
        let head = repo.head().unwrap();
        (d, repo, head)
    }

    #[test]
    fn worktree_diff_lists_staged_unstaged_and_untracked_but_not_ignored() {
        let (_d, repo, head) = dirty_repo();
        let cmp = Comparison::commit_vs_worktree(&head).unwrap();
        let mut set = repo.diff(&cmp, &DiffOpts::default()).unwrap();
        add_untracked(&repo, &mut set).unwrap();
        let got: Vec<(&str, Status, u32, u32)> = set.files.iter().map(|f| (f.path.as_str(), f.status, f.add, f.del)).collect();
        assert_eq!(got, vec![("a", Status::Modified, 1, 0), ("b", Status::Modified, 0, 1), ("c", Status::Modified, 2, 0), ("n", Status::Added, 2, 0)]);
    }

    #[test]
    fn untracked_file_diff_is_all_added_lines_and_tracked_still_works() {
        let (_d, repo, head) = dirty_repo();
        let cmp = Comparison::commit_vs_worktree(&head).unwrap();
        let n = file_diff(&repo, &cmp, "n", &DiffOpts::default()).unwrap();
        assert_eq!(n.status, Status::Added);
        assert_eq!(n.hunks.len(), 1);
        assert_eq!(n.hunks[0].lines.iter().map(|l| (l.kind, l.new_no, l.text.as_str())).collect::<Vec<_>>(), vec![(LineKind::Add, Some(1), "new1"), (LineKind::Add, Some(2), "new2")]);
        let c = file_diff(&repo, &cmp, "c", &DiffOpts::default()).unwrap();
        assert_eq!(c.hunks[0].lines.iter().filter(|l| l.kind == LineKind::Add).count(), 2, "staged and unstaged edits combined");
        assert!(file_diff(&repo, &cmp, "nope", &DiffOpts::default()).is_err());
    }

    #[test]
    fn summary_counts_everything_and_signature_tracks_content() {
        let (d, repo, head) = dirty_repo();
        let s = summary(&repo, &head, None, None).unwrap();
        assert_eq!((s.files, s.add, s.del, s.untracked), (4, 5, 1, 1));
        assert_eq!(s.text(), "4 files +5 −1");
        assert_eq!(summary(&repo, &head, None, None).unwrap(), s, "stable while nothing changes");
        std::fs::write(d.path().join("n"), "new1\nnew2\nnew3 longer\n").unwrap();
        assert_ne!(summary(&repo, &head, None, None).unwrap().sig, s.sig);
    }

    #[test]
    fn clean_repo_is_clean() {
        let d = tempfile::tempdir().unwrap();
        git(d.path(), &["init", "-q"]);
        std::fs::write(d.path().join("f"), "x\n").unwrap();
        git(d.path(), &["add", "."]);
        git(d.path(), &["commit", "-q", "-m", "i"]);
        let repo = Repo::open(d.path()).unwrap();
        let s = summary(&repo, &repo.head().unwrap(), None, None).unwrap();
        assert!(!s.dirty());
        assert_eq!(s.text(), "clean");
    }
}
