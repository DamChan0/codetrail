//! Adapter from the GUI traits to the real `ct-agentd` crate (PLAN §10.7). Pure type mapping;
//! no behaviour of its own. The app never reads or stores credentials here.

use crate::agents::*;
use ct_agentd as d;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

pub struct RealAgents;

fn kind(b: Backend) -> d::BackendKind {
    match b {
        Backend::Pi => d::BackendKind::Pi,
        Backend::Claude => d::BackendKind::Claude,
        Backend::Codex => d::BackendKind::Codex,
    }
}
fn back(b: d::BackendKind) -> Backend {
    match b {
        d::BackendKind::Pi => Backend::Pi,
        d::BackendKind::Claude => Backend::Claude,
        d::BackendKind::Codex => Backend::Codex,
    }
}
fn sel(m: &ModelSel) -> d::ModelSel {
    d::ModelSel { backend: kind(m.backend), provider: m.provider.clone(), id: m.id.clone(), thinking: m.thinking.clone() }
}
pub fn sel_back(m: &d::ModelSel) -> ModelSel {
    ModelSel { backend: back(m.backend), provider: m.provider.clone(), id: m.id.clone(), thinking: m.thinking.clone() }
}
pub fn event_back(e: d::AgentEvent) -> AgentEvent {
    match e {
        d::AgentEvent::Started => AgentEvent::Started,
        d::AgentEvent::TextDelta(t) => AgentEvent::TextDelta(t),
        d::AgentEvent::ToolStart { id, name, summary } => AgentEvent::ToolStart { id, name, summary },
        d::AgentEvent::ToolEnd { id, name, ok, path } => AgentEvent::ToolEnd { id, name, ok, path },
        d::AgentEvent::Usage { input, output, cost } => AgentEvent::Usage { input, output, cost },
        d::AgentEvent::Settled { ok, error } => AgentEvent::Settled { ok, error },
        d::AgentEvent::Stderr(s) => AgentEvent::Stderr(s),
        d::AgentEvent::Exited(c) => AgentEvent::Exited(c),
    }
}

struct RealLogin {
    h: Mutex<d::LoginHandle>,
}
impl LoginSession for RealLogin {
    fn next(&self, timeout: Duration) -> Option<LoginEvent> {
        let ev = self.h.lock().events.recv_timeout(timeout).ok()?;
        Some(match ev {
            d::LoginEvent::OpenUrl(u) => LoginEvent::OpenUrl(u),
            d::LoginEvent::NeedCode { prompt } => LoginEvent::NeedCode { prompt },
            d::LoginEvent::Progress(p) => LoginEvent::Progress(p),
            d::LoginEvent::Done => LoginEvent::Done,
            d::LoginEvent::Failed(e) => LoginEvent::Failed(e),
        })
    }
    fn submit_code(&self, code: &str) {
        self.h.lock().submit_code(code);
    }
    fn cancel(&self) {
        self.h.lock().cancel();
    }
}

struct RealSession(Box<dyn d::AgentSession>);
impl Session for RealSession {
    fn prompt(&mut self, text: &str) -> Result<(), String> {
        self.0.prompt(text).map_err(|e| e.to_string())
    }
    fn abort(&mut self) -> Result<(), String> {
        self.0.abort().map_err(|e| e.to_string())
    }
    fn next_event(&self, timeout: Duration) -> Poll {
        match self.0.events().recv_timeout(timeout) {
            Ok(e) => Poll::Event(event_back(e)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Poll::Timeout,
            Err(_) => Poll::Closed,
        }
    }
    fn close(self: Box<Self>) {
        self.0.close();
    }
}

impl AgentService for RealAgents {
    fn caps(&self, b: Backend) -> Caps {
        let c = d::capabilities(kind(b));
        Caps { list_models: c.list_models, steer: c.steer, follow_up: c.follow_up, abort: c.abort, thinking_level: c.thinking_level, streaming_tools: c.streaming_tools, persistent_session: c.persistent_session }
    }
    fn accounts(&self) -> Vec<Account> {
        d::accounts()
            .into_iter()
            .map(|a| Account {
                id: a.id,
                label: a.label,
                state: match a.state {
                    d::AccountState::LoggedIn { method } => AccountState::LoggedIn { method },
                    d::AccountState::LoggedOut => AccountState::LoggedOut,
                    d::AccountState::Unavailable { reason } => AccountState::Unavailable { reason },
                },
                login: match a.login {
                    d::LoginKind::InApp => LoginKind::InApp,
                    d::LoginKind::ExternalCli { command } => LoginKind::ExternalCli { command },
                    d::LoginKind::None => LoginKind::None,
                },
            })
            .collect()
    }
    fn models(&self, b: Backend) -> Result<Vec<ModelInfo>, String> {
        d::list_models(kind(b)).map_err(|e| e.to_string()).map(|v| v.into_iter().map(|m| ModelInfo { backend: back(m.backend), provider: m.provider, id: m.id, name: m.name, reasoning: m.reasoning, context_window: m.context_window }).collect())
    }
    fn runtime_status(&self) -> RuntimeStatus {
        let s = d::runtime_status();
        RuntimeStatus { installed: s.installed, node: s.node, pi: s.pi }
    }
    fn runtime_setup(&self, progress: &dyn Fn(&str)) -> Result<(), String> {
        d::runtime_setup(progress).map_err(|e| e.to_string())
    }
    fn login_start(&self, account_id: &str) -> Result<Arc<dyn LoginSession>, String> {
        let h = d::login_start(account_id).map_err(|e| e.to_string())?;
        Ok(Arc::new(RealLogin { h: Mutex::new(h) }))
    }
    fn logout(&self, account_id: &str) -> Result<(), String> {
        d::logout(account_id).map_err(|e| e.to_string())
    }
    fn start_session(&self, b: Backend, o: SessionOpts) -> Result<Box<dyn Session>, String> {
        let opts = d::SessionOpts { cwd: o.cwd, model: o.model.as_ref().map(sel), read_only: o.read_only, env_allow: o.env_allow, system_note: o.system_note };
        let inner = d::start_session(kind(b), opts).map_err(|e| e.to_string())?;
        Ok(Box::new(RealSession(inner)))
    }
    fn open_runs(&self, max_concurrent: usize) -> Result<Arc<dyn RunService>, String> {
        crate::runs_real::open(max_concurrent)
    }
}
