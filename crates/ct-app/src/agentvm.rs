//! View-models for accounts, model selection and runs (PLAN §10.3 AC8–AC10). Pure state, no
//! egui and no I/O, so the rules (gating, ordering, login steps, quit confirmation) are testable.

use crate::agents::*;
use crate::settings::AgentCfg;
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------- gating

/// Why a backend cannot be used right now (shown as the disabled segment's tooltip).
pub fn backend_gate(b: Backend, accounts: &[Account], runtime: &RuntimeStatus, caps: &Caps) -> Result<(), String> {
    if !caps.abort && !caps.list_models && !caps.streaming_tools {
        return Err(format!("{} is not available in this build.", b.label()));
    }
    let find = |id: &str| accounts.iter().find(|a| a.id == id);
    match b {
        Backend::Pi => {
            if !runtime.installed {
                return Err("The agent runtime is not installed. Install it in Settings > Accounts.".into());
            }
            let pi_ids = ["openai-codex", "github-copilot"];
            let any = pi_ids.iter().any(|id| matches!(find(id).map(|a| &a.state), Some(AccountState::LoggedIn { .. })));
            if any {
                Ok(())
            } else {
                Err("Sign in to ChatGPT or GitHub Copilot in Settings > Accounts.".into())
            }
        }
        Backend::Claude | Backend::Codex => {
            let id = if b == Backend::Claude { "claude" } else { "codex" };
            match find(id).map(|a| &a.state) {
                Some(AccountState::LoggedIn { .. }) => Ok(()),
                Some(AccountState::Unavailable { reason }) => Err(reason.clone()),
                Some(AccountState::LoggedOut) => Err(format!("{} is signed out. See Settings > Accounts.", b.label())),
                None => Err(format!("{} account status is unknown.", b.label())),
            }
        }
    }
}

/// Applies the `accounts.disabled` kill switch from config.toml.
pub fn apply_disabled(accounts: Vec<Account>, disabled: &[String]) -> Vec<Account> {
    accounts
        .into_iter()
        .map(|mut a| {
            if disabled.iter().any(|d| *d == a.id) {
                a.state = AccountState::Unavailable { reason: "Disabled in config.toml (accounts_disabled).".into() };
                a.login = LoginKind::None;
            }
            a
        })
        .collect()
}

/// The persisted selection as a `ModelSel` (None until a model is chosen).
pub fn selection(cfg: &AgentCfg) -> Option<ModelSel> {
    Some(ModelSel { backend: cfg.backend, provider: cfg.provider.clone(), id: cfg.model.clone()?, thinking: cfg.thinking.clone() })
}

/// Records a model pick; a model that cannot think drops a stale thinking level.
pub fn choose_model(cfg: &mut AgentCfg, m: &ModelInfo) {
    cfg.backend = m.backend;
    cfg.provider = (!m.provider.is_empty()).then(|| m.provider.clone());
    cfg.model = Some(m.id.clone());
    if !m.reasoning {
        cfg.thinking = None;
    }
}

pub fn selected_info<'a>(cfg: &AgentCfg, models: &'a [ModelInfo]) -> Option<&'a ModelInfo> {
    let id = cfg.model.as_deref()?;
    models.iter().find(|m| m.id == id && (cfg.provider.is_none() || cfg.provider.as_deref() == Some(m.provider.as_str())))
}

pub const THINKING_LEVELS: [&str; 5] = ["off", "low", "medium", "high", "xhigh"];

// ---------------------------------------------------------------- login state machine

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LoginPhase {
    Starting,
    Url(String),
    NeedCode(String),
    Working(String),
    Done,
    Failed(String),
    Cancelled,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LoginUi {
    pub account: String,
    pub phase: LoginPhase,
    /// URL last offered by the provider (kept so it stays copyable while a code is awaited).
    pub url: Option<String>,
    pub code_input: String,
    pub code_sent: bool,
}

impl LoginUi {
    pub fn new(account: &str) -> Self {
        LoginUi { account: account.into(), phase: LoginPhase::Starting, url: None, code_input: String::new(), code_sent: false }
    }
    pub fn finished(&self) -> bool {
        matches!(self.phase, LoginPhase::Done | LoginPhase::Failed(_) | LoginPhase::Cancelled)
    }
    /// Applies a provider event. Events after a terminal state are ignored.
    pub fn apply(&mut self, ev: LoginEvent) {
        if self.finished() {
            return;
        }
        match ev {
            LoginEvent::OpenUrl(u) => {
                self.url = Some(u.clone());
                self.phase = LoginPhase::Url(u);
            }
            LoginEvent::NeedCode { prompt } => {
                self.code_sent = false;
                self.phase = LoginPhase::NeedCode(prompt);
            }
            LoginEvent::Progress(p) => self.phase = LoginPhase::Working(p),
            LoginEvent::Done => self.phase = LoginPhase::Done,
            LoginEvent::Failed(e) => self.phase = LoginPhase::Failed(e),
        }
    }
    pub fn cancel(&mut self) {
        if !self.finished() {
            self.phase = LoginPhase::Cancelled;
        }
    }
    /// The paste field is shown only while the provider asks for a code.
    pub fn wants_code(&self) -> bool {
        matches!(self.phase, LoginPhase::NeedCode(_)) && !self.code_sent
    }
    pub fn status_text(&self) -> String {
        match &self.phase {
            LoginPhase::Starting => "Starting sign-in…".into(),
            LoginPhase::Url(_) => "Finish signing in in your browser…".into(),
            LoginPhase::NeedCode(p) => p.clone(),
            LoginPhase::Working(p) => p.clone(),
            LoginPhase::Done => "Signed in.".into(),
            LoginPhase::Failed(e) => e.clone(),
            LoginPhase::Cancelled => "Sign-in cancelled.".into(),
        }
    }
}

// ---------------------------------------------------------------- runs

pub const STREAM_CAP_CHARS: usize = 200_000;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ToolLine {
    pub id: String,
    pub name: String,
    pub summary: String,
    /// None while running.
    pub ok: Option<bool>,
    pub path: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StreamItem {
    Text(String),
    Tool(ToolLine),
    Note(String),
}

#[derive(Default, Clone, Debug)]
pub struct Stream {
    pub items: Vec<StreamItem>,
    pub chars: usize,
    pub settled: Option<(bool, Option<String>)>,
    pub usage: Option<(u64, u64, Option<f64>)>,
    pub truncated: bool,
}

impl Stream {
    pub fn push(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Started => {}
            AgentEvent::TextDelta(t) => {
                self.chars += t.chars().count();
                match self.items.last_mut() {
                    Some(StreamItem::Text(s)) => s.push_str(&t),
                    _ => self.items.push(StreamItem::Text(t)),
                }
            }
            AgentEvent::ToolStart { id, name, summary } => self.items.push(StreamItem::Tool(ToolLine { id, name, summary, ok: None, path: None })),
            AgentEvent::ToolEnd { id, name, ok, path } => {
                let slot = self.items.iter_mut().rev().find_map(|i| match i {
                    StreamItem::Tool(t) if t.id == id && t.ok.is_none() => Some(t),
                    _ => None,
                });
                match slot {
                    Some(t) => {
                        t.ok = Some(ok);
                        t.path = path;
                    }
                    None => self.items.push(StreamItem::Tool(ToolLine { id, name, summary: String::new(), ok: Some(ok), path })),
                }
            }
            AgentEvent::Usage { input, output, cost } => self.usage = Some((input, output, cost)),
            AgentEvent::Settled { ok, error } => self.settled = Some((ok, error)),
            AgentEvent::Stderr(s) => {
                let s = s.trim_end().to_string();
                if !s.is_empty() {
                    self.chars += s.chars().count();
                    self.items.push(StreamItem::Note(s));
                }
            }
            AgentEvent::Exited(code) => {
                if self.settled.is_none() {
                    self.items.push(StreamItem::Note(format!("Agent process exited{}.", code.map(|c| format!(" with status {c}")).unwrap_or_default())));
                }
            }
        }
        self.trim();
    }

    /// Drops the oldest items once the buffer is over the cap (the full log is the agent's, not ours).
    fn trim(&mut self) {
        while self.chars > STREAM_CAP_CHARS && self.items.len() > 1 {
            let gone = self.items.remove(0);
            self.truncated = true;
            if let StreamItem::Text(s) | StreamItem::Note(s) = gone {
                self.chars = self.chars.saturating_sub(s.chars().count());
            }
        }
    }
}

/// Rank for the list: running first, then queued, then everything finished.
fn rank(s: &RunState) -> u8 {
    match s {
        RunState::Running => 0,
        RunState::Queued => 1,
        _ => 2,
    }
}

pub fn first_line(prompt: &str) -> &str {
    prompt.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("(empty prompt)")
}

#[derive(Default)]
pub struct RunsVm {
    pub infos: Vec<RunInfo>,
    pub streams: HashMap<String, Stream>,
    /// Finished runs whose result the user has not opened yet.
    pub unseen: HashSet<String>,
    pub selected: Option<String>,
    loaded: bool,
}

pub struct Notice {
    pub title: String,
    pub body: String,
}

impl RunsVm {
    pub fn set_list(&mut self, list: Vec<RunInfo>) {
        self.loaded = true;
        self.infos = list;
        self.sort();
    }

    fn sort(&mut self) {
        self.infos.sort_by(|a, b| rank(&a.state).cmp(&rank(&b.state)).then_with(|| {
            if rank(&a.state) == 1 {
                a.started_ms.cmp(&b.started_ms)
            } else {
                b.started_ms.cmp(&a.started_ms)
            }
        }).then_with(|| a.id.cmp(&b.id)));
    }

    /// Applies one update from the run bridge. Returns a desktop notification when a run that
    /// was active just finished.
    pub fn apply(&mut self, u: RunUpdate) -> Option<Notice> {
        match u {
            RunUpdate::Event(id, ev) => {
                self.streams.entry(id).or_default().push(ev);
                None
            }
            RunUpdate::State(info) => {
                let prev = self.infos.iter().position(|r| r.id == info.id);
                let was_active = prev.map(|i| self.infos[i].state.is_active());
                let note = match (&info.state, was_active) {
                    (RunState::Succeeded, Some(true)) => Some(Notice { title: "Run finished".into(), body: format!("{} — {} files changed", first_line(&info.spec.prompt), info.files_changed) }),
                    (RunState::Failed(e), Some(true)) => Some(Notice { title: "Run failed".into(), body: format!("{} — {e}", first_line(&info.spec.prompt)) }),
                    _ => None,
                };
                if !info.state.is_active() && was_active == Some(true) && self.selected.as_deref() != Some(info.id.as_str()) {
                    self.unseen.insert(info.id.clone());
                }
                match prev {
                    Some(i) => self.infos[i] = info,
                    None => self.infos.push(info),
                }
                self.sort();
                note
            }
        }
    }

    pub fn select(&mut self, id: &str) {
        self.selected = Some(id.to_string());
        self.unseen.remove(id);
    }

    pub fn get(&self, id: &str) -> Option<&RunInfo> {
        self.infos.iter().find(|r| r.id == id)
    }

    pub fn running(&self) -> usize {
        self.infos.iter().filter(|r| r.state == RunState::Running).count()
    }

    /// Runs the quit dialog must warn about: running or queued.
    pub fn active(&self) -> usize {
        self.infos.iter().filter(|r| r.state.is_active()).count()
    }

    /// Badge on the Runs tab: running now + finished-unseen.
    pub fn badge(&self) -> usize {
        self.running() + self.unseen.len()
    }
}

pub fn elapsed_text(info: &RunInfo, now_ms: i64) -> String {
    let end = info.ended_ms.unwrap_or(now_ms);
    let s = ((end - info.started_ms).max(0) / 1000) as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

// ---------------------------------------------------------------- quit

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum QuitPhase {
    #[default]
    Idle,
    /// Close requested with active runs: waiting for the user's decision.
    Confirm,
    /// User confirmed: runs are being aborted; close when none is active or the deadline passes.
    Aborting,
}

pub enum QuitAction {
    /// Let the window close.
    Close,
    /// Cancel the close and show the dialog.
    Ask,
    Wait,
}

/// Close request: no active runs closes at once, otherwise ask first.
pub fn on_close_requested(phase: QuitPhase, active_runs: usize) -> QuitAction {
    match (phase, active_runs) {
        (QuitPhase::Aborting, _) => QuitAction::Wait,
        (_, 0) => QuitAction::Close,
        _ => QuitAction::Ask,
    }
}

/// While aborting: close once nothing is active, or when the wait ran out.
pub fn abort_wait_done(active_runs: usize, waited_ms: u64, limit_ms: u64) -> bool {
    active_runs == 0 || waited_ms >= limit_ms
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn caps_all() -> Caps {
        Caps { list_models: true, steer: true, follow_up: true, abort: true, thinking_level: true, streaming_tools: true, persistent_session: true }
    }
    fn acc(id: &str, st: AccountState) -> Account {
        Account { id: id.into(), label: id.into(), state: st, login: LoginKind::InApp }
    }
    fn li() -> AccountState {
        AccountState::LoggedIn { method: "subscription".into() }
    }
    fn info(id: &str, st: RunState, started: i64) -> RunInfo {
        RunInfo {
            id: id.into(),
            spec: RunSpec { repo: PathBuf::from("/r"), prompt: format!("do {id}\nmore"), model: ModelSel { backend: Backend::Pi, provider: None, id: "m".into(), thinking: None }, base_ref: "HEAD".into(), isolate: true },
            base_sha: "b".into(),
            branch: None,
            worktree: None,
            state: st,
            started_ms: started,
            ended_ms: None,
            files_changed: 2,
        }
    }

    #[test]
    fn pi_needs_runtime_then_a_pi_account() {
        let rt = RuntimeStatus { installed: false, ..Default::default() };
        let ok_rt = RuntimeStatus { installed: true, node: None, pi: None };
        let a = vec![acc("openai-codex", li())];
        assert!(backend_gate(Backend::Pi, &a, &rt, &caps_all()).unwrap_err().contains("runtime"));
        assert!(backend_gate(Backend::Pi, &[acc("openai-codex", AccountState::LoggedOut)], &ok_rt, &caps_all()).unwrap_err().contains("Sign in"));
        assert!(backend_gate(Backend::Pi, &a, &ok_rt, &caps_all()).is_ok());
        // claude accounts do not unlock Pi
        assert!(backend_gate(Backend::Pi, &[acc("claude", li())], &ok_rt, &caps_all()).is_err());
    }

    #[test]
    fn claude_and_codex_follow_their_own_account() {
        let rt = RuntimeStatus::default();
        let a = vec![acc("claude", li()), acc("codex", AccountState::Unavailable { reason: "codex not on PATH".into() })];
        assert!(backend_gate(Backend::Claude, &a, &rt, &caps_all()).is_ok());
        assert_eq!(backend_gate(Backend::Codex, &a, &rt, &caps_all()).unwrap_err(), "codex not on PATH");
        assert!(backend_gate(Backend::Claude, &[acc("claude", AccountState::LoggedOut)], &rt, &caps_all()).unwrap_err().contains("signed out"));
        assert!(backend_gate(Backend::Codex, &[], &rt, &caps_all()).unwrap_err().contains("unknown"));
    }

    #[test]
    fn kill_switch_marks_account_unavailable() {
        let out = apply_disabled(vec![acc("claude", li()), acc("codex", li())], &["claude".to_string()]);
        assert!(matches!(out[0].state, AccountState::Unavailable { .. }));
        assert_eq!(out[0].login, LoginKind::None);
        assert_eq!(out[1].state, li());
    }

    #[test]
    fn selection_roundtrips_through_config_toml() {
        let mut cfg = AgentCfg::default();
        assert!(selection(&cfg).is_none());
        let m = ModelInfo { backend: Backend::Claude, provider: String::new(), id: "opus".into(), name: "Opus".into(), reasoning: true, context_window: 200_000 };
        choose_model(&mut cfg, &m);
        cfg.thinking = Some("high".into());
        cfg.max_concurrent = 2;
        let mut s = crate::settings::Settings::default();
        s.agent = cfg.clone();
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        s.save(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("[agent]") && text.contains("backend = \"claude\""), "{text}");
        let (l, b) = crate::settings::Settings::load(&p);
        assert!(b.is_none());
        assert_eq!(l.agent, cfg);
        let sel = selection(&l.agent).unwrap();
        assert_eq!((sel.backend, sel.id.as_str(), sel.thinking.as_deref(), sel.provider), (Backend::Claude, "opus", Some("high"), None));
        // non-reasoning model drops thinking
        let plain = ModelInfo { reasoning: false, id: "haiku".into(), ..m };
        choose_model(&mut cfg, &plain);
        assert!(cfg.thinking.is_none());
    }

    #[test]
    fn concurrency_limit_is_clamped_and_old_config_still_loads() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "ask_agent = \"claude\"\n[agent]\nmax_concurrent = 99\n").unwrap();
        let (s, b) = crate::settings::Settings::load(&p);
        assert!(b.is_none());
        assert_eq!(s.agent.max_concurrent, 8);
    }

    #[test]
    fn login_flow_url_then_code_then_done() {
        let mut l = LoginUi::new("openai-codex");
        assert!(!l.wants_code());
        l.apply(LoginEvent::OpenUrl("https://x/y".into()));
        assert_eq!(l.url.as_deref(), Some("https://x/y"));
        assert!(!l.wants_code(), "paste field only on NeedCode");
        l.apply(LoginEvent::NeedCode { prompt: "Paste the code".into() });
        assert!(l.wants_code());
        assert_eq!(l.url.as_deref(), Some("https://x/y"), "url stays copyable");
        l.code_sent = true;
        assert!(!l.wants_code());
        l.apply(LoginEvent::Progress("Exchanging code".into()));
        assert_eq!(l.status_text(), "Exchanging code");
        l.apply(LoginEvent::Done);
        assert!(l.finished());
        l.apply(LoginEvent::Failed("late".into()));
        assert_eq!(l.phase, LoginPhase::Done, "terminal state is final");
    }

    #[test]
    fn login_cancel_and_failure_are_terminal() {
        let mut l = LoginUi::new("github-copilot");
        l.apply(LoginEvent::Failed("denied".into()));
        l.cancel();
        assert_eq!(l.phase, LoginPhase::Failed("denied".into()));
        let mut c = LoginUi::new("github-copilot");
        c.cancel();
        c.apply(LoginEvent::Done);
        assert_eq!(c.phase, LoginPhase::Cancelled);
    }

    #[test]
    fn run_list_orders_running_then_queued_then_finished_newest_first() {
        let mut vm = RunsVm::default();
        vm.set_list(vec![
            info("old", RunState::Succeeded, 100),
            info("q2", RunState::Queued, 300),
            info("run", RunState::Running, 50),
            info("new", RunState::Failed("x".into()), 200),
            info("q1", RunState::Queued, 250),
        ]);
        let ids: Vec<_> = vm.infos.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["run", "q1", "q2", "new", "old"]);
        // a queued run starting moves to the front group
        vm.apply(RunUpdate::State(info("q1", RunState::Running, 260)));
        let ids: Vec<_> = vm.infos.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["q1", "run", "q2", "new", "old"]);
    }

    #[test]
    fn finish_notifies_once_and_badge_counts_unseen() {
        let mut vm = RunsVm::default();
        vm.set_list(vec![info("a", RunState::Running, 1)]);
        assert_eq!(vm.badge(), 1);
        let n = vm.apply(RunUpdate::State(info("a", RunState::Succeeded, 1))).expect("notice");
        assert_eq!(n.title, "Run finished");
        assert!(n.body.starts_with("do a"), "{}", n.body);
        assert!(vm.apply(RunUpdate::State(info("a", RunState::Succeeded, 1))).is_none(), "no repeat on re-sent state");
        assert_eq!(vm.badge(), 1, "finished but unseen");
        vm.select("a");
        assert_eq!(vm.badge(), 0);
        // a run seen while it finishes is not marked unseen
        vm.apply(RunUpdate::State(info("b", RunState::Running, 2)));
        vm.select("b");
        vm.apply(RunUpdate::State(info("b", RunState::Aborted, 2)));
        assert_eq!(vm.badge(), 0);
    }

    #[test]
    fn restored_finished_runs_do_not_notify_or_badge() {
        let mut vm = RunsVm::default();
        vm.set_list(vec![info("a", RunState::Succeeded, 1)]);
        assert_eq!(vm.badge(), 0);
        assert!(vm.apply(RunUpdate::State(info("a", RunState::Succeeded, 1))).is_none());
    }

    #[test]
    fn stream_merges_text_pairs_tools_and_caps() {
        let mut s = Stream::default();
        s.push(AgentEvent::TextDelta("Hel".into()));
        s.push(AgentEvent::TextDelta("lo 한글".into()));
        s.push(AgentEvent::ToolStart { id: "1".into(), name: "edit".into(), summary: "src/a.rs".into() });
        s.push(AgentEvent::ToolEnd { id: "1".into(), name: "edit".into(), ok: true, path: Some("src/a.rs".into()) });
        s.push(AgentEvent::Settled { ok: true, error: None });
        assert_eq!(s.items.len(), 2);
        assert_eq!(s.items[0], StreamItem::Text("Hello 한글".into()));
        assert!(matches!(&s.items[1], StreamItem::Tool(t) if t.ok == Some(true) && t.path.as_deref() == Some("src/a.rs")));
        assert_eq!(s.settled, Some((true, None)));
        let mut big = Stream::default();
        for _ in 0..30 {
            big.push(AgentEvent::TextDelta("x".repeat(10_000)));
            big.push(AgentEvent::ToolStart { id: "t".into(), name: "bash".into(), summary: "ls".into() });
        }
        assert!(big.chars <= STREAM_CAP_CHARS + 10_000 && big.truncated);
    }

    #[test]
    fn quit_asks_only_with_active_runs_and_waits_while_aborting() {
        assert!(matches!(on_close_requested(QuitPhase::Idle, 0), QuitAction::Close));
        assert!(matches!(on_close_requested(QuitPhase::Idle, 2), QuitAction::Ask));
        assert!(matches!(on_close_requested(QuitPhase::Aborting, 2), QuitAction::Wait));
        assert!(abort_wait_done(0, 10, 10_000));
        assert!(!abort_wait_done(1, 9_999, 10_000));
        assert!(abort_wait_done(1, 10_000, 10_000));
    }

    #[test]
    fn elapsed_formats() {
        let mut r = info("a", RunState::Running, 0);
        assert_eq!(elapsed_text(&r, 42_000), "42s");
        assert_eq!(elapsed_text(&r, 125_000), "2m 05s");
        r.ended_ms = Some(3_900_000);
        assert_eq!(elapsed_text(&r, 99_999_999), "1h 05m");
        assert_eq!(first_line("\n  hi there \nx"), "hi there");
    }
}
