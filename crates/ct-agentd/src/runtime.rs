//! App-private runtime: `node@22` + pinned pi under `<data>/agent`, installed by `npm ci` from an
//! embedded lockfile. Idempotent.

use crate::config::Config;
use crate::proc::{self, Spec};
use crate::util::{lossy_masked, read_lf_lines};
use crate::{Error, Result, RuntimeStatus};
use serde_json::Value;
use std::ffi::OsString;

const PACKAGE_JSON: &str = include_str!("../assets/package.json");
const PACKAGE_LOCK: &str = include_str!("../assets/package-lock.json");
const STAMP: &str = ".ct-runtime-stamp";

/// Identifies the embedded lockfile; a changed lock re-runs `npm ci`.
fn stamp() -> String {
    // FNV-1a: only for change detection, not security
    let mut h: u64 = 0xcbf29ce484222325;
    for b in PACKAGE_JSON.bytes().chain(PACKAGE_LOCK.bytes()) {
        h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn pkg_version(path: &std::path::Path) -> Option<String> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v.get("version")?.as_str().map(str::to_string)
}

pub(crate) fn status(cfg: &Config) -> RuntimeStatus {
    let dir = cfg.agent_dir();
    let node = cfg.node_bin().exists().then(|| pkg_version(&dir.join("node_modules/node/package.json"))).flatten();
    let pi_pkg = pkg_version(&dir.join("node_modules/@earendil-works/pi-coding-agent/package.json"));
    let pi = if let Some(p) = &cfg.pi_path {
        p.exists().then(|| format!("custom: {}", p.display()))
    } else if cfg.pi_cli_js().exists() {
        pi_pkg
    } else {
        None
    };
    let installed = if cfg.pi_path.is_some() {
        pi.is_some()
    } else {
        node.is_some() && pi.is_some() && std::fs::read_to_string(dir.join(STAMP)).is_ok_and(|s| s.trim() == stamp())
    };
    RuntimeStatus { installed, node, pi }
}

pub(crate) fn setup(cfg: &Config, progress: &dyn Fn(&str)) -> Result<()> {
    if status(cfg).installed {
        progress("agent runtime already installed");
        return Ok(());
    }
    if cfg.pi_path.is_some() {
        return Err(Error::Other("agent.pi_path is set but does not exist".into()));
    }
    let dir = cfg.agent_dir();
    std::fs::create_dir_all(&dir).map_err(|e| Error::Other(format!("cannot create {}: {e}", dir.display())))?;
    for (name, body) in [("package.json", PACKAGE_JSON), ("package-lock.json", PACKAGE_LOCK)] {
        std::fs::write(dir.join(name), body).map_err(|e| Error::Other(format!("cannot write {name}: {e}")))?;
    }
    progress("installing node and pi (npm ci)...");
    let args: Vec<OsString> = ["ci", "--no-audit", "--no-fund", "--loglevel=error"].map(OsString::from).into();
    let mut env = proc::whitelist_env(&["npm_config_registry", "NPM_CONFIG_REGISTRY", "npm_config_cache", "NPM_CONFIG_CACHE"], &[]);
    proc::set_env(&mut env, "npm_config_update_notifier", "false");
    let sp = proc::spawn(Spec { program: cfg.npm_bin.clone(), args, cwd: Some(dir.clone()), env, ceiling: Some(cfg.install_ceiling) }).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Other("npm is required to install the agent runtime but was not found on PATH".into())
        } else {
            Error::Other(format!("cannot run npm: {e}"))
        }
    })?;
    drop(sp.stdin);
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut out = sp.stdout;
    let mut err = sp.stderr;
    let (t1, t2) = (tx.clone(), tx);
    let a = std::thread::spawn(move || read_lf_lines(&mut out, |l| drop(t1.send(lossy_masked(&l))), |_| {}));
    let b = std::thread::spawn(move || read_lf_lines(&mut err, |l| drop(t2.send(lossy_masked(&l))), |_| {}));
    let mut tail: std::collections::VecDeque<String> = Default::default();
    for line in rx {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        progress(&line);
        if tail.len() == 8 {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    let _ = (a.join(), b.join());
    let code = sp.host.wait_exit(std::time::Duration::from_secs(30)).flatten();
    if code != Some(0) {
        let t: Vec<String> = tail.into_iter().collect();
        return Err(Error::Other(format!("npm ci failed (exit {}): {}", code.map_or("signal".into(), |c| c.to_string()), t.join(" | "))));
    }
    let st = status_without_stamp(cfg);
    if !st {
        return Err(Error::Other("npm ci finished but node/pi were not installed".into()));
    }
    std::fs::write(dir.join(STAMP), stamp()).map_err(|e| Error::Other(format!("cannot write stamp: {e}")))?;
    progress("agent runtime installed");
    Ok(())
}

fn status_without_stamp(cfg: &Config) -> bool {
    cfg.node_bin().exists() && cfg.pi_cli_js().exists()
}
