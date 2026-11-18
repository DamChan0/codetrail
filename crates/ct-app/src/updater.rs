//! Self-update from GitHub releases: check the latest release, download the tarball and
//! `SHA256SUMS` with `curl`, verify, and atomically replace the running binary when it lives in a
//! user-writable folder (`~/.local/bin`, `~/.cargo/bin`). A system install (`.deb` in `/usr/bin`)
//! only gets the notice and a download link.

use crate::proc::run_bounded;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const REPO: &str = "DamChan0/codetrail";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub page: String,
    pub tarball: Option<String>,
    pub sums: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum UpdState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Installing(Release),
    /// Binary replaced; restart to run it.
    Installed(String),
    /// The install failed (shown in the notice).
    Failed(String),
    /// The check failed (offline, no curl): only shown in Settings > About.
    CheckFailed(String),
}

/// `"v1.2.3"` / `"1.2.3"` → (1, 2, 3); anything unparsable is `None`.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut it = s.trim().trim_start_matches('v').split(['.', '-']).map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse_version(latest), parse_version(current)), (Some(a), Some(b)) if a > b)
}

fn api_url() -> String {
    std::env::var("CT_UPDATE_URL").unwrap_or_else(|_| format!("https://api.github.com/repos/{REPO}/releases/latest"))
}

fn curl(args: &[&str], timeout: Duration) -> Result<Vec<u8>, String> {
    let mut c = Command::new("curl");
    c.args(["-fsSL", "--max-time", &timeout.as_secs().to_string()]).args(args);
    let o = run_bounded(c, timeout + Duration::from_secs(2), None).map_err(|e| format!("curl: {e}"))?;
    if !o.ok {
        return Err(format!("download failed: {}", o.stderr.trim()));
    }
    Ok(o.stdout)
}

/// Parses the GitHub "latest release" JSON.
pub fn parse_release(json: &[u8]) -> Result<Release, String> {
    let v: serde_json::Value = serde_json::from_slice(json).map_err(|e| format!("bad release data: {e}"))?;
    let tag = v["tag_name"].as_str().ok_or("release has no tag")?;
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    let find = |pred: &dyn Fn(&str) -> bool| assets.iter().find(|a| a["name"].as_str().is_some_and(pred)).and_then(|a| a["browser_download_url"].as_str()).map(str::to_string);
    Ok(Release {
        version: tag.trim_start_matches('v').to_string(),
        page: v["html_url"].as_str().unwrap_or_default().to_string(),
        tarball: find(&|n| n.ends_with("-linux-x86_64.tar.gz")),
        sums: find(&|n| n == "SHA256SUMS"),
    })
}

/// Version this process already installed (waiting for a restart).
static INSTALLED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn installed() -> Option<String> {
    INSTALLED.lock().ok().and_then(|g| g.clone())
}

/// `Ok(None)` = up to date (or that version is already installed and only needs a restart).
pub fn check() -> Result<Option<Release>, String> {
    let body = curl(&["-H", "Accept: application/vnd.github+json", &api_url()], Duration::from_secs(15))?;
    let r = parse_release(&body)?;
    let have = INSTALLED.lock().map(|g| g.clone()).ok().flatten().filter(|v| is_newer(v, CURRENT)).unwrap_or_else(|| CURRENT.to_string());
    Ok(is_newer(&r.version, &have).then_some(r))
}

/// The binary can replace itself: its folder is writable and not a system prefix.
pub fn self_updatable(exe: &Path) -> bool {
    let Some(dir) = exe.parent() else { return false };
    if dir.starts_with("/usr") || dir.starts_with("/opt") {
        return false;
    }
    let probe = dir.join(format!(".codetrail-write-test-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Path of the running binary, taken once at startup. After the update swaps the file, Linux
/// reports `current_exe()` as `<path> (deleted)` (the old inode), which must never be used to
/// relaunch or to install again.
pub fn exe_path() -> Option<PathBuf> {
    static P: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    P.get_or_init(|| std::env::current_exe().ok().map(|p| strip_deleted(&p))).clone()
}

pub fn strip_deleted(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(&s).to_string())
}

/// [`self_updatable`] for the running binary, probed once per process.
pub fn running_self_updatable() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| exe_path().is_some_and(|e| self_updatable(&e)))
}

fn sha256(path: &Path) -> Result<String, String> {
    let mut c = Command::new("sha256sum");
    c.arg(path);
    let o = run_bounded(c, Duration::from_secs(30), None)?;
    let s = String::from_utf8_lossy(&o.stdout);
    s.split_whitespace().next().map(str::to_string).filter(|h| o.ok && h.len() == 64).ok_or_else(|| "sha256sum failed".into())
}

/// Downloads, verifies and swaps in the new binary at `exe`. Returns the installed version.
pub fn install(r: &Release, exe: &Path) -> Result<String, String> {
    let (Some(tar_url), Some(sums_url)) = (&r.tarball, &r.sums) else { return Err("the release has no Linux tarball".into()) };
    let dir = tempfile_dir()?;
    let res = install_in(&dir, tar_url, sums_url, exe);
    let _ = std::fs::remove_dir_all(&dir);
    res
}

fn tempfile_dir() -> Result<PathBuf, String> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let d = std::env::temp_dir().join(format!("codetrail-update-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    Ok(d)
}

fn install_in(dir: &Path, tar_url: &str, sums_url: &str, exe: &Path) -> Result<String, String> {
    let name = tar_url.rsplit('/').next().unwrap_or("codetrail.tar.gz");
    let tar = dir.join(name);
    std::fs::write(&tar, curl(&[tar_url], Duration::from_secs(300))?).map_err(|e| e.to_string())?;
    let sums = String::from_utf8_lossy(&curl(&[sums_url], Duration::from_secs(30))?).into_owned();
    let want = sums.lines().find_map(|l| l.split_once(char::is_whitespace).filter(|(_, n)| n.trim().trim_start_matches('*') == name).map(|(h, _)| h.to_string())).ok_or("SHA256SUMS does not list the tarball")?;
    if sha256(&tar)? != want {
        return Err("checksum mismatch: download discarded".into());
    }
    let mut c = Command::new("tar");
    c.arg("-xzf").arg(&tar).arg("-C").arg(dir);
    let o = run_bounded(c, Duration::from_secs(60), None)?;
    if !o.ok {
        return Err(format!("cannot unpack: {}", o.stderr.trim()));
    }
    let stem = name.trim_end_matches(".tar.gz");
    let new_bin = dir.join(stem).join("bin/codetrail");
    let mut v = Command::new(&new_bin);
    v.arg("--version");
    let o = run_bounded(v, Duration::from_secs(10), None)?;
    let ver = String::from_utf8_lossy(&o.stdout).split_whitespace().nth(1).unwrap_or_default().to_string();
    if !o.ok || ver.is_empty() {
        return Err("the downloaded binary does not run on this system".into());
    }
    let exe = &strip_deleted(exe);
    let staged = exe.with_extension("update");
    std::fs::copy(&new_bin, &staged).map_err(|e| format!("cannot write {}: {e}", staged.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    std::fs::rename(&staged, exe).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        format!("cannot replace {}: {e}", exe.display())
    })?;
    if let Ok(mut g) = INSTALLED.lock() {
        *g = Some(ver.clone());
    }
    Ok(ver)
}

/// Starts `exe` again with this process' arguments, detached.
pub fn relaunch(exe: &Path) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    Command::new(exe).args(std::env::args_os().skip(1)).process_group(0).spawn().map(|_| ()).map_err(|e| e.to_string())
}

pub fn open_url(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = Command::new(opener).arg(url).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert_eq!(parse_version("v0.10.2"), Some((0, 10, 2)));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(!is_newer("0.4.0", "0.4.0") && !is_newer("0.3.9", "0.4.0") && !is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn release_json_finds_the_tarball_and_sums() {
        let j = br#"{"tag_name":"v9.1.0","html_url":"https://x/r","assets":[
            {"name":"codetrail_9.1.0_amd64.deb","browser_download_url":"https://x/deb"},
            {"name":"codetrail-9.1.0-linux-x86_64.tar.gz","browser_download_url":"https://x/tgz"},
            {"name":"SHA256SUMS","browser_download_url":"https://x/sums"}]}"#;
        let r = parse_release(j).unwrap();
        assert_eq!((r.version.as_str(), r.tarball.as_deref(), r.sums.as_deref()), ("9.1.0", Some("https://x/tgz"), Some("https://x/sums")));
        assert!(parse_release(b"{}").is_err());
    }

    #[test]
    fn a_swapped_binary_path_loses_the_deleted_suffix() {
        assert_eq!(strip_deleted(Path::new("/home/u/.local/bin/codetrail (deleted)")), PathBuf::from("/home/u/.local/bin/codetrail"));
        assert_eq!(strip_deleted(Path::new("/x/codetrail")), PathBuf::from("/x/codetrail"));
    }

    #[test]
    fn system_folders_are_not_self_updatable() {
        assert!(!self_updatable(Path::new("/usr/bin/codetrail")));
        let d = tempfile::tempdir().unwrap();
        assert!(self_updatable(&d.path().join("codetrail")));
    }

    /// End to end against file:// URLs: a fake release whose "binary" is a shell script.
    #[test]
    fn install_verifies_the_checksum_and_swaps_the_binary() {
        let d = tempfile::tempdir().unwrap();
        let stage = d.path().join("codetrail-9.9.9-linux-x86_64/bin");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("codetrail"), "#!/bin/sh\necho codetrail 9.9.9\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(stage.join("codetrail"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let tgz = d.path().join("codetrail-9.9.9-linux-x86_64.tar.gz");
        assert!(Command::new("tar").arg("-czf").arg(&tgz).arg("-C").arg(d.path()).arg("codetrail-9.9.9-linux-x86_64").status().unwrap().success());
        let sums = d.path().join("SHA256SUMS");
        std::fs::write(&sums, format!("{}  codetrail-9.9.9-linux-x86_64.tar.gz\n", sha256(&tgz).unwrap())).unwrap();
        let exe = d.path().join("app/codetrail");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "old").unwrap();
        let r = Release { version: "9.9.9".into(), page: String::new(), tarball: Some(format!("file://{}", tgz.display())), sums: Some(format!("file://{}", sums.display())) };
        assert_eq!(install(&r, &exe).unwrap(), "9.9.9");
        assert!(std::fs::read_to_string(&exe).unwrap().contains("9.9.9"));
        // a tampered checksum leaves the binary alone
        std::fs::write(&exe, "old").unwrap();
        std::fs::write(&sums, format!("{}  codetrail-9.9.9-linux-x86_64.tar.gz\n", "0".repeat(64))).unwrap();
        assert!(install(&r, &exe).unwrap_err().contains("checksum"));
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
    }

    /// Network: installs the real latest release into a temp folder (`cargo test -- --ignored`).
    #[test]
    #[ignore]
    fn real_latest_release_installs() {
        let body = curl(&["-H", "Accept: application/vnd.github+json", &api_url()], Duration::from_secs(15)).unwrap();
        let r = parse_release(&body).unwrap();
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("codetrail");
        std::fs::write(&exe, "old").unwrap();
        assert_eq!(install(&r, &exe).unwrap(), r.version);
    }
}
