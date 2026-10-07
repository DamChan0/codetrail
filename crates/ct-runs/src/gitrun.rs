//! Minimal `git` runner with stderr in errors, env, stdin and a hard timeout.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

const TIMEOUT: Duration = Duration::from_secs(120);

/// Variables git (and the hooks / filters it runs) may see. Everything else — provider tokens in
/// particular — is dropped.
const PASS_THROUGH: &[&str] = &["PATH", "HOME", "USER", "LOGNAME", "LANG", "LANGUAGE", "TMPDIR", "TZ", "XDG_CONFIG_HOME"];

/// Absolute git binary: a cleaned environment must not depend on PATH wrappers that need extra
/// variables.
static GIT_BIN: std::sync::LazyLock<std::path::PathBuf> = std::sync::LazyLock::new(|| {
    ["/usr/bin/git", "/usr/local/bin/git", "/opt/homebrew/bin/git", "/bin/git"]
        .iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.exists())
        .unwrap_or_else(|| "git".into())
});

pub struct Out {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Runs git and returns its output whatever the exit code (spawn / timeout failures are errors).
pub fn run_raw(cwd: &Path, args: &[&str], env: &[(&str, &str)], stdin: Option<&[u8]>) -> Result<Out> {
    let mut cmd = Command::new(&*GIT_BIN);
    cmd.current_dir(cwd).args(args).env_clear();
    for (k, v) in std::env::vars_os() {
        if k.to_str().is_some_and(|k| PASS_THROUGH.contains(&k) || k.starts_with("LC_")) {
            cmd.env(k, v);
        }
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| anyhow!("cannot run git: {e}"))?;
    if let Some(inp) = stdin {
        let mut si = child.stdin.take().ok_or_else(|| anyhow!("no git stdin"))?;
        let inp = inp.to_vec();
        std::thread::spawn(move || {
            let _ = si.write_all(&inp);
        });
    }
    let mut so = child.stdout.take().ok_or_else(|| anyhow!("no git stdout"))?;
    let mut se = child.stderr.take().ok_or_else(|| anyhow!("no git stderr"))?;
    let rd_o = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = so.read_to_end(&mut v);
        v
    });
    let rd_e = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = se.read_to_end(&mut v);
        v
    });
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait()? {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow!("git {} timed out", args.join(" ")));
            }
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    };
    Ok(Out {
        code: status.code().unwrap_or(-1),
        stdout: rd_o.join().unwrap_or_default(),
        stderr: String::from_utf8_lossy(&rd_e.join().unwrap_or_default()).trim().to_string(),
    })
}

pub fn run_env(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<Vec<u8>> {
    let o = run_raw(cwd, args, env, None)?;
    if o.code != 0 {
        return Err(anyhow!("git {} failed ({}): {}", args.join(" "), o.code, o.stderr));
    }
    Ok(o.stdout)
}

pub fn run(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    run_env(cwd, args, &[])
}

/// stdout, UTF-8 lossy, trimmed.
pub fn out(cwd: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&run(cwd, args)?).trim().to_string())
}

/// True when git exits 0, false on exit 1; anything else is an error (for `merge-base --is-ancestor`).
pub fn test(cwd: &Path, args: &[&str]) -> Result<bool> {
    let o = run_raw(cwd, args, &[], None)?;
    match o.code {
        0 => Ok(true),
        1 => Ok(false),
        c => Err(anyhow!("git {} failed ({c}): {}", args.join(" "), o.stderr)),
    }
}
