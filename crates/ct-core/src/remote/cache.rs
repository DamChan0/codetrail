//! Bounded local cache of a remote repository's AI-change log (`<git_dir>/codetrail/log.ct`).
//!
//! Layout: `<root>/<xxh3-128 hex of every connection field + git_dir>/{log.ct,meta}`. `meta` holds the
//! remote `"<size> <mtime> <digest>"` seen when the file was downloaded; the file is fetched again only when that
//! changes. At most [`MAX_CACHE_DIRS`] cache directories are kept (least recently used evicted) and
//! one file is at most [`MAX_RECORDS_BYTES`].

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use super::exec::{ensure_private_dir, quote_path};
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
    // every connection field (identity, transport, host-key policy, ...) is part of the namespace
    let s = format!("{}\0{}", super::exec::policy_string(spec), git_dir);
    format!("{:032x}", xxhash_rust::xxh3::xxh3_128(s.as_bytes()))
}

fn is_cache_dir_name(n: &str) -> bool {
    n.len() == 32 && n.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `<size> <mtime> <digest>` printed by the remote stat script; the digest is `-` when the remote has
/// no checksum tool (then the cache is never considered fresh).
fn parse_stat(out: &[u8]) -> Option<(u64, u64, String)> {
    let s = String::from_utf8_lossy(out);
    let l = s.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    let mut it = l.split_whitespace();
    let (a, b) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    let d = it.next()?;
    if it.next().is_some() || d.len() > 64 || !d.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-') {
        return None;
    }
    Some((a, b, d.to_string()))
}

/// Unlinks `p` if it is a symlink (never follows it). Anything else is left alone.
fn drop_if_symlink(p: &Path) {
    if std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
        let _ = std::fs::remove_file(p);
    }
}

fn read_nofollow(p: &Path, max: u64) -> Option<String> {
    let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(p).ok()?;
    let mut s = String::new();
    f.take(max).read_to_string(&mut s).ok()?;
    Some(s)
}

fn random_suffix() -> String {
    let mut b = [0u8; 8];
    let ok = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_ok();
    if !ok {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        b = (n ^ SEQ.fetch_add(0x9e37_79b9, Ordering::Relaxed) ^ u64::from(std::process::id())).to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Writes `data` to `<dir>/<name>` through a random temp file created with
/// `O_CREAT|O_EXCL|O_NOFOLLOW` (mode 0600) and an atomic rename (which replaces, never follows,
/// an existing symlink at the destination).
fn write_atomic(dir: &Path, name: &str, data: &[u8]) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{name}.{}.tmp", random_suffix()));
    let res = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).custom_flags(libc::O_NOFOLLOW).mode(0o600).open(&tmp)?;
        f.write_all(data)?;
        f.flush()?;
        std::fs::rename(&tmp, dir.join(name))
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Returns the cached copy's path (downloading it first when the remote file changed), or `None`
/// when the remote has no log.
///
/// Trust model: the cache root and the entry directory must be real directories owned by us
/// (mode 0700; a symlink at the entry is unlinked, never followed); files inside are opened with
/// `O_NOFOLLOW` and written through random `O_EXCL` temp names. Freshness needs remote size, mtime
/// AND a content digest (`sha1sum`/`shasum`/`cksum`) to match; without a digest tool the file is
/// downloaded every time.
pub fn fetch(spec: &RemoteSpec, git_dir: &str, o: &RunOpts, root: &Path) -> Result<Option<PathBuf>> {
    let gd = quote_path(git_dir);
    let stat_script = format!(
        "if cd {gd}/codetrail 2>/dev/null && [ -f log.ct ]; then s=$(stat -c '%s %Y' log.ct 2>/dev/null || stat -f '%z %m' log.ct 2>/dev/null) || exit 0; \
         size=${{s%% *}}; d=; \
         if [ \"$size\" -le {MAX_RECORDS_BYTES} ]; then d=$( (sha1sum log.ct 2>/dev/null || shasum log.ct 2>/dev/null || cksum log.ct 2>/dev/null) | cut -d' ' -f1); fi; \
         echo \"$s ${{d:--}}\"; else echo none; fi"
    );
    let mut so = o.clone();
    so.max_output = 4096;
    let (out, _) = run_remote_script(spec, &stat_script, "stat log.ct", None, &so, &[])?;
    let Some((size, mtime, digest)) = parse_stat(&out) else { return Ok(None) };
    if size > MAX_RECORDS_BYTES {
        return Err(Error::TooLarge { cmd: "cat codetrail/log.ct".into(), limit: MAX_RECORDS_BYTES as usize });
    }
    ensure_private_dir(root)?;
    let dir = root.join(key(spec, git_dir));
    drop_if_symlink(&dir);
    ensure_private_dir(&dir)?;
    let (log, meta) = (dir.join("log.ct"), dir.join("meta"));
    drop_if_symlink(&log);
    let want = format!("{size} {mtime} {digest}");
    let have_log = std::fs::symlink_metadata(&log).is_ok_and(|m| m.is_file());
    let fresh = digest != "-" && have_log && read_nofollow(&meta, 256).is_some_and(|m| m.trim() == want);
    if fresh {
        // refresh the LRU stamp
        if let Ok(f) = std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW).open(&meta) {
            let _ = f.set_modified(SystemTime::now());
        }
    } else {
        drop_if_symlink(&meta);
        let mut dl = o.clone();
        dl.max_output = MAX_RECORDS_BYTES as usize;
        let (data, _) = run_remote_script(spec, &format!("cd {gd}/codetrail && exec cat -- log.ct"), "cat log.ct", None, &dl, &[])?;
        write_atomic(&dir, "log.ct", &data)?;
        write_atomic(&dir, "meta", want.as_bytes())?;
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
            let t = std::fs::symlink_metadata(p.join("meta")).or_else(|_| e.metadata()).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
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
        assert_eq!(parse_stat(b"123 1700000000 ab12\n"), Some((123, 1700000000, "ab12".into())));
        assert_eq!(parse_stat(b"123 1700000000 -\n"), Some((123, 1700000000, "-".into())));
        assert_eq!(parse_stat(b"none\n"), None);
        assert_eq!(parse_stat(b""), None);
        assert_eq!(parse_stat(b"1 2\n"), None);
        assert_eq!(parse_stat(b"1 2 zz\n"), None);
        assert_eq!(parse_stat(b"1 2 ab cd\n"), None);
    }
}
