#![allow(dead_code)]
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct T {
    pub dir: tempfile::TempDir,
}

pub fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "core.quotepath=false", "-c", "protocol.file.allow=always"])
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

impl T {
    pub fn new() -> T {
        let t = T { dir: tempfile::tempdir().unwrap() };
        git_in(t.path(), &["init", "-q", "-b", "main"]);
        t
    }
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
    pub fn git(&self, args: &[&str]) -> String {
        git_in(self.path(), args)
    }
    pub fn write(&self, rel: &str, content: &[u8]) {
        let p = self.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    pub fn rm(&self, rel: &str) {
        std::fs::remove_file(self.path().join(rel)).unwrap();
    }
    pub fn commit_all(&self, msg: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }
    pub fn repo(&self) -> ct_core::Repo {
        ct_core::Repo::open(self.path()).unwrap()
    }
}

pub fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap()
}

/// Orphan audit: no process may outlive the test with a cwd inside its temp dir.
impl Drop for T {
    fn drop(&mut self) {
        let root = self.dir.path().canonicalize().unwrap_or_else(|_| self.dir.path().to_path_buf());
        let me = std::process::id();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let mut left = Vec::new();
            for e in std::fs::read_dir("/proc").unwrap().flatten() {
                let Some(p) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
                let alive = std::fs::read_to_string(format!("/proc/{p}/stat")).is_ok_and(|s| !s[s.rfind(')').unwrap_or(0)..].starts_with(") Z"));
                if p != me && alive && std::fs::read_link(format!("/proc/{p}/cwd")).is_ok_and(|c| c.starts_with(&root)) {
                    left.push(p);
                }
            }
            if left.is_empty() || std::thread::panicking() {
                return;
            }
            assert!(std::time::Instant::now() < end, "orphan processes left by test: {left:?}");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
