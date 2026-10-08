//! Recent projects and folder-browser helpers: pure functions, no UI.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const MAX_RECENT: usize = 12;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecentProject {
    pub path: String,
    /// Unix milliseconds of the last open.
    pub opened_ms: i64,
}

/// Records `path` as opened now: most recent first, de-duplicated, capped at `MAX_RECENT`.
pub fn touch(list: &mut Vec<RecentProject>, path: &Path, now_ms: i64) {
    let p = path.to_string_lossy().into_owned();
    list.retain(|r| r.path != p);
    list.insert(0, RecentProject { path: p, opened_ms: now_ms });
    list.truncate(MAX_RECENT);
}

pub fn remove(list: &mut Vec<RecentProject>, path: &str) {
    list.retain(|r| r.path != path);
}

/// Most recent first, capped. Used on load; `exists` is injected so tests need no filesystem.
pub fn normalize(list: Vec<RecentProject>, exists: &dyn Fn(&Path) -> bool) -> Vec<RecentProject> {
    let mut l: Vec<RecentProject> = list.into_iter().filter(|r| exists(Path::new(&r.path))).collect();
    l.sort_by(|a, b| b.opened_ms.cmp(&a.opened_ms));
    let mut seen = std::collections::HashSet::new();
    l.retain(|r| seen.insert(r.path.clone()));
    l.truncate(MAX_RECENT);
    l
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// `~`, `~/x` expansion; anything else is returned as typed (trimmed).
pub fn expand_tilde(input: &str, home: &Path) -> PathBuf {
    let t = input.trim();
    if t == "~" {
        home.to_path_buf()
    } else if let Some(rest) = t.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(t)
    }
}

/// `/home/me/proj` → `~/proj`.
pub fn abbrev_home(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// Keeps the tail of a path (the part that identifies it) when it exceeds `max_chars`: `…/a/b`.
pub fn shorten_path(path: &str, max_chars: usize) -> String {
    if path.chars().count() <= max_chars {
        return path.to_string();
    }
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut out = String::new();
    for part in parts.iter().rev() {
        let next = if out.is_empty() { (*part).to_string() } else { format!("{part}/{out}") };
        if next.chars().count() + 2 > max_chars && !out.is_empty() {
            break;
        }
        out = next;
    }
    format!("…/{out}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirKind {
    Git,
    Plain,
}

pub fn is_git_dir(p: &Path) -> bool {
    p.join(".git").exists()
}

/// Inspects a typed/selected folder. Errors are user-facing sentences.
pub fn check_dir(p: &Path) -> Result<DirKind, String> {
    match std::fs::metadata(p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!("{} does not exist.", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(format!("No permission to read {}.", p.display())),
        Err(e) => Err(format!("Cannot read {}: {e}", p.display())),
        Ok(m) if !m.is_dir() => Err(format!("{} is a file, not a folder.", p.display())),
        Ok(_) => Ok(if is_git_dir(p) { DirKind::Git } else { DirKind::Plain }),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntryInfo {
    pub name: String,
    pub is_git: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    pub dir: PathBuf,
    pub kind: DirKind,
    /// Sub-folders: git repositories first is NOT applied; plain alphabetical (case-insensitive).
    pub entries: Vec<DirEntryInfo>,
}

pub fn list_dir(p: &Path) -> Result<Listing, String> {
    let kind = check_dir(p)?;
    let rd = std::fs::read_dir(p).map_err(|e| format!("Cannot list {}: {e}", p.display()))?;
    let mut entries: Vec<DirEntryInfo> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (!name.starts_with('.')).then(|| DirEntryInfo { is_git: is_git_dir(&e.path()), name })
        })
        .collect();
    entries.sort_by_key(|e| e.name.to_lowercase());
    Ok(Listing { dir: p.to_path_buf(), kind, entries })
}

/// Where the folder browser starts: the last project's parent when it exists, else `$HOME`.
pub fn browser_start(recent: &[RecentProject], home: &Path) -> PathBuf {
    recent.first().and_then(|r| Path::new(&r.path).parent().map(Path::to_path_buf)).filter(|p| p.is_dir()).unwrap_or_else(|| home.to_path_buf())
}

/// Breadcrumb segments: (label, absolute path).
pub fn breadcrumbs(p: &Path) -> Vec<(String, PathBuf)> {
    let mut acc = PathBuf::new();
    let mut out = Vec::new();
    for c in p.components() {
        acc.push(c);
        let label = match c {
            std::path::Component::RootDir => "/".to_string(),
            other => other.as_os_str().to_string_lossy().into_owned(),
        };
        out.push((label, acc.clone()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rp(p: &str, t: i64) -> RecentProject {
        RecentProject { path: p.into(), opened_ms: t }
    }

    #[test]
    fn touch_moves_to_front_dedups_and_caps_at_twelve() {
        let mut l = Vec::new();
        for i in 0..15 {
            touch(&mut l, Path::new(&format!("/p/{i}")), i);
        }
        assert_eq!(l.len(), MAX_RECENT);
        assert_eq!(l[0].path, "/p/14");
        touch(&mut l, Path::new("/p/5"), 99);
        assert_eq!(l[0], rp("/p/5", 99));
        assert_eq!(l.iter().filter(|r| r.path == "/p/5").count(), 1);
        remove(&mut l, "/p/5");
        assert!(l.iter().all(|r| r.path != "/p/5"));
    }

    #[test]
    fn normalize_drops_missing_sorts_newest_first_and_dedups() {
        let l = vec![rp("/a", 1), rp("/gone", 9), rp("/b", 5), rp("/a", 3)];
        let out = normalize(l, &|p| p != Path::new("/gone"));
        assert_eq!(out.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["/b", "/a"]);
        assert_eq!(out[1].opened_ms, 3, "newest duplicate wins");
    }

    #[test]
    fn tilde_expansion_and_abbreviation_round_trip() {
        let home = Path::new("/home/me");
        assert_eq!(expand_tilde(" ~ ", home), home);
        assert_eq!(expand_tilde("~/code/x", home), Path::new("/home/me/code/x"));
        assert_eq!(expand_tilde("/etc", home), Path::new("/etc"));
        assert_eq!(expand_tilde("~other", home), Path::new("~other"));
        assert_eq!(abbrev_home(Path::new("/home/me/code/x"), home), "~/code/x");
        assert_eq!(abbrev_home(home, home), "~");
        assert_eq!(abbrev_home(Path::new("/home/meow/x"), home), "/home/meow/x", "prefix must be a path component");
    }

    #[test]
    fn shorten_keeps_the_identifying_tail() {
        assert_eq!(shorten_path("~/code/x", 20), "~/code/x");
        assert_eq!(shorten_path("~/very/long/nested/folders/project", 20), "…/folders/project");
        assert!(shorten_path("~/a/b/c/averyveryverylongprojectname", 12).starts_with("…/"));
    }

    #[test]
    fn check_dir_distinguishes_git_plain_missing_and_file() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("r");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let plain = d.path().join("p");
        std::fs::create_dir(&plain).unwrap();
        let file = d.path().join("f");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(check_dir(&repo), Ok(DirKind::Git));
        assert_eq!(check_dir(&plain), Ok(DirKind::Plain));
        assert!(check_dir(&d.path().join("nope")).unwrap_err().contains("does not exist"));
        assert!(check_dir(&file).unwrap_err().contains("not a folder"));
        // worktree-style `.git` file counts as a repo
        let wt = d.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: /x").unwrap();
        assert_eq!(check_dir(&wt), Ok(DirKind::Git));
    }

    #[test]
    fn listing_hides_dot_dirs_and_files_and_flags_repos() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("Beta/.git")).unwrap();
        std::fs::create_dir(d.path().join("alpha")).unwrap();
        std::fs::create_dir(d.path().join(".hidden")).unwrap();
        std::fs::write(d.path().join("file.txt"), "x").unwrap();
        let l = list_dir(d.path()).unwrap();
        assert_eq!(l.entries, vec![DirEntryInfo { name: "alpha".into(), is_git: false }, DirEntryInfo { name: "Beta".into(), is_git: true }]);
        assert_eq!(l.kind, DirKind::Plain);
    }

    #[test]
    fn browser_starts_at_last_projects_parent_else_home() {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join("proj");
        std::fs::create_dir(&proj).unwrap();
        let home = Path::new("/home/me");
        assert_eq!(browser_start(&[rp(proj.to_str().unwrap(), 1)], home), d.path());
        assert_eq!(browser_start(&[rp("/definitely/missing/x", 1)], home), home);
        assert_eq!(browser_start(&[], home), home);
    }

    #[test]
    fn breadcrumbs_cover_every_ancestor() {
        let b = breadcrumbs(Path::new("/home/me/x"));
        assert_eq!(b.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(), ["/", "home", "me", "x"]);
        assert_eq!(b[2].1, Path::new("/home/me"));
    }
}
