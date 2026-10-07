//! Runtime configuration: where the app-private runtime lives and which binaries to run.

use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::time::Duration;


#[derive(Debug, Clone)]
pub struct Config {
    /// Root app data dir (`~/.local/share/codetrail`, or `$CT_DATA_DIR`). The runtime lives in `<data_dir>/agent`.
    pub data_dir: PathBuf,
    /// `agent.pi_path`: run this already-installed `pi` instead of the app-private runtime.
    pub pi_path: Option<PathBuf>,
    pub claude_bin: PathBuf,
    pub codex_bin: PathBuf,
    pub npm_bin: PathBuf,
    /// Account ids switched off by `accounts.disabled` in config.toml (kill switch).
    pub accounts_disabled: Vec<String>,
    /// Staged shutdown: wait for abort to settle a running prompt.
    pub grace_abort: Duration,
    /// Staged shutdown: after closing stdin, wait for the child to exit.
    pub grace_close: Duration,
    /// Staged shutdown: after SIGTERM(group), wait before SIGKILL(group).
    pub grace_term: Duration,
    /// Timeout for one RPC request/response.
    pub rpc_timeout: Duration,
    /// Timeout for short status commands (`claude auth status`, ...).
    pub status_timeout: Duration,
}

struct Overrides {
    pi_path: Option<PathBuf>,
    accounts_disabled: Vec<String>,
}

static OVERRIDES: RwLock<Overrides> = RwLock::new(Overrides { pi_path: None, accounts_disabled: Vec::new() });

/// `agent.pi_path` from the app config. `None` = use the app-private runtime.
pub fn set_pi_path(p: Option<PathBuf>) {
    OVERRIDES.write().pi_path = p;
}

/// `accounts.disabled` from the app config.
pub fn set_disabled_accounts(ids: Vec<String>) {
    OVERRIDES.write().accounts_disabled = ids;
}

impl Config {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Config {
            data_dir: data_dir.into(),
            pi_path: None,
            claude_bin: "claude".into(),
            codex_bin: "codex".into(),
            npm_bin: "npm".into(),
            accounts_disabled: Vec::new(),
            grace_abort: Duration::from_secs(5),
            grace_close: Duration::from_secs(2),
            grace_term: Duration::from_secs(3),
            rpc_timeout: Duration::from_secs(30),
            status_timeout: Duration::from_secs(15),
        }
    }

    /// Process-wide configuration: `$CT_DATA_DIR` (else `~/.local/share/codetrail`) plus the
    /// values set through [`set_pi_path`] / [`set_disabled_accounts`].
    pub fn current() -> Self {
        let data_dir = std::env::var_os("CT_DATA_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(default_data_dir);
        let mut c = Config::new(data_dir);
        let o = OVERRIDES.read();
        c.pi_path = o.pi_path.clone();
        c.accounts_disabled = o.accounts_disabled.clone();
        c
    }

    pub fn agent_dir(&self) -> PathBuf {
        self.data_dir.join("agent")
    }
    /// `PI_CODING_AGENT_DIR`: isolates pi's `auth.json`, settings and sessions from other pi/omp installs.
    pub fn pi_home(&self) -> PathBuf {
        self.agent_dir().join("pi-home")
    }
    pub fn auth_json(&self) -> PathBuf {
        self.pi_home().join("auth.json")
    }
    pub fn node_bin(&self) -> PathBuf {
        self.agent_dir().join("node_modules/.bin/node")
    }
    pub fn pi_cli_js(&self) -> PathBuf {
        self.agent_dir().join("node_modules/@earendil-works/pi-coding-agent/dist/cli.js")
    }
    pub fn is_disabled(&self, account: &str) -> bool {
        self.accounts_disabled.iter().any(|d| d == account)
    }
}

fn default_data_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Path::new(&x).join("codetrail");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".local/share/codetrail")
}
