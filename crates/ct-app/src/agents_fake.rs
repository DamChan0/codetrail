//! Scripted agent services: deterministic data for smoke scenes (`--smoke`) and app tests.
//! Behaves like the real services at the trait boundary, without processes or credentials.

use crate::agents::*;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

pub struct FakeAgents {
    pub accounts: Mutex<Vec<Account>>,
    pub runtime: Mutex<RuntimeStatus>,
    pub models: Vec<ModelInfo>,
    pub runs: Arc<FakeRuns>,
    /// Text a started session streams back for any prompt.
    pub reply: String,
    pub sessions_started: Mutex<Vec<(Backend, SessionOpts)>>,
}

pub fn model(b: Backend, provider: &str, id: &str, name: &str, reasoning: bool) -> ModelInfo {
    ModelInfo { backend: b, provider: provider.into(), id: id.into(), name: name.into(), reasoning, context_window: 200_000 }
}

impl FakeAgents {
    pub fn new() -> Self {
        let acc = |id: &str, label: &str, st: AccountState, login: LoginKind| Account { id: id.into(), label: label.into(), state: st, login };
        FakeAgents {
            accounts: Mutex::new(vec![
                acc("openai-codex", "ChatGPT", AccountState::LoggedIn { method: "subscription".into() }, LoginKind::InApp),
                acc("github-copilot", "GitHub Copilot", AccountState::LoggedOut, LoginKind::InApp),
                acc("claude", "Claude", AccountState::LoggedIn { method: "claude.ai".into() }, LoginKind::ExternalCli { command: "claude auth login".into() }),
                acc("codex", "Codex CLI", AccountState::Unavailable { reason: "`codex` was not found on PATH".into() }, LoginKind::ExternalCli { command: "codex login".into() }),
            ]),
            runtime: Mutex::new(RuntimeStatus { installed: true, node: Some("22.23.3".into()), pi: Some("0.74.2".into()) }),
            models: vec![
                model(Backend::Pi, "openai-codex", "gpt-5.1-codex", "GPT-5.1 Codex", true),
                model(Backend::Pi, "openai-codex", "gpt-5.1-mini", "GPT-5.1 Mini", false),
                model(Backend::Pi, "github-copilot", "claude-sonnet-4.5", "Claude Sonnet 4.5", true),
                model(Backend::Claude, "", "opus", "Opus", true),
                model(Backend::Claude, "", "sonnet", "Sonnet", true),
                model(Backend::Codex, "", "gpt-5-codex", "GPT-5 Codex", true),
            ],
            runs: Arc::new(FakeRuns::default()),
            reply: "Because the log helper makes startup visible.\n".into(),
            sessions_started: Mutex::new(Vec::new()),
        }
    }
}

impl Default for FakeAgents {
    fn default() -> Self {
        Self::new()
    }
}

struct FakeLogin {
    rx: Mutex<Receiver<LoginEvent>>,
    tx: Sender<LoginEvent>,
    submitted: Mutex<Vec<String>>,
}

impl LoginSession for FakeLogin {
    fn next(&self, timeout: Duration) -> Option<LoginEvent> {
        self.rx.lock().recv_timeout(timeout).ok()
    }
    fn submit_code(&self, code: &str) {
        self.submitted.lock().push(code.to_string());
        let _ = self.tx.send(LoginEvent::Progress("Exchanging the code…".into()));
        let _ = self.tx.send(LoginEvent::Done);
    }
    fn cancel(&self) {
        let _ = self.tx.send(LoginEvent::Failed("cancelled".into()));
    }
}

struct FakeSession {
    rx: Receiver<AgentEvent>,
    tx: Sender<AgentEvent>,
    reply: String,
}

impl Session for FakeSession {
    fn prompt(&mut self, _text: &str) -> Result<(), String> {
        let _ = self.tx.send(AgentEvent::Started);
        for line in self.reply.split_inclusive('\n') {
            let _ = self.tx.send(AgentEvent::TextDelta(line.to_string()));
        }
        let _ = self.tx.send(AgentEvent::Settled { ok: true, error: None });
        Ok(())
    }
    fn abort(&mut self) -> Result<(), String> {
        let _ = self.tx.send(AgentEvent::Settled { ok: false, error: Some("aborted".into()) });
        Ok(())
    }
    fn next_event(&self, timeout: Duration) -> Poll {
        match self.rx.recv_timeout(timeout) {
            Ok(e) => Poll::Event(e),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Poll::Timeout,
            Err(_) => Poll::Closed,
        }
    }
    fn close(self: Box<Self>) {}
}

impl AgentService for FakeAgents {
    fn caps(&self, b: Backend) -> Caps {
        let pi = b == Backend::Pi;
        Caps { list_models: true, steer: pi, follow_up: pi, abort: true, thinking_level: pi || b == Backend::Codex, streaming_tools: pi, persistent_session: pi }
    }
    fn accounts(&self) -> Vec<Account> {
        self.accounts.lock().clone()
    }
    fn models(&self, b: Backend) -> Result<Vec<ModelInfo>, String> {
        let logged_in: Vec<String> = self.accounts.lock().iter().filter(|a| matches!(a.state, AccountState::LoggedIn { .. })).map(|a| a.id.clone()).collect();
        Ok(self.models.iter().filter(|m| m.backend == b && (b != Backend::Pi || logged_in.contains(&m.provider))).cloned().collect())
    }
    fn runtime_status(&self) -> RuntimeStatus {
        self.runtime.lock().clone()
    }
    fn runtime_setup(&self, progress: &dyn Fn(&str)) -> Result<(), String> {
        progress("Downloading node 22…");
        progress("Installing pi…");
        *self.runtime.lock() = RuntimeStatus { installed: true, node: Some("22.23.3".into()), pi: Some("0.74.2".into()) };
        Ok(())
    }
    fn login_start(&self, account_id: &str) -> Result<Arc<dyn LoginSession>, String> {
        let (tx, rx) = channel();
        let needs_code = account_id != "claude";
        let _ = tx.send(LoginEvent::OpenUrl("https://auth.example.com/device?user_code=ABCD-1234".into()));
        if needs_code {
            let _ = tx.send(LoginEvent::NeedCode { prompt: "Paste the code from the browser".into() });
        } else {
            let _ = tx.send(LoginEvent::Progress("Waiting for you to authorize in the browser…".into()));
        }
        Ok(Arc::new(FakeLogin { rx: Mutex::new(rx), tx, submitted: Mutex::new(Vec::new()) }))
    }
    fn logout(&self, account_id: &str) -> Result<(), String> {
        for a in self.accounts.lock().iter_mut() {
            if a.id == account_id {
                a.state = AccountState::LoggedOut;
            }
        }
        Ok(())
    }
    fn start_session(&self, b: Backend, o: SessionOpts) -> Result<Box<dyn Session>, String> {
        self.sessions_started.lock().push((b, o));
        let (tx, rx) = channel();
        Ok(Box::new(FakeSession { rx, tx, reply: self.reply.clone() }))
    }
    fn open_runs(&self, _max: usize) -> Result<Arc<dyn RunService>, String> {
        Ok(self.runs.clone())
    }
}

#[derive(Default)]
pub struct FakeRuns {
    pub infos: Mutex<Vec<RunInfo>>,
    pub subs: Mutex<Vec<Sender<RunUpdate>>>,
    pub log: Mutex<Vec<String>>,
}

impl FakeRuns {
    pub fn push_update(&self, u: RunUpdate) {
        if let RunUpdate::State(i) = &u {
            let mut v = self.infos.lock();
            match v.iter().position(|r| r.id == i.id) {
                Some(p) => v[p] = i.clone(),
                None => v.push(i.clone()),
            }
        }
        self.subs.lock().retain(|s| s.send(u.clone()).is_ok());
    }
}

pub fn run_info(id: &str, prompt: &str, state: RunState, started_ms: i64, ended_ms: Option<i64>, files: u32) -> RunInfo {
    RunInfo {
        id: id.into(),
        spec: RunSpec { repo: PathBuf::from("/tmp/ct-demo"), prompt: prompt.into(), model: ModelSel { backend: Backend::Pi, provider: Some("openai-codex".into()), id: "gpt-5.1-codex".into(), thinking: Some("medium".into()) }, base_ref: "main".into(), isolate: true },
        base_sha: "92de12c0000000000000000000000000000000000".into(),
        branch: Some(format!("ct/{id}")),
        worktree: None,
        state,
        started_ms,
        ended_ms,
        files_changed: files,
    }
}

impl RunService for FakeRuns {
    fn resources(&self) -> Vec<(String, crate::agents::Resource)> {
        self.infos.lock().iter().filter(|r| r.state.is_active()).map(|r| (r.id.clone(), crate::agents::Resource { rss_kb: 600 * 1024, cpu_pct: 35.0, procs: 3 })).collect()
    }
    fn submit(&self, spec: RunSpec) -> Result<String, String> {
        let id = format!("r{}", self.infos.lock().len() + 1);
        let mut i = run_info(&id, &spec.prompt, RunState::Queued, crate::agentapp::now_ms(), None, 0);
        i.spec = spec;
        self.push_update(RunUpdate::State(i));
        self.log.lock().push(format!("submit {id}"));
        Ok(id)
    }
    fn abort(&self, id: &str) -> Result<(), String> {
        let cur = self.infos.lock().iter().find(|r| r.id == id).cloned().ok_or("no such run")?;
        let mut i = cur;
        i.state = RunState::Aborted;
        i.ended_ms = Some(crate::agentapp::now_ms());
        self.push_update(RunUpdate::State(i));
        self.log.lock().push(format!("abort {id}"));
        Ok(())
    }
    fn steer(&self, id: &str, text: &str) -> Result<(), String> {
        self.log.lock().push(format!("steer {id} {text}"));
        Ok(())
    }
    fn list(&self) -> Vec<RunInfo> {
        self.infos.lock().clone()
    }
    fn subscribe(&self) -> Receiver<RunUpdate> {
        let (tx, rx) = channel();
        self.subs.lock().push(tx);
        rx
    }
    fn apply(&self, id: &str) -> Result<ApplyOutcome, String> {
        self.log.lock().push(format!("apply {id}"));
        Ok(ApplyOutcome { ok: true, message: "Merged ct/run into main.".into() })
    }
    fn discard(&self, id: &str) -> Result<(), String> {
        self.log.lock().push(format!("discard {id}"));
        Ok(())
    }
    fn comparison(&self, _id: &str) -> Result<(String, String), String> {
        Err("no result".into())
    }
    fn set_max_concurrent(&self, n: usize) {
        self.log.lock().push(format!("max {n}"));
    }
    fn shutdown(&self) {
        let ids: Vec<String> = self.infos.lock().iter().filter(|r| r.state.is_active()).map(|r| r.id.clone()).collect();
        for id in ids {
            let _ = self.abort(&id);
        }
    }
}

impl FakeAgents {
    /// Deterministic demo runs for screenshots.
    pub fn seeded() -> Self {
        let f = Self::new();
        let now = crate::agentapp::now_ms();
        {
            let mut v = f.runs.infos.lock();
            v.push(run_info("r1", "Add a --json flag to the export command and cover it with a test", RunState::Running, now - 95_000, None, 2));
            v.push(run_info("r2", "Replace the hand-rolled retry loop in sync.rs with a backoff helper", RunState::Queued, now - 20_000, None, 0));
            v.push(run_info("r3", "한글 커밋 메시지에서 깨지는 로그 파서를 고쳐줘", RunState::Succeeded, now - 900_000, Some(now - 640_000), 3));
            v.push(run_info("r4", "Upgrade serde and fix the resulting deprecation warnings", RunState::Failed("The agent exited with status 1 before finishing.".into()), now - 3_600_000, Some(now - 3_500_000), 0));
        }
        f
    }
}
