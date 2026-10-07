//! Agent backends (pi / claude / codex), app-private runtime, accounts and login.

mod accounts;
mod config;
mod models;
mod oneshot;
mod pi;
mod proc;
mod runtime;
mod util;

pub use config::{set_disabled_accounts, set_pi_path, Config};
pub use util::mask_secrets;

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported by this backend: {0}")]
    Unsupported(&'static str),
    #[error("{0}")]
    Other(String),
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Other(crate::util::mask_secrets(&e.to_string()))
    }
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    Pi,
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub list_models: bool,
    pub steer: bool,
    pub follow_up: bool,
    pub abort: bool,
    pub thinking_level: bool,
    pub streaming_tools: bool,
    pub persistent_session: bool,
}
pub fn capabilities(b: BackendKind) -> Capabilities {
    match b {
        BackendKind::Pi => Capabilities {
            list_models: true,
            steer: true,
            follow_up: true,
            abort: true,
            thinking_level: true,
            streaming_tools: true,
            persistent_session: true,
        },
        // one CLI process per prompt: no mid-run input, no state between prompts
        BackendKind::Claude | BackendKind::Codex => Capabilities {
            list_models: true,
            steer: false,
            follow_up: false,
            abort: true,
            thinking_level: true,
            streaming_tools: true,
            persistent_session: false,
        },
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub backend: BackendKind,
    pub provider: String,
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    pub context_window: u32,
}
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSel {
    pub backend: BackendKind,
    pub provider: Option<String>,
    pub id: String,
    pub thinking: Option<String>,
}
pub fn list_models(b: BackendKind) -> Result<Vec<ModelInfo>> {
    models::list_models(&Config::current(), b)
}

#[derive(Debug, Clone)]
pub struct SessionOpts {
    pub cwd: PathBuf,
    pub model: Option<ModelSel>,
    pub read_only: bool,
    pub env_allow: Vec<String>,
    pub system_note: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    Started,
    TextDelta(String),
    ToolStart { id: String, name: String, summary: String },
    ToolEnd { id: String, name: String, ok: bool, path: Option<String> },
    Usage { input: u64, output: u64, cost: Option<f64> },
    Settled { ok: bool, error: Option<String> },
    Stderr(String),
    Exited(Option<i32>),
}

pub trait AgentSession: Send {
    fn pid(&self) -> Option<u32>;
    fn prompt(&mut self, text: &str) -> Result<()>;
    fn steer(&mut self, text: &str) -> Result<()>;
    fn follow_up(&mut self, text: &str) -> Result<()>;
    fn abort(&mut self) -> Result<()>;
    fn set_model(&mut self, m: &ModelSel) -> Result<()>;
    fn events(&self) -> &Receiver<AgentEvent>;
    fn close(self: Box<Self>);
}
pub fn start_session(b: BackendKind, o: SessionOpts) -> Result<Box<dyn AgentSession>> {
    with::start_session(&Config::current(), b, o)
}

#[derive(Debug, Clone, PartialEq)]
pub enum AccountState {
    LoggedIn { method: String },
    LoggedOut,
    Unavailable { reason: String },
}
#[derive(Debug, Clone, PartialEq)]
pub enum LoginKind {
    InApp,
    ExternalCli { command: String },
    None,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub id: String,
    pub label: String,
    pub state: AccountState,
    pub login: LoginKind,
}
pub fn accounts() -> Vec<Account> {
    accounts::accounts(&Config::current())
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoginEvent {
    OpenUrl(String),
    NeedCode { prompt: String },
    Progress(String),
    Done,
    Failed(String),
}
pub struct LoginHandle {
    pub events: Receiver<LoginEvent>,
    ctl: accounts::LoginCtl,
}
impl LoginHandle {
    pub(crate) fn new(events: Receiver<LoginEvent>, ctl: accounts::LoginCtl) -> Self {
        LoginHandle { events, ctl }
    }
    /// Answers a `NeedCode` prompt (pasted redirect URL / code).
    pub fn submit_code(&self, code: &str) {
        self.ctl.submit_code(code);
    }
    /// Aborts the login; the helper process is terminated.
    pub fn cancel(&self) {
        self.ctl.cancel();
    }
}
pub fn login_start(account_id: &str) -> Result<LoginHandle> {
    accounts::login_start(&Config::current(), account_id)
}
pub fn logout(account_id: &str) -> Result<()> {
    accounts::logout(&Config::current(), account_id)
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeStatus {
    pub installed: bool,
    pub node: Option<String>,
    pub pi: Option<String>,
}
pub fn runtime_status() -> RuntimeStatus {
    runtime::status(&Config::current())
}
pub fn runtime_setup(progress: &dyn Fn(&str)) -> Result<()> {
    runtime::setup(&Config::current(), progress)
}

/// Explicit-config variants of the API above (tests, embedding). The plain functions use
/// [`Config::current`].
pub mod with {
    use super::*;
    pub fn list_models(c: &Config, b: BackendKind) -> Result<Vec<ModelInfo>> {
        models::list_models(c, b)
    }
    pub fn start_session(c: &Config, b: BackendKind, o: SessionOpts) -> Result<Box<dyn AgentSession>> {
        match b {
            BackendKind::Pi => pi::start(c, o),
            BackendKind::Claude | BackendKind::Codex => oneshot::start(c, b, o),
        }
    }
    pub fn accounts(c: &Config) -> Vec<Account> {
        accounts::accounts(c)
    }
    pub fn login_start(c: &Config, id: &str) -> Result<LoginHandle> {
        accounts::login_start(c, id)
    }
    pub fn logout(c: &Config, id: &str) -> Result<()> {
        accounts::logout(c, id)
    }
    pub fn runtime_status(c: &Config) -> RuntimeStatus {
        runtime::status(c)
    }
    pub fn runtime_setup(c: &Config, progress: &dyn Fn(&str)) -> Result<()> {
        runtime::setup(c, progress)
    }
}
