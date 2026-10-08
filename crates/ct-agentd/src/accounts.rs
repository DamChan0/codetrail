//! Account state, login and logout.
//!
//! * pi-backed (openai-codex, github-copilot): `auth.json` under `PI_CODING_AGENT_DIR`. State detection
//!   parses only provider keys and `type`; token values are never read into any retained structure.
//!   Login is pi-ai's own OAuth flow run by `login-helper.mjs`.
//! * claude / codex: official CLIs own their credentials; only their status commands are called.
//!   Login is external (`ExternalCli`).
//! * Anthropic OAuth through pi is intentionally not offered; API-key entry does not exist.

use crate::config::Config;
use crate::proc::{self, Spec};
use crate::util::{lossy_masked, mask_secrets, read_lf_lines, truncate};
use crate::{Account, AccountState, Error, LoginEvent, LoginHandle, LoginKind, Result};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

pub(crate) const PI_PROVIDERS: [(&str, &str); 2] = [("openai-codex", "ChatGPT (OpenAI Codex)"), ("github-copilot", "GitHub Copilot")];

pub(crate) const LOGIN_HELPER: &str = include_str!("../assets/login-helper.mjs");
const AUTH_TOOL: &str = include_str!("../assets/auth-tool.mjs");

#[derive(Deserialize)]
struct ToolStatus {
    #[serde(default)]
    providers: BTreeMap<String, ToolProvider>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct ToolProvider {
    #[serde(rename = "type")]
    ty: Option<String>,
}

/// Runs `auth-tool.mjs <args>` with the app-private node. Its stdout is token-free by construction;
/// only that output is parsed here, auth.json itself is never opened by this process.
fn auth_tool(cfg: &Config, args: &[&str]) -> std::result::Result<ToolStatus, String> {
    let node = cfg.node_bin();
    if !node.exists() {
        return Err("agent runtime is not installed; run runtime_setup first".into());
    }
    let script = cfg.agent_dir().join("auth-tool.mjs");
    if std::fs::read_to_string(&script).map_or(true, |s| s != AUTH_TOOL) {
        std::fs::write(&script, AUTH_TOOL).map_err(|_| "cannot write auth tool".to_string())?;
    }
    let mut argv: Vec<OsString> = vec![script.into_os_string()];
    argv.extend(args.iter().map(OsString::from));
    let spec = Spec { program: node, args: argv, cwd: Some(cfg.agent_dir()), env: proc::whitelist_env(&[], &[]), ceiling: None };
    let cap = proc::run_capture(spec, cfg.status_timeout).map_err(|_| "cannot run auth tool".to_string())?;
    if cap.timed_out || cap.code != Some(0) {
        return Err("auth tool failed".into());
    }
    let st: ToolStatus = serde_json::from_slice(&cap.stdout).map_err(|_| "unexpected auth tool output".to_string())?;
    match st.error {
        Some(e) => Err(mask_secrets(&truncate(&e, 200))),
        None => Ok(st),
    }
}

/// Credentials must be owner-only; tighten silently when found looser (metadata only).
fn ensure_private(path: &Path) {
    if let Ok(md) = std::fs::metadata(path) {
        if md.permissions().mode() & 0o077 != 0 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

/// provider id -> state for the pi-backed accounts, from one tool call.
fn pi_states(cfg: &Config) -> BTreeMap<&'static str, AccountState> {
    let mut m = BTreeMap::new();
    let p = cfg.auth_json();
    let detected = if !p.exists() {
        Ok(BTreeMap::new())
    } else {
        ensure_private(&p);
        auth_tool(cfg, &["status", &p.to_string_lossy()]).map(|s| s.providers)
    };
    for (id, _) in PI_PROVIDERS {
        let st = if cfg.is_disabled(id) {
            AccountState::Unavailable { reason: "disabled by config (accounts.disabled)".into() }
        } else {
            match &detected {
                Ok(map) => match map.get(id) {
                    Some(e) => AccountState::LoggedIn { method: e.ty.clone().unwrap_or_else(|| "oauth".into()) },
                    None => AccountState::LoggedOut,
                },
                Err(e) => AccountState::Unavailable { reason: e.clone() },
            }
        };
        m.insert(id, st);
    }
    m
}

fn status_spec(program: &Path, args: &[&str], backend_vars: &[&str]) -> Spec {
    Spec {
        program: program.to_path_buf(),
        args: args.iter().map(OsString::from).collect(),
        cwd: Some(std::env::temp_dir()),
        env: proc::whitelist_env(backend_vars, &[]),
        ceiling: None, // run_capture enforces its own timeout
    }
}

fn claude_state(cfg: &Config) -> AccountState {
    if cfg.is_disabled("claude") {
        return AccountState::Unavailable { reason: "disabled by config (accounts.disabled)".into() };
    }
    let cap = match proc::run_capture(status_spec(&cfg.claude_bin, &["auth", "status"], &["CLAUDE_CONFIG_DIR"]), cfg.status_timeout) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return AccountState::Unavailable { reason: "claude CLI is not installed".into() },
        Err(e) => return AccountState::Unavailable { reason: mask_secrets(&format!("cannot run claude: {e}")) },
    };
    if cap.timed_out {
        return AccountState::Unavailable { reason: "claude auth status timed out".into() };
    }
    match serde_json::from_slice::<Value>(&cap.stdout) {
        Ok(v) => {
            if v.get("loggedIn").and_then(Value::as_bool) == Some(true) {
                let m = v.get("authMethod").and_then(Value::as_str).unwrap_or("unknown");
                AccountState::LoggedIn { method: m.to_string() }
            } else {
                AccountState::LoggedOut
            }
        }
        Err(_) if cap.code == Some(0) => AccountState::Unavailable { reason: "unexpected claude auth status output".into() },
        Err(_) => AccountState::LoggedOut,
    }
}

fn codex_state(cfg: &Config) -> AccountState {
    if cfg.is_disabled("codex") {
        return AccountState::Unavailable { reason: "disabled by config (accounts.disabled)".into() };
    }
    let cap = match proc::run_capture(status_spec(&cfg.codex_bin, &["login", "status"], &["CODEX_HOME"]), cfg.status_timeout) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return AccountState::Unavailable { reason: "codex CLI is not installed".into() },
        Err(e) => return AccountState::Unavailable { reason: mask_secrets(&format!("cannot run codex: {e}")) },
    };
    if cap.timed_out {
        return AccountState::Unavailable { reason: "codex login status timed out".into() };
    }
    let text = format!("{}\n{}", String::from_utf8_lossy(&cap.stdout), cap.stderr);
    let lower = text.to_ascii_lowercase();
    if cap.code == Some(0) && lower.contains("logged in") && !lower.contains("not logged in") {
        let method = text
            .lines()
            .find_map(|l| l.to_ascii_lowercase().find("logged in using").map(|i| l[i + "logged in using".len()..].trim().to_string()))
            .filter(|m| !m.is_empty())
            .map(|m| truncate(&mask_secrets(&m), 60))
            .unwrap_or_else(|| "unknown".into());
        AccountState::LoggedIn { method }
    } else {
        AccountState::LoggedOut
    }
}

pub(crate) fn accounts(cfg: &Config) -> Vec<Account> {
    let mut states = pi_states(cfg);
    let mut v: Vec<Account> = PI_PROVIDERS
        .iter()
        .map(|(id, label)| Account { id: (*id).into(), label: (*label).into(), state: states.remove(id).unwrap_or(AccountState::LoggedOut), login: LoginKind::InApp })
        .collect();
    v.push(Account { id: "claude".into(), label: "Claude (Anthropic)".into(), state: claude_state(cfg), login: LoginKind::ExternalCli { command: "claude auth login".into() } });
    v.push(Account { id: "codex".into(), label: "Codex (OpenAI CLI)".into(), state: codex_state(cfg), login: LoginKind::ExternalCli { command: "codex login".into() } });
    v
}

// ---------------------------------------------------------------- login

pub(crate) struct LoginCtl {
    stdin: Mutex<Option<std::process::ChildStdin>>,
    host: proc::Host,
    grace_term: Duration,
}

impl LoginCtl {
    fn send(&self, v: &Value) {
        let mut line = v.to_string();
        line.push('\n');
        if let Some(w) = self.stdin.lock().as_mut() {
            let _ = w.write_all(line.as_bytes()).and_then(|_| w.flush());
        }
    }
    pub(crate) fn submit_code(&self, code: &str) {
        self.send(&serde_json::json!({ "code": code }));
    }
    pub(crate) fn cancel(&self) {
        self.send(&serde_json::json!({ "cancel": true }));
        self.stdin.lock().take();
        self.host.terminate_detached(self.grace_term);
    }
}

impl Drop for LoginCtl {
    fn drop(&mut self) {
        if self.host.exited().is_none() {
            self.host.terminate_detached(self.grace_term);
        }
    }
}

pub(crate) fn login_start(cfg: &Config, id: &str) -> Result<LoginHandle> {
    if !PI_PROVIDERS.iter().any(|(p, _)| *p == id) {
        return Err(match id {
            "claude" => Error::Other("claude login is external: run `claude auth login`".into()),
            "codex" => Error::Other("codex login is external: run `codex login`".into()),
            _ => Error::Other(format!("unknown account: {id}")),
        });
    }
    if cfg.is_disabled(id) {
        return Err(Error::Other(format!("account {id} is disabled by config (accounts.disabled)")));
    }
    let node = cfg.node_bin();
    if !node.exists() {
        return Err(Error::Other("agent runtime is not installed; run runtime_setup first".into()));
    }
    // placed in the runtime dir so `@earendil-works/pi-ai` resolves from its node_modules
    let helper = cfg.agent_dir().join("login-helper.mjs");
    std::fs::write(&helper, LOGIN_HELPER).map_err(|e| Error::Other(format!("cannot write login helper: {e}")))?;
    let home = cfg.pi_home();
    std::fs::create_dir_all(&home).map_err(|e| Error::Other(format!("cannot create {}: {e}", home.display())))?;

    let env = proc::whitelist_env(&[], &[]);
    let sp = proc::spawn(Spec {
        program: node,
        args: vec![helper.into_os_string(), id.into(), cfg.auth_json().into_os_string()],
        cwd: Some(cfg.agent_dir()),
        env,
        ceiling: Some(cfg.login_ceiling),
    })
    .map_err(|e| Error::Other(format!("cannot start login helper: {e}")))?;

    let (tx, rx) = mpsc::channel::<LoginEvent>();
    let mut out = sp.stdout;
    let t = tx.clone();
    let host = sp.host.clone();
    std::thread::Builder::new().name("ct-login-stdout".into()).spawn(move || {
        let mut terminal = false;
        read_lf_lines(
            &mut out,
            |l| {
                let Ok(v) = serde_json::from_slice::<Value>(&l) else { return };
                let text = |k: &str| mask_secrets(&truncate(v.get(k).and_then(Value::as_str).unwrap_or(""), 2000));
                let ev = match v.get("t").and_then(Value::as_str) {
                    // the URL is the one place a long opaque string is intended (OAuth authorize URL)
                    Some("url") => v.get("url").and_then(Value::as_str).map(|u| {
                        let instr = text("instructions");
                        if !instr.is_empty() {
                            let _ = t.send(LoginEvent::Progress(instr));
                        }
                        LoginEvent::OpenUrl(u.to_string())
                    }),
                    Some("need_code") => Some(LoginEvent::NeedCode { prompt: text("prompt") }),
                    Some("progress") => Some(LoginEvent::Progress(text("msg"))),
                    Some("done") => {
                        terminal = true;
                        Some(LoginEvent::Done)
                    }
                    Some("failed") => {
                        terminal = true;
                        Some(LoginEvent::Failed(text("msg")))
                    }
                    _ => None,
                };
                if let Some(e) = ev {
                    let _ = t.send(e);
                }
            },
            |_| {},
        );
        if !terminal {
            let code = host.wait_exit(Duration::from_secs(5)).flatten();
            let _ = t.send(LoginEvent::Failed(format!("login helper ended unexpectedly (exit {})", code.map_or("signal".into(), |c| c.to_string()))));
        }
    })?;
    let mut err = sp.stderr;
    std::thread::Builder::new().name("ct-login-stderr".into()).spawn(move || {
        // drained so the helper never blocks; content is not shown (may mention tokens)
        read_lf_lines(&mut err, |l| drop(lossy_masked(&l)), |_| {});
    })?;
    drop(tx);
    Ok(LoginHandle::new(rx, LoginCtl { stdin: Mutex::new(Some(sp.stdin)), host: sp.host, grace_term: cfg.grace_term }))
}

/// Removes only `id`'s entry from auth.json. Done by `auth-tool.mjs` (atomic rewrite, 0600, other
/// entries verbatim): this process never reads the file's contents.
pub(crate) fn logout(cfg: &Config, id: &str) -> Result<()> {
    if !PI_PROVIDERS.iter().any(|(p, _)| *p == id) {
        return Err(match id {
            "claude" => Error::Other("claude logout is external: run `claude auth logout`".into()),
            "codex" => Error::Other("codex logout is external: run `codex login`".into()),
            _ => Error::Other(format!("unknown account: {id}")),
        });
    }
    if !cfg.auth_json().exists() {
        return Ok(());
    }
    auth_tool(cfg, &["logout", &cfg.auth_json().to_string_lossy(), id]).map_err(Error::Other)?;
    crate::models::invalidate(cfg);
    Ok(())
}
