//! Seam between the GUI and the agent crates (PLAN §10.7). The GUI only sees these types and
//! traits; `agents_real` adapts `ct-agentd` / `ct-runs`, `agents_fake` scripts them for tests and
//! smoke scenes. Nothing here blocks: every call is made from a worker job or a bridge thread.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Pi,
    Claude,
    Codex,
}

impl Backend {
    pub const ALL: [Backend; 3] = [Backend::Pi, Backend::Claude, Backend::Codex];
    pub fn label(self) -> &'static str {
        match self {
            Backend::Pi => "Pi",
            Backend::Claude => "Claude",
            Backend::Codex => "Codex",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Caps {
    pub list_models: bool,
    pub steer: bool,
    pub follow_up: bool,
    pub abort: bool,
    pub thinking_level: bool,
    pub streaming_tools: bool,
    pub persistent_session: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModelInfo {
    pub backend: Backend,
    pub provider: String,
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    pub context_window: u32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModelSel {
    pub backend: Backend,
    pub provider: Option<String>,
    pub id: String,
    pub thinking: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AccountState {
    LoggedIn { method: String },
    LoggedOut,
    Unavailable { reason: String },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LoginKind {
    InApp,
    ExternalCli { command: String },
    None,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Account {
    pub id: String,
    pub label: String,
    pub state: AccountState,
    pub login: LoginKind,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LoginEvent {
    OpenUrl(String),
    NeedCode { prompt: String },
    Progress(String),
    Done,
    Failed(String),
}

/// One in-flight login. Shared between the UI (submit/cancel) and the worker pumping `next`.
pub trait LoginSession: Send + Sync {
    fn next(&self, timeout: Duration) -> Option<LoginEvent>;
    fn submit_code(&self, code: &str);
    fn cancel(&self);
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RuntimeStatus {
    pub installed: bool,
    pub node: Option<String>,
    pub pi: Option<String>,
}

#[derive(Clone, PartialEq, Debug)]
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

#[derive(Clone, Debug)]
pub struct SessionOpts {
    pub cwd: PathBuf,
    pub model: Option<ModelSel>,
    pub read_only: bool,
    pub env_allow: Vec<String>,
    pub system_note: Option<String>,
}

pub enum Poll {
    Event(AgentEvent),
    Timeout,
    Closed,
}

/// One agent process. Events are pulled with `next_event`; `close` ends the process.
pub trait Session: Send {
    fn prompt(&mut self, text: &str) -> Result<(), String>;
    fn abort(&mut self) -> Result<(), String>;
    fn next_event(&self, timeout: Duration) -> Poll;
    fn close(self: Box<Self>);
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RunSpec {
    pub repo: PathBuf,
    pub prompt: String,
    pub model: ModelSel,
    pub base_ref: String,
    pub isolate: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RunState {
    Queued,
    Running,
    Succeeded,
    Failed(String),
    Aborted,
    Interrupted,
}

impl RunState {
    pub fn is_active(&self) -> bool {
        matches!(self, RunState::Queued | RunState::Running)
    }
    pub fn label(&self) -> &'static str {
        match self {
            RunState::Queued => "Queued",
            RunState::Running => "Running",
            RunState::Succeeded => "Done",
            RunState::Failed(_) => "Failed",
            RunState::Aborted => "Aborted",
            RunState::Interrupted => "Interrupted",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RunInfo {
    pub id: String,
    pub spec: RunSpec,
    pub base_sha: String,
    pub branch: Option<String>,
    pub worktree: Option<PathBuf>,
    pub state: RunState,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub files_changed: u32,
}

#[derive(Clone, Debug)]
pub enum RunUpdate {
    State(RunInfo),
    Event(String, AgentEvent),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ApplyOutcome {
    pub ok: bool,
    pub message: String,
}

pub trait RunService: Send + Sync {
    fn submit(&self, spec: RunSpec) -> Result<String, String>;
    fn abort(&self, id: &str) -> Result<(), String>;
    fn steer(&self, id: &str, text: &str) -> Result<(), String>;
    fn list(&self) -> Vec<RunInfo>;
    fn subscribe(&self) -> Receiver<RunUpdate>;
    fn apply(&self, id: &str) -> Result<ApplyOutcome, String>;
    fn discard(&self, id: &str) -> Result<(), String>;
    /// (base_sha, head_sha) of the run's result.
    fn comparison(&self, id: &str) -> Result<(String, String), String>;
    fn set_max_concurrent(&self, n: usize);
    /// App quit: abort every active run (staged kill) and wait for them; queued runs stay queued.
    fn shutdown(&self);
}

pub trait AgentService: Send + Sync {
    fn caps(&self, b: Backend) -> Caps;
    fn accounts(&self) -> Vec<Account>;
    fn models(&self, b: Backend) -> Result<Vec<ModelInfo>, String>;
    fn runtime_status(&self) -> RuntimeStatus;
    fn runtime_setup(&self, progress: &dyn Fn(&str)) -> Result<(), String>;
    fn login_start(&self, account_id: &str) -> Result<Arc<dyn LoginSession>, String>;
    fn logout(&self, account_id: &str) -> Result<(), String>;
    fn start_session(&self, b: Backend, o: SessionOpts) -> Result<Box<dyn Session>, String>;
    fn open_runs(&self, max_concurrent: usize) -> Result<Arc<dyn RunService>, String>;
}
