//! Agent state and actions on `App`: accounts, models, login, runtime setup, runs, Ask AI routing.
//! Every call into the agent crates runs on a worker job (PLAN §9.2); results come back as `Msg`.

use crate::agents::*;
use crate::agentvm::*;
use crate::app::{App, AskPhase, Centre, Loadable, Msg};
use crate::jobs::JobKind;
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct NewRun {
    pub prompt: String,
    pub base_ref: String,
    pub isolate: bool,
    pub focus: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Confirm {
    Apply(String),
    Discard(String),
}

pub struct AgentState {
    pub svc: Arc<dyn AgentService>,
    pub accounts: Loadable<Vec<Account>>,
    pub runtime: RuntimeStatus,
    /// Some while `runtime_setup` runs.
    pub runtime_progress: Option<String>,
    pub runtime_result: Option<Result<(), String>>,
    pub models: Loadable<Vec<ModelInfo>>,
    pub models_for: Backend,
    pub model_cache: HashMap<Backend, Vec<ModelInfo>>,
    pub login: Option<(LoginUi, Option<Arc<dyn LoginSession>>)>,
    /// Account whose "How to sign in" block is expanded.
    pub how_open: Option<String>,
    pub account_error: Option<String>,
    pub runs_svc: Loadable<Arc<dyn RunService>>,
    pub runs: RunsVm,
    pub new_run: Option<NewRun>,
    pub confirm: Option<Confirm>,
    pub run_banner: Option<(crate::widgets::Level, String)>,
    pub steer_input: String,
    pub quit: QuitPhase,
    pub quit_since: Option<Instant>,
    pub allow_close: bool,
    pub caps: HashMap<Backend, Caps>,
    pub side_tx: Sender<Msg>,
    pub side_rx: Receiver<Msg>,
}

impl AgentState {
    pub fn new(svc: Arc<dyn AgentService>) -> Self {
        let (side_tx, side_rx) = channel();
        let caps = Backend::ALL.iter().map(|b| (*b, svc.caps(*b))).collect();
        AgentState {
            svc,
            accounts: Loadable::Idle,
            runtime: RuntimeStatus::default(),
            runtime_progress: None,
            runtime_result: None,
            models: Loadable::Idle,
            models_for: Backend::Pi,
            model_cache: HashMap::new(),
            login: None,
            how_open: None,
            account_error: None,
            runs_svc: Loadable::Idle,
            runs: RunsVm::default(),
            new_run: None,
            confirm: None,
            run_banner: None,
            steer_input: String::new(),
            quit: QuitPhase::Idle,
            quit_since: None,
            allow_close: false,
            caps,
            side_tx,
            side_rx,
        }
    }

    pub fn caps(&self, b: Backend) -> Caps {
        self.caps[&b]
    }

    /// `Ok` when `b` can be used now, else the tooltip reason. Unknown (not loaded yet) is "Checking…".
    pub fn gate(&self, b: Backend) -> Result<(), String> {
        match &self.accounts {
            Loadable::Ready(a) => backend_gate(b, a, &self.runtime, &self.caps(b)),
            Loadable::Failed(e) => Err(e.clone()),
            _ => Err("Checking accounts…".into()),
        }
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Opens a URL in the user's browser (best effort; the URL is always shown and copyable too).
fn open_url(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    let url = url.to_string();
    let _ = std::thread::Builder::new().name("ct-open-url".into()).spawn(move || {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let _ = std::process::Command::new(opener).arg(&url).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    });
}

/// Desktop notification through `notify-send` when present; silently nothing otherwise.
fn notify(title: &str, body: &str) {
    let (t, b) = (title.to_string(), body.to_string());
    let _ = std::thread::Builder::new().name("ct-notify".into()).spawn(move || {
        let _ = std::process::Command::new("notify-send").args(["-a", "codetrail", "--", &t, &b]).stdin(std::process::Stdio::null()).status();
    });
}

impl App {
    // ------------------------------------------------------------ accounts / runtime

    /// Loads accounts + runtime status once (lazily: reading them may spawn short CLI probes).
    pub fn ag_ensure_accounts(&mut self) {
        if matches!(self.ag.accounts, Loadable::Idle) {
            self.ag_refresh_accounts();
        }
    }

    pub fn ag_refresh_accounts(&mut self) {
        self.ag.accounts = Loadable::Loading;
        let svc = self.ag.svc.clone();
        self.jobs.spawn(JobKind::Accounts, move |c| {
            let accounts = svc.accounts();
            let runtime = svc.runtime_status();
            c.finish(Msg::Accounts { accounts, runtime });
        });
    }

    pub fn ag_runtime_setup(&mut self) {
        if self.ag.runtime_progress.is_some() {
            return;
        }
        self.ag.runtime_progress = Some("Starting…".into());
        self.ag.runtime_result = None;
        let svc = self.ag.svc.clone();
        self.jobs.spawn(JobKind::Runtime, move |c| {
            let res = svc.runtime_setup(&|p| {
                c.stream(Msg::RuntimeProgress(p.to_string()));
            });
            c.finish_forced(Msg::RuntimeDone(res));
        });
    }

    pub fn ag_login(&mut self, account: &str) {
        self.ag.login = Some((LoginUi::new(account), None));
        let (svc, id) = (self.ag.svc.clone(), account.to_string());
        self.jobs.spawn(JobKind::Login, move |c| {
            let session = match svc.login_start(&id) {
                Ok(s) => s,
                Err(e) => return c.finish_forced(Msg::LoginEv(LoginEvent::Failed(e))),
            };
            c.stream(Msg::LoginSession(session.clone()));
            loop {
                if c.cancelled() {
                    return;
                }
                let Some(ev) = session.next(Duration::from_millis(200)) else { continue };
                let last = matches!(ev, LoginEvent::Done | LoginEvent::Failed(_));
                if last {
                    return c.finish_forced(Msg::LoginEv(ev));
                }
                if !c.stream(Msg::LoginEv(ev)) {
                    return;
                }
            }
        });
    }

    pub fn ag_login_submit(&mut self) {
        let Some((ui, Some(s))) = &mut self.ag.login else { return };
        let code = ui.code_input.trim().to_string();
        if code.is_empty() {
            return;
        }
        s.submit_code(&code);
        ui.code_sent = true;
        ui.code_input.clear();
        ui.phase = LoginPhase::Working("Checking the code…".into());
    }

    pub fn ag_login_cancel(&mut self) {
        if let Some((ui, s)) = &mut self.ag.login {
            if let Some(s) = s {
                s.cancel();
            }
            ui.cancel();
        }
        self.jobs.cancel(JobKind::Login);
    }

    pub fn ag_logout(&mut self, account: &str) {
        let (svc, id) = (self.ag.svc.clone(), account.to_string());
        self.jobs.spawn(JobKind::Logout, move |c| {
            let res = svc.logout(&id);
            c.finish(Msg::LoggedOut { id, res });
        });
    }

    // ------------------------------------------------------------ models

    /// Loads the model list for the selected backend (cached per backend for this session).
    pub fn ag_load_models(&mut self, b: Backend) {
        self.ag.models_for = b;
        if let Some(m) = self.ag.model_cache.get(&b) {
            self.ag.models = Loadable::Ready(m.clone());
            return;
        }
        if self.ag.gate(b).is_err() {
            self.ag.models = Loadable::Ready(Vec::new());
            return;
        }
        self.ag.models = Loadable::Loading;
        let svc = self.ag.svc.clone();
        self.jobs.spawn(JobKind::Models, move |c| {
            c.finish(Msg::Models { backend: b, res: svc.models(b) });
        });
    }

    /// Re-fetch after sign-in/out: the cache is stale then.
    pub fn ag_invalidate_models(&mut self) {
        self.ag.model_cache.clear();
        self.ag.models = Loadable::Idle;
    }

    /// Drives lazy loading from the pickers: accounts first, then the models of the selection.
    pub fn ag_ensure_models(&mut self) {
        self.ag_ensure_accounts();
        if matches!(self.ag.models, Loadable::Idle) && self.ag.accounts.ready().is_some() {
            self.ag_load_models(self.settings.agent.backend);
        }
    }

    pub fn ag_set_backend(&mut self, b: Backend) {
        if self.settings.agent.backend == b {
            return;
        }
        self.settings.agent.backend = b;
        self.settings.agent.provider = None;
        self.settings.agent.model = None;
        self.settings.agent.thinking = None;
        self.settings_dirty_at = Some(Instant::now());
        self.ag_load_models(b);
    }

    pub fn ag_pick_model(&mut self, m: &ModelInfo) {
        choose_model(&mut self.settings.agent, m);
        self.settings_dirty_at = Some(Instant::now());
    }

    // ------------------------------------------------------------ runs

    pub fn ag_open_runs(&mut self) {
        if !matches!(self.ag.runs_svc, Loadable::Idle) {
            return;
        }
        self.ag.runs_svc = Loadable::Loading;
        let svc = self.ag.svc.clone();
        let max = self.settings.agent.max_concurrent;
        self.jobs.spawn(JobKind::RunsOpen, move |c| {
            let res = svc.open_runs(max).map(|r| {
                let list = r.list();
                (r, list)
            });
            c.finish(Msg::RunsOpened(res));
        });
    }

    fn runs_bridge(&self, r: Arc<dyn RunService>) {
        let (tx, ctx) = (self.ag.side_tx.clone(), self.ctx.clone());
        let rx = r.subscribe();
        let _ = std::thread::Builder::new().name("ct-run-bridge".into()).spawn(move || {
            for u in rx {
                if tx.send(Msg::RunUpd(u)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        });
    }

    pub fn ag_new_run_dialog(&mut self) {
        let base = if self.head_name.is_empty() || self.head_name == "(detached)" { "HEAD".to_string() } else { self.head_name.clone() };
        self.ag.new_run = Some(NewRun { prompt: String::new(), base_ref: base, isolate: true, focus: true });
        self.ag_ensure_models();
    }

    pub fn ag_submit_run(&mut self) {
        let (Some(repo), Some(nr), Some(sel), Loadable::Ready(rs)) = (self.repo.as_ref(), self.ag.new_run.as_ref(), selection(&self.settings.agent), &self.ag.runs_svc) else { return };
        let spec = RunSpec { repo: repo.root.clone(), prompt: nr.prompt.trim().to_string(), model: sel, base_ref: nr.base_ref.trim().to_string(), isolate: nr.isolate };
        let rs = rs.clone();
        self.ag.new_run = None;
        self.jobs.spawn(JobKind::RunSubmit, move |c| {
            c.finish(Msg::RunSubmitted(rs.submit(spec)));
        });
    }

    fn run_svc(&self) -> Option<Arc<dyn RunService>> {
        self.ag.runs_svc.ready().cloned()
    }

    pub fn ag_abort(&mut self, id: &str) {
        let Some(rs) = self.run_svc() else { return };
        let id = id.to_string();
        self.jobs.spawn(JobKind::RunControl, move |c| {
            c.finish(Msg::RunControl { what: "Abort", res: rs.abort(&id) });
        });
    }

    /// Quit path: staged kill of every active run, off the UI thread.
    pub fn ag_shutdown_runs(&mut self) {
        let Some(rs) = self.run_svc() else { return };
        self.jobs.spawn(JobKind::RunControl, move |c| {
            rs.shutdown();
            c.finish_forced(Msg::RunControl { what: "Shutdown", res: Ok(()) });
        });
    }

    pub fn ag_set_max_concurrent(&mut self, n: usize) {
        self.settings.agent.max_concurrent = n.clamp(1, 8);
        self.settings_dirty_at = Some(Instant::now());
        if let Some(rs) = self.run_svc() {
            rs.set_max_concurrent(self.settings.agent.max_concurrent);
        }
    }

    pub fn ag_steer(&mut self, id: &str, text: &str) {
        let Some(rs) = self.run_svc() else { return };
        let (id, text) = (id.to_string(), text.to_string());
        self.jobs.spawn(JobKind::RunControl, move |c| {
            c.finish(Msg::RunControl { what: "Send", res: rs.steer(&id, &text) });
        });
    }

    pub fn ag_apply(&mut self, id: &str) {
        let Some(rs) = self.run_svc() else { return };
        let id = id.to_string();
        self.jobs.spawn(JobKind::RunFinish, move |c| {
            let res = rs.apply(&id).and_then(|o| if o.ok { Ok(o.message) } else { Err(o.message) });
            c.finish(Msg::RunFinished { what: "Apply", res });
        });
    }

    pub fn ag_discard(&mut self, id: &str) {
        let Some(rs) = self.run_svc() else { return };
        let id = id.to_string();
        self.jobs.spawn(JobKind::RunFinish, move |c| {
            c.finish(Msg::RunFinished { what: "Discard", res: rs.discard(&id).map(|_| "Run discarded.".to_string()) });
        });
    }

    /// Opens the run's result (`base_sha..head`) in the regular diff view.
    pub fn ag_review(&mut self, id: &str) {
        let Some(rs) = self.run_svc() else { return };
        let id = id.to_string();
        self.jobs.spawn(JobKind::RunReview, move |c| {
            c.finish(Msg::RunReview { res: rs.comparison(&id) });
        });
    }

    pub fn ag_select_run(&mut self, id: &str) {
        self.ag.runs.select(id);
        self.ag.run_banner = None;
        self.centre = Centre::Run;
    }

    // ------------------------------------------------------------ quit

    pub fn ag_close_requested(&mut self, ctx: &egui::Context) {
        if self.ag.allow_close || !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        match on_close_requested(self.ag.quit, self.ag.runs.active()) {
            QuitAction::Close => {}
            QuitAction::Ask => {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.ag.quit = QuitPhase::Confirm;
            }
            QuitAction::Wait => ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose),
        }
    }

    pub fn ag_quit_tick(&mut self, ctx: &egui::Context) {
        if self.ag.quit != QuitPhase::Aborting {
            return;
        }
        let waited = self.ag.quit_since.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
        if abort_wait_done(self.ag.runs.active(), waited, 12_000) {
            self.ag.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    pub fn ag_confirm_quit(&mut self) {
        self.ag.quit = QuitPhase::Aborting;
        self.ag.quit_since = Some(Instant::now());
        self.ag_shutdown_runs();
    }

    // ------------------------------------------------------------ message handling

    pub fn ag_handle(&mut self, msg: Msg) {
        match msg {
            Msg::Accounts { accounts, runtime } => {
                self.ag.accounts = Loadable::Ready(apply_disabled(accounts, &self.settings.accounts_disabled));
                self.ag.runtime = runtime;
                if matches!(self.ag.models, Loadable::Idle) {
                    self.ag_load_models(self.settings.agent.backend);
                }
            }
            Msg::RuntimeProgress(p) => self.ag.runtime_progress = Some(p),
            Msg::RuntimeDone(res) => {
                self.ag.runtime_progress = None;
                self.ag.runtime_result = Some(res);
                self.ag_invalidate_models();
                self.ag_refresh_accounts();
            }
            Msg::Models { backend, res } => {
                if backend != self.ag.models_for {
                    return;
                }
                self.ag.models = match res {
                    Ok(m) => {
                        self.ag.model_cache.insert(backend, m.clone());
                        Loadable::Ready(m)
                    }
                    Err(e) => Loadable::Failed(e),
                };
            }
            Msg::LoginSession(s) => {
                if let Some((_, slot)) = &mut self.ag.login {
                    *slot = Some(s);
                }
            }
            Msg::LoginEv(ev) => {
                let Some((ui, _)) = &mut self.ag.login else { return };
                if let LoginEvent::OpenUrl(u) = &ev {
                    open_url(u);
                }
                let done = ev == LoginEvent::Done;
                ui.apply(ev);
                if done {
                    self.ag_invalidate_models();
                    self.ag_refresh_accounts();
                }
            }
            Msg::LoggedOut { id, res } => {
                self.ag.account_error = res.err().map(|e| format!("Sign-out of {id} failed: {e}"));
                self.ag_invalidate_models();
                self.ag_refresh_accounts();
            }
            Msg::RunsOpened(res) => match res {
                Ok((rs, list)) => {
                    self.ag.runs.set_list(list);
                    self.runs_bridge(rs.clone());
                    self.ag.runs_svc = Loadable::Ready(rs);
                }
                Err(e) => self.ag.runs_svc = Loadable::Failed(e),
            },
            Msg::RunUpd(u) => {
                if let Some(n) = self.ag.runs.apply(u) {
                    notify(&n.title, &n.body);
                    self.flash(format!("{}: {}", n.title, n.body));
                }
            }
            Msg::RunSubmitted(res) => match res {
                Ok(id) => self.ag_select_run(&id),
                Err(e) => self.ag.run_banner = Some((crate::widgets::Level::Error, format!("Could not start the run: {e}"))),
            },
            Msg::RunControl { what, res } => {
                if let Err(e) = res {
                    self.ag.run_banner = Some((crate::widgets::Level::Error, format!("{what} failed: {e}")));
                }
            }
            Msg::RunFinished { what, res } => {
                self.ag.run_banner = Some(match res {
                    Ok(m) => (crate::widgets::Level::Info, m),
                    Err(e) => (crate::widgets::Level::Error, format!("{what} failed: {e}")),
                });
                if what == "Apply" {
                    self.request_status();
                    self.request_log(false);
                }
            }
            Msg::RunReview { res } => match res {
                Ok((base, head)) => {
                    if self.target.is_none() {
                        self.target = Some(crate::app::TargetSel::Worktree);
                    }
                    self.centre = Centre::Diff;
                    self.insp_open = true;
                    self.insp = crate::app::InspTab::Why;
                    self.set_range(base, head);
                }
                Err(e) => self.ag.run_banner = Some((crate::widgets::Level::Error, format!("Cannot open the result: {e}"))),
            },
            _ => {}
        }
    }

    // ------------------------------------------------------------ Ask AI routing

    /// Backend + model the Ask flow will use, or the reason it cannot start.
    pub fn ag_ask_target(&self) -> Result<ModelSel, String> {
        let b = self.settings.agent.backend;
        self.ag.gate(b)?;
        selection(&self.settings.agent).ok_or_else(|| "Choose a model first (Ask AI tab).".to_string())
    }

    pub fn ag_ask_phase_failed(&mut self, why: String) {
        self.ask.phase = AskPhase::Failed(why);
    }
}

/// Runs one read-only question through a session, streaming text to `on_chunk`.
pub fn run_ask_session(svc: &dyn AgentService, backend: Backend, opts: SessionOpts, text: &str, cancel: &std::sync::atomic::AtomicBool, timeout: Duration, on_chunk: &mut dyn FnMut(&str)) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    let mut s = svc.start_session(backend, opts).map_err(|e| format!("Could not start {}: {e}", backend.label()))?;
    let started = Instant::now();
    if let Err(e) = s.prompt(text) {
        s.close();
        return Err(format!("The agent rejected the prompt: {e}"));
    }
    let mut stderr_tail = String::new();
    let res = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = s.abort();
            break Err("Cancelled.".to_string());
        }
        if started.elapsed() >= timeout {
            let _ = s.abort();
            break Err(format!("No answer within {}s: the agent was stopped. Raise the timeout in Settings, or retry.", timeout.as_secs()));
        }
        match s.next_event(Duration::from_millis(100)) {
            Poll::Event(AgentEvent::TextDelta(t)) => on_chunk(&t),
            Poll::Event(AgentEvent::Settled { ok: true, .. }) => break Ok(()),
            Poll::Event(AgentEvent::Settled { ok: false, error }) => break Err(error.unwrap_or_else(|| "The agent reported a failure.".into())),
            Poll::Event(AgentEvent::Stderr(e)) => {
                stderr_tail.push_str(e.trim_end());
                stderr_tail.push('\n');
                if stderr_tail.len() > 600 {
                    let cut = stderr_tail.len() - 600;
                    let cut = (cut..stderr_tail.len()).find(|i| stderr_tail.is_char_boundary(*i)).unwrap_or(0);
                    stderr_tail.drain(..cut);
                }
            }
            Poll::Event(AgentEvent::Exited(code)) => {
                let tail = stderr_tail.trim();
                break Err(format!("The agent exited before answering{}.{}", code.map(|c| format!(" (status {c})")).unwrap_or_default(), if tail.is_empty() { String::new() } else { format!(" {tail}") }));
            }
            Poll::Event(_) | Poll::Timeout => {}
            Poll::Closed => break Err("The agent connection closed unexpectedly.".to_string()),
        }
    };
    s.close();
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents_fake::FakeAgents;
    use std::sync::atomic::AtomicBool;

    fn opts() -> SessionOpts {
        SessionOpts { cwd: std::env::temp_dir(), model: None, read_only: true, env_allow: Vec::new(), system_note: None }
    }

    #[test]
    fn ask_streams_text_through_a_read_only_session() {
        let f = FakeAgents::new();
        let mut got = String::new();
        run_ask_session(&f, Backend::Claude, opts(), "why?", &AtomicBool::new(false), Duration::from_secs(5), &mut |t| got.push_str(t)).unwrap();
        assert_eq!(got, f.reply);
        let started = f.sessions_started.lock();
        assert_eq!(started.len(), 1);
        assert!(started[0].1.read_only);
    }

    #[test]
    fn ask_cancel_and_timeout_are_reported() {
        let f = FakeAgents::new();
        // Fake session settles on prompt, so cancel before the first poll wins.
        let e = run_ask_session(&f, Backend::Pi, opts(), "q", &AtomicBool::new(true), Duration::from_secs(5), &mut |_| {}).unwrap_err();
        assert_eq!(e, "Cancelled.");
        let e = run_ask_session(&f, Backend::Pi, opts(), "q", &AtomicBool::new(false), Duration::ZERO, &mut |_| {}).unwrap_err();
        assert!(e.contains("No answer within"), "{e}");
    }
}
