//! Bounded local cache of a remote repository's AI-change log (`<git_dir>/codetrail/log.ct`).
//!
//! Layout: `<root>/<xxh3-128 hex of host/port/user/git_dir>/{log.ct,meta}`. `meta` holds the remote
//! `"<size> <mtime>"` seen when the file was downloaded; the file is fetched again only when that
//! changes. At most [`MAX_CACHE_DIRS`] cache directories are kept (least recently used evicted) and
//! one file is at most [`MAX_RECORDS_BYTES`].

use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use super::exec::quote_path;
use super::RemoteSpec;
use crate::git::{run_remote_script, RunOpts};
use crate::{Error, Result};

pub const MAX_RECORDS_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_CACHE_DIRS: usize = 8;

/// `$XDG_CACHE_HOME/codetrail/remote`, else `~/.cache/codetrail/remote`.
pub fn default_root() -> Result<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME").filter(|x| !x.is_empty()) {
        return Ok(PathBuf::from(x).join("codetrail/remote"));
    }
    match std::env::var_os("HOME").filter(|x| !x.is_empty()) {
        Some(h) => Ok(PathBuf::from(h).join(".cache/codetrail/remote")),
        None => Err(Error::Invalid("neither XDG_CACHE_HOME nor HOME is set".into())),
    }
}

fn key(spec: &RemoteSpec, git_dir: &str) -> String {
    let s = format!("{}\0{}\0{}\0{}", spec.host, spec.port.unwrap_or(0), spec.user.as_deref().unwrap_or(""), git_dir);
    format!("{:032x}", xxhash_rust::xxh3::xxh3_128(s.as_bytes()))
}

fn is_cache_dir_name(n: &str) -> bool {
    n.len() == 32 && n.bytes().all(|b| b.is_ascii_hexdigit())
}

fn mkdir_private(p: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(p)
}

fn parse_stat(out: &[u8]) -> Option<(u64, u64)> {
    let s = String::from_utf8_lossy(out);
    let l = s.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    let mut it = l.split_whitespace();
    let (a, b) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    it.next().is_none().then_some((a, b))
}

/// Returns the cached copy's path (downloading it first when the remote file changed), or `None`
/// when the remote has no log.
pub fn fetch(spec: &RemoteSpec, git_dir: &str, o: &RunOpts, root: &Path) -> Result<Option<PathBuf>> {
    let gd = quote_path(git_dir);
    let stat_script = format!(
        "if cd {gd}/codetrail 2>/dev/null && [ -f log.ct ]; then stat -c '%s %Y' log.ct 2>/dev/null || stat -f '%z %m' log.ct 2>/dev/null; else echo none; fi"
    );
    let mut so = o.clone();
    so.max_output = 4096;
    let (out, _) = run_remote_script(spec, &stat_script, "stat log.ct", None, &so, &[])?;
    let Some((size, mtime)) = parse_stat(&out) else { return Ok(None) };
    if size > MAX_RECORDS_BYTES {
        return Err(Error::TooLarge { cmd: "cat codetrail/log.ct".into(), limit: MAX_RECORDS_BYTES as usize });
    }
    let dir = root.join(key(spec, git_dir));
    let (log, meta) = (dir.join("log.ct"), dir.join("meta"));
    let want = format!("{size} {mtime}");
    let fresh = std::fs::read_to_string(&meta).is_ok_and(|m| m.trim() == want) && std::fs::metadata(&log).is_ok();
    if fresh {
        // refresh the LRU stamp
        let _ = std::fs::write(&meta, &want);
    } else {
        mkdir_private(&dir)?;
        let mut dl = o.clone();
        dl.max_output = MAX_RECORDS_BYTES as usize;
        let (data, _) = run_remote_script(spec, &format!("cd {gd}/codetrail && exec cat -- log.ct"), "cat log.ct", None, &dl, &[])?;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = dir.join(format!("log.ct.tmp{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        let res = (|| -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&data)?;
            f.flush()?;
            std::fs::rename(&tmp, &log)?;
            std::fs::write(&meta, &want)
        })();
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
    }
    evict(root, &dir);
    Ok(Some(log))
}

/// Keeps at most [`MAX_CACHE_DIRS`] cache directories, dropping the least recently used.
fn evict(root: &Path, keep: &Path) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    let mut dirs: Vec<(SystemTime, PathBuf)> = rd
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(is_cache_dir_name) && e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            let p = e.path();
            let t = std::fs::metadata(p.join("meta")).or_else(|_| e.metadata()).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            (t, p)
        })
        .collect();
    if dirs.len() <= MAX_CACHE_DIRS {
        return;
    }
    dirs.sort();
    let excess = dirs.len() - MAX_CACHE_DIRS;
    for (_, p) in dirs.into_iter().filter(|(_, p)| p != keep).take(excess) {
        let _ = std::fs::remove_dir_all(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_parse() {
        assert_eq!(parse_stat(b"123 1700000000\n"), Some((123, 1700000000)));
        assert_eq!(parse_stat(b"none\n"), None);
        assert_eq!(parse_stat(b""), None);
        assert_eq!(parse_stat(b"1 2 3\n"), None);
    }
}
