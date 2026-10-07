//! pi backend: `pi --mode rpc` JSONL client.
//!
//! Framing is LF-only (U+2028/2029 stay inside records), stdout is drained continuously by a
//! reader thread that never blocks on the consumer, stderr is forwarded as `AgentEvent::Stderr`
//! and never parsed as protocol, responses are correlated by `id`.

use crate::config::Config;
use crate::proc::{self, Host, Spec};
use crate::util::{lossy_masked, mask_secrets, read_lf_lines, truncate};
use crate::{AgentEvent, AgentSession, Error, ModelInfo, ModelSel, Result, SessionOpts};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ChildStdin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Quiet period after `agent_end` before the run is reported as settled.
///
/// Limitation: pi 0.74.2 emits no `agent_settled` event, only a per-run `agent_end`, and pi may start
/// an automatic retry (`auto_retry_start`, then another `agent_start`) right after an `agent_end`.
/// Without a settle signal the run is therefore reported as settled only once no retry/compaction/new
/// run has begun for this long. A pi that sends `agent_settled` bypasses the wait. A retry that
/// starts later than this window would be reported as a second run of an already-settled prompt.
const SETTLE_QUIET_PERIOD: Duration = Duration::from_millis(400);

/// Capacity of the consumer-facing event queue (`AgentSession::events`).
const EVENT_QUEUE: usize = 1024;
/// Capacity of the reader -> dispatcher queue. When full the reader blocks and pi's pipe backs up.
const MSG_QUEUE: usize = 1024;
/// Stderr lines kept while the consumer lags; older lines are dropped (and counted).
const STDERR_RING: usize = 200;
/// How often a lagging consumer's queue is retried for coalesced text / stderr.
const OUTBOX_RETRY: Duration = Duration::from_millis(20);

enum Msg {
    Event(Value),
    /// A line that is not a JSON object, or a response without an id (already masked/truncated).
    Note(String),
    Err(Vec<u8>),
    OutEof,
    ErrEof,
    Oversize(&'static str, usize),
}

/// Consumer-facing side of the pipeline. Bounded: terminal/tool events block (nothing is lost, pi is
/// back-pressured through the pipe), text deltas are coalesced while the queue is full, stderr lines
/// live in a ring.
struct Outbox {
    tx: SyncSender<AgentEvent>,
    delta: String,
    errs: VecDeque<String>,
    dropped: usize,
    dead: bool,
}

impl Outbox {
    fn new(tx: SyncSender<AgentEvent>) -> Self {
        Outbox { tx, delta: String::new(), errs: VecDeque::new(), dropped: 0, dead: false }
    }

    fn lagging(&self) -> bool {
        !self.dead && (!self.delta.is_empty() || !self.errs.is_empty() || self.dropped > 0)
    }

    fn text(&mut self, t: &str) {
        if self.dead {
            return;
        }
        self.delta.push_str(t);
        self.pump();
    }

    fn stderr(&mut self, line: String) {
        if self.dead {
            return;
        }
        if self.errs.len() >= STDERR_RING {
            self.errs.pop_front();
            self.dropped += 1;
        }
        self.errs.push_back(line);
        self.pump();
    }

    /// Non-blocking: hand over as much pending text/stderr as the queue accepts.
    fn pump(&mut self) {
        if self.dead {
            return;
        }
        if !self.delta.is_empty() {
            match self.tx.try_send(AgentEvent::TextDelta(std::mem::take(&mut self.delta))) {
                Ok(()) => {}
                Err(TrySendError::Full(AgentEvent::TextDelta(t))) => {
                    self.delta = t;
                    return;
                }
                Err(_) => return self.kill(),
            }
        }
        if self.dropped > 0 {
            let note = AgentEvent::Stderr(format!("[{} stderr lines dropped]", self.dropped));
            match self.tx.try_send(note) {
                Ok(()) => self.dropped = 0,
                Err(TrySendError::Full(_)) => return,
                Err(_) => return self.kill(),
            }
        }
        while let Some(e) = self.errs.pop_front() {
            match self.tx.try_send(AgentEvent::Stderr(e)) {
                Ok(()) => {}
                Err(TrySendError::Full(AgentEvent::Stderr(e))) => {
                    self.errs.push_front(e);
                    return;
                }
                Err(_) => return self.kill(),
            }
        }
    }

    fn kill(&mut self) {
        self.dead = true;
        self.delta.clear();
        self.errs.clear();
    }

    /// Blocking send of everything pending, then `ev`. Returns when the consumer took it or is gone.
    fn send(&mut self, ev: AgentEvent) {
        if self.dead {
            return;
        }
        let mut batch: Vec<AgentEvent> = Vec::new();
        if !self.delta.is_empty() {
            batch.push(AgentEvent::TextDelta(std::mem::take(&mut self.delta)));
        }
        if self.dropped > 0 {
            batch.push(AgentEvent::Stderr(format!("[{} stderr lines dropped]", self.dropped)));
            self.dropped = 0;
        }
        batch.extend(self.errs.drain(..).map(AgentEvent::Stderr));
        batch.push(ev);
        for e in batch {
            if self.tx.send(e).is_err() {
                return self.kill();
            }
        }
    }
}

/// Maps pi events to [`AgentEvent`]s. Pure state machine; time is passed in.
pub(crate) struct Mapper {
    tools: HashMap<String, (String, Option<String>)>,
    saw_settled_event: bool,
    last_end: Option<(bool, Option<String>)>,
    pending_settle: Option<(Instant, AgentEvent)>,
    running: bool,
}

impl Mapper {
    pub fn new() -> Self {
        Mapper { tools: HashMap::new(), saw_settled_event: false, last_end: None, pending_settle: None, running: false }
    }

    pub fn running(&self) -> bool {
        self.running
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.pending_settle.as_ref().map(|(t, _)| *t)
    }

    fn settle_now(&mut self, ev: AgentEvent, out: &mut Vec<AgentEvent>) {
        self.pending_settle = None;
        self.running = false;
        out.push(ev);
    }

    /// Flush the debounced settle if its time has come.
    pub fn tick(&mut self, now: Instant) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        if let Some((t, _)) = &self.pending_settle {
            if *t <= now {
                let (_, ev) = self.pending_settle.take().unwrap();
                self.settle_now(ev, &mut out);
            }
        }
        out
    }

    /// The process is gone: flush a pending settle, or fail a run that was cut off.
    pub fn on_exit(&mut self, code: Option<i32>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        if let Some((_, ev)) = self.pending_settle.take() {
            self.settle_now(ev, &mut out);
        } else if self.running {
            let how = match code {
                Some(c) => format!("pi exited unexpectedly (code {c})"),
                None => "pi was killed by a signal".to_string(),
            };
            self.settle_now(AgentEvent::Settled { ok: false, error: Some(how) }, &mut out);
        }
        out.push(AgentEvent::Exited(code));
        out
    }

    pub fn on_event(&mut self, v: &Value, now: Instant) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "agent_start" => {
                self.pending_settle = None;
                self.running = true;
                out.push(AgentEvent::Started);
            }
            "auto_retry_start" | "compaction_start" => {
                self.pending_settle = None;
            }
            "message_update" => {
                if let Some(e) = v.get("assistantMessageEvent") {
                    if e.get("type").and_then(Value::as_str) == Some("text_delta") {
                        if let Some(d) = e.get("delta").and_then(Value::as_str) {
                            out.push(AgentEvent::TextDelta(d.to_string()));
                        }
                    }
                }
            }
            "message_end" => {
                let m = v.get("message");
                if m.and_then(|m| m.get("role")).and_then(Value::as_str) == Some("assistant") {
                    if let Some(u) = m.and_then(|m| m.get("usage")) {
                        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                        out.push(AgentEvent::Usage {
                            input: n("input") + n("cacheRead") + n("cacheWrite"),
                            output: n("output"),
                            cost: u.get("cost").and_then(|c| c.get("total")).and_then(Value::as_f64),
                        });
                    }
                }
            }
            "tool_execution_start" => {
                let id = str_of(v, "toolCallId");
                let name = str_of(v, "toolName");
                let args = v.get("args").cloned().unwrap_or(Value::Null);
                let path = file_path_of(&name, &args);
                self.tools.insert(id.clone(), (name.clone(), path));
                out.push(AgentEvent::ToolStart { id, name: name.clone(), summary: tool_summary(&name, &args) });
            }
            "tool_execution_end" => {
                let id = str_of(v, "toolCallId");
                let (name, path) = self.tools.remove(&id).unwrap_or_else(|| (str_of(v, "toolName"), None));
                let name = if name.is_empty() { str_of(v, "toolName") } else { name };
                let is_error = v.get("isError").and_then(Value::as_bool).unwrap_or(false);
                out.push(AgentEvent::ToolEnd { id, name, ok: !is_error, path });
            }
            "agent_end" => {
                let result = end_result(v);
                self.last_end = Some(result.clone());
                if !self.saw_settled_event {
                    let ev = AgentEvent::Settled { ok: result.0, error: result.1 };
                    self.pending_settle = Some((now + SETTLE_QUIET_PERIOD, ev));
                }
            }
            "agent_settled" => {
                self.saw_settled_event = true;
                let (mut ok, mut err) = self.last_end.clone().unwrap_or((true, None));
                if let Some(b) = v.get("ok").and_then(Value::as_bool) {
                    ok = b;
                }
                if let Some(e) = v.get("error").or_else(|| v.get("errorMessage")).and_then(Value::as_str) {
                    ok = false;
                    err = Some(mask_secrets(&truncate(e, 2000)));
                }
                self.last_end = None;
                self.settle_now(AgentEvent::Settled { ok, error: err }, &mut out);
            }
            _ => {}
        }
        out
    }
}

fn str_of(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn end_result(v: &Value) -> (bool, Option<String>) {
    let last = v
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().rev().find(|m| m.get("role").and_then(Value::as_str) == Some("assistant")));
    match last.and_then(|m| m.get("stopReason")).and_then(Value::as_str) {
        Some("error") => {
            let msg = last.and_then(|m| m.get("errorMessage")).and_then(Value::as_str).unwrap_or("agent error");
            (false, Some(mask_secrets(&truncate(msg, 2000))))
        }
        Some("aborted") => (false, Some("aborted".to_string())),
        _ => (true, None),
    }
}

fn file_path_of(name: &str, args: &Value) -> Option<String> {
    if !matches!(name.to_ascii_lowercase().as_str(), "edit" | "write") {
        return None;
    }
    args.get("path").or_else(|| args.get("file_path")).and_then(Value::as_str).map(str::to_string)
}

fn tool_summary(name: &str, args: &Value) -> String {
    let pick = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
    let s = match name {
        "bash" => pick("command"),
        "read" | "write" | "edit" | "ls" => pick("path"),
        "grep" | "find" => pick("pattern").or_else(|| pick("path")),
        _ => None,
    }
    .unwrap_or_else(|| args.to_string());
    mask_secrets(&truncate(&s, 300))
}

struct Inner {
    host: Host,
    stdin: Mutex<Option<ChildStdin>>,
    pending: Arc<Mutex<HashMap<String, Sender<Value>>>>,
    next_id: AtomicU64,
    running: Arc<AtomicBool>,
    shut: AtomicBool,
    rpc_timeout: Duration,
    grace_abort: Duration,
    grace_close: Duration,
    grace_term: Duration,
}

impl Inner {
    fn write_line(&self, v: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(v).map_err(|e| Error::Other(e.to_string()))?;
        line.push(b'\n');
        let mut g = self.stdin.lock();
        let w = g.as_mut().ok_or_else(|| Error::Other("pi session is closed".into()))?;
        w.write_all(&line).and_then(|_| w.flush()).map_err(|e| Error::Other(format!("write to pi failed: {e}")))
    }

    /// Sends a command and waits for its `response`; returns `data` on success.
    fn request(&self, mut cmd: Value) -> Result<Value> {
        let id = format!("ct-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        cmd["id"] = json!(id);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().insert(id.clone(), tx);
        if let Err(e) = self.write_line(&cmd) {
            self.pending.lock().remove(&id);
            return Err(e);
        }
        match rx.recv_timeout(self.rpc_timeout) {
            Ok(resp) => {
                if resp.get("success").and_then(Value::as_bool) == Some(true) {
                    Ok(resp.get("data").cloned().unwrap_or(Value::Null))
                } else {
                    let e = resp.get("error").and_then(Value::as_str).unwrap_or("command failed");
                    Err(Error::Other(mask_secrets(&truncate(e, 2000))))
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                self.pending.lock().remove(&id);
                Err(Error::Other(format!("pi did not answer within {:?}", self.rpc_timeout)))
            }
            Err(RecvTimeoutError::Disconnected) => Err(Error::Other("pi exited before answering".into())),
        }
    }

    /// abort -> close stdin -> (grace) -> SIGTERM group -> (grace) -> SIGKILL group.
    fn shutdown(&self) {
        if self.shut.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.host.exited().is_none() {
            if self.running.load(Ordering::SeqCst) {
                let _ = self.write_line(&json!({"type": "abort"}));
                // let the abort land before the pipe closes
                self.host.wait_exit(Duration::from_millis(50));
            }
            self.stdin.lock().take();
            let grace = if self.running.load(Ordering::SeqCst) { self.grace_abort } else { self.grace_close };
            if self.host.wait_exit(grace).is_none() {
                self.host.terminate(self.grace_term);
            }
        }
        self.host.signal_group(libc::SIGKILL);
    }
}

struct Conn {
    inner: Arc<Inner>,
    events: Option<Receiver<AgentEvent>>,
    dispatcher: Option<std::thread::JoinHandle<()>>,
}

fn pi_command(cfg: &Config) -> Result<(PathBuf, Vec<OsString>)> {
    if let Some(p) = &cfg.pi_path {
        if !p.exists() {
            return Err(Error::Other(format!("configured agent.pi_path does not exist: {}", p.display())));
        }
        return Ok((p.clone(), Vec::new()));
    }
    let (node, cli) = (cfg.node_bin(), cfg.pi_cli_js());
    if !node.exists() || !cli.exists() {
        return Err(Error::Other("agent runtime is not installed; run runtime_setup first".into()));
    }
    Ok((node, vec![cli.into_os_string()]))
}

fn open(cfg: &Config, extra_args: Vec<OsString>, cwd: Option<PathBuf>, env_allow: &[String]) -> Result<Conn> {
    let (program, mut args) = pi_command(cfg)?;
    args.extend(["--mode", "rpc", "--no-session", "--no-extensions"].map(OsString::from));
    args.extend(extra_args);
    let home = cfg.pi_home();
    std::fs::create_dir_all(&home).map_err(|e| Error::Other(format!("cannot create {}: {e}", home.display())))?;
    let mut env = proc::whitelist_env(&[], env_allow);
    proc::set_env(&mut env, "PI_CODING_AGENT_DIR", home.to_string_lossy());
    proc::set_env(&mut env, "PI_SKIP_VERSION_CHECK", "1");
    proc::set_env(&mut env, "PI_TELEMETRY", "0");
    let sp = proc::spawn(Spec { program: program.clone(), args, cwd, env })
        .map_err(|e| Error::Other(format!("cannot start {}: {e}", program.display())))?;

    let pending: Arc<Mutex<HashMap<String, Sender<Value>>>> = Arc::new(Mutex::new(HashMap::new()));
    let (msg_tx, msg_rx) = mpsc::sync_channel::<Msg>(MSG_QUEUE);
    let tx = msg_tx.clone();
    let mut stdout = sp.stdout;
    let route = pending.clone();
    std::thread::Builder::new().name("ct-pi-stdout".into()).spawn(move || {
        let t2 = tx.clone();
        read_lf_lines(
            &mut stdout,
            |l| {
                if l.is_empty() {
                    return;
                }
                // responses are routed here, not in the dispatcher: a consumer that issues a request
                // from its event loop must get the answer even while the event queue is full
                let msg = match serde_json::from_slice::<Value>(&l) {
                    Ok(v) if v.is_object() => {
                        if v.get("type").and_then(Value::as_str) == Some("response") {
                            match v.get("id").and_then(Value::as_str) {
                                Some(id) => {
                                    if let Some(w) = route.lock().remove(id) {
                                        let _ = w.send(v);
                                    }
                                    return;
                                }
                                None => {
                                    let e = v.get("error").and_then(Value::as_str).unwrap_or("response without id");
                                    Msg::Note(format!("pi: {}", mask_secrets(&truncate(e, 500))))
                                }
                            }
                        } else {
                            Msg::Event(v)
                        }
                    }
                    _ => Msg::Note(format!("unparsable pi output: {}", lossy_masked(&l[..l.len().min(300)]))),
                };
                let _ = tx.send(msg);
            },
            |n| drop(t2.send(Msg::Oversize("stdout", n))),
        );
        let _ = t2.send(Msg::OutEof);
    })?;
    let tx = msg_tx.clone();
    let mut stderr = sp.stderr;
    std::thread::Builder::new().name("ct-pi-stderr".into()).spawn(move || {
        let t2 = tx.clone();
        read_lf_lines(&mut stderr, |l| drop(tx.send(Msg::Err(l))), |n| drop(t2.send(Msg::Oversize("stderr", n))));
        let _ = t2.send(Msg::ErrEof);
    })?;
    drop(msg_tx);

    let running = Arc::new(AtomicBool::new(false));
    let inner = Arc::new(Inner {
        host: sp.host.clone(),
        stdin: Mutex::new(Some(sp.stdin)),
        pending: pending.clone(),
        next_id: AtomicU64::new(1),
        running: running.clone(),
        shut: AtomicBool::new(false),
        rpc_timeout: cfg.rpc_timeout,
        grace_abort: cfg.grace_abort,
        grace_close: cfg.grace_close,
        grace_term: cfg.grace_term,
    });
    let (ev_tx, ev_rx) = mpsc::sync_channel::<AgentEvent>(EVENT_QUEUE);
    let host = sp.host;
    let disp_inner = inner.clone();
    let dispatcher = std::thread::Builder::new().name("ct-pi-dispatch".into()).spawn(move || {
        dispatch(msg_rx, ev_tx, pending, running, host, disp_inner);
    })?;
    Ok(Conn { inner, events: Some(ev_rx), dispatcher: Some(dispatcher) })
}

fn dispatch(
    rx: Receiver<Msg>,
    ev: SyncSender<AgentEvent>,
    pending: Arc<Mutex<HashMap<String, Sender<Value>>>>,
    running: Arc<AtomicBool>,
    host: Host,
    inner: Arc<Inner>,
) {
    let mut m = Mapper::new();
    let mut out = Outbox::new(ev);
    let (mut out_eof, mut err_eof) = (false, false);
    fn emit(out: &mut Outbox, running: &AtomicBool, m: &Mapper, evs: Vec<AgentEvent>) {
        running.store(m.running(), Ordering::SeqCst);
        for e in evs {
            match e {
                AgentEvent::TextDelta(t) => out.text(&t),
                other => out.send(other),
            }
        }
    }
    while !(out_eof && err_eof) {
        let mut wait: Option<Duration> = m.deadline().map(|d| d.saturating_duration_since(Instant::now()));
        if out.lagging() {
            wait = Some(wait.map_or(OUTBOX_RETRY, |w| w.min(OUTBOX_RETRY)));
        }
        let msg = match wait {
            Some(w) => match rx.recv_timeout(w) {
                Ok(x) => Some(x),
                Err(RecvTimeoutError::Timeout) => {
                    out.pump();
                    let evs = m.tick(Instant::now());
                    emit(&mut out, &running, &m, evs);
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => None,
            },
            None => rx.recv().ok(),
        };
        let Some(msg) = msg else { break };
        match msg {
            Msg::Event(v) => {
                if v.get("type").and_then(Value::as_str) == Some("extension_ui_request") {
                    // no UI here: dismiss dialogs so pi never blocks on us
                    let is_dialog =
                        matches!(v.get("method").and_then(Value::as_str), Some("select" | "confirm" | "input" | "editor"));
                    if let (true, Some(id)) = (is_dialog, v.get("id").and_then(Value::as_str)) {
                        let _ = inner.write_line(&json!({"type": "extension_ui_response", "id": id, "cancelled": true}));
                    }
                } else {
                    let evs = m.on_event(&v, Instant::now());
                    emit(&mut out, &running, &m, evs);
                }
            }
            Msg::Note(t) => out.stderr(t),
            Msg::Err(line) => out.stderr(lossy_masked(&line[..line.len().min(4000)])),
            Msg::Oversize(which, n) => out.stderr(format!("dropped an oversized pi {which} line ({n} bytes)")),
            Msg::OutEof => out_eof = true,
            Msg::ErrEof => err_eof = true,
        }
    }
    let code = host.wait_exit(Duration::from_secs(10)).unwrap_or(None);
    pending.lock().clear();
    let evs = m.on_exit(code);
    emit(&mut out, &running, &m, evs);
}

impl Conn {
    fn close(mut self) {
        // releases a dispatcher blocked on a full event queue
        self.events.take();
        self.inner.shutdown();
        if let Some(h) = self.dispatcher.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if self.dispatcher.is_some() {
            // dropped without close(): shut down off-thread, never block the caller
            let inner = self.inner.clone();
            let d = self.dispatcher.take();
            self.events.take();
            let _ = std::thread::Builder::new().name("ct-pi-reaper".into()).spawn(move || {
                inner.shutdown();
                if let Some(h) = d {
                    let _ = h.join();
                }
            });
        }
    }
}

// ---------------------------------------------------------------- session

pub(crate) struct PiSession {
    conn: Option<Conn>,
}

fn model_args(m: &ModelSel) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    match &m.provider {
        Some(p) => a.extend(["--provider".into(), p.into(), "--model".into(), (&m.id).into()]),
        None => a.extend(["--model".into(), (&m.id).into()]),
    }
    if let Some(t) = &m.thinking {
        a.extend(["--thinking".into(), t.into()]);
    }
    a
}

pub(crate) fn start(cfg: &Config, o: SessionOpts) -> Result<Box<dyn AgentSession>> {
    let mut args: Vec<OsString> = Vec::new();
    if let Some(m) = &o.model {
        args.extend(model_args(m));
    }
    if o.read_only {
        args.extend(["--tools", "read,grep,find,ls"].map(OsString::from));
    }
    if let Some(n) = &o.system_note {
        args.extend(["--append-system-prompt".into(), n.into()]);
    }
    let conn = open(cfg, args, Some(o.cwd.clone()), &o.env_allow)?;
    // handshake: proves the child speaks the protocol before the caller relies on it
    if let Err(e) = conn.inner.request(json!({"type": "get_state"})) {
        let msg = format!("pi did not start cleanly: {e}");
        conn.close();
        return Err(Error::Other(msg));
    }
    Ok(Box::new(PiSession { conn: Some(conn) }))
}

impl PiSession {
    fn conn(&self) -> Result<&Conn> {
        self.conn.as_ref().ok_or_else(|| Error::Other("session is closed".into()))
    }
}

impl AgentSession for PiSession {
    fn pid(&self) -> Option<u32> {
        self.conn.as_ref().map(|c| c.inner.host.pid())
    }
    fn prompt(&mut self, text: &str) -> Result<()> {
        self.conn()?.inner.request(json!({"type": "prompt", "message": text})).map(|_| ())
    }
    fn steer(&mut self, text: &str) -> Result<()> {
        self.conn()?.inner.request(json!({"type": "steer", "message": text})).map(|_| ())
    }
    fn follow_up(&mut self, text: &str) -> Result<()> {
        self.conn()?.inner.request(json!({"type": "follow_up", "message": text})).map(|_| ())
    }
    /// Fire-and-forget: the command is written and the call returns. pi's response (if any) carries an
    /// id nobody waits on and is dropped by the reader; a hung pi cannot delay the caller's escalation.
    fn abort(&mut self) -> Result<()> {
        let inner = &self.conn()?.inner;
        let id = format!("ct-abort-{}", inner.next_id.fetch_add(1, Ordering::Relaxed));
        inner.write_line(&json!({"type": "abort", "id": id}))
    }
    fn set_model(&mut self, m: &ModelSel) -> Result<()> {
        let c = self.conn()?;
        let provider = match &m.provider {
            Some(p) => p.clone(),
            None => {
                let models = parse_models(&c.inner.request(json!({"type": "get_available_models"}))?);
                let mut it = models.iter().filter(|x| x.id == m.id);
                match (it.next(), it.next()) {
                    (Some(x), None) => x.provider.clone(),
                    (None, _) => return Err(Error::Other(format!("model not available: {}", m.id))),
                    _ => return Err(Error::Other(format!("model id {} is ambiguous; give a provider", m.id))),
                }
            }
        };
        c.inner.request(json!({"type": "set_model", "provider": provider, "modelId": m.id}))?;
        if let Some(t) = &m.thinking {
            c.inner.request(json!({"type": "set_thinking_level", "level": t}))?;
        }
        Ok(())
    }
    fn events(&self) -> &Receiver<AgentEvent> {
        self.conn.as_ref().and_then(|c| c.events.as_ref()).expect("events() on a closed session")
    }
    fn close(mut self: Box<Self>) {
        if let Some(c) = self.conn.take() {
            c.close();
        }
    }
}

// ---------------------------------------------------------------- models

pub(crate) fn parse_models(data: &Value) -> Vec<ModelInfo> {
    data.get("models")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let id = m.get("id")?.as_str()?.to_string();
                    let provider = m.get("provider")?.as_str()?.to_string();
                    Some(ModelInfo {
                        backend: crate::BackendKind::Pi,
                        provider,
                        name: m.get("name").and_then(Value::as_str).unwrap_or(&id).to_string(),
                        id,
                        reasoning: m.get("reasoning").and_then(Value::as_bool).unwrap_or(false),
                        context_window: m
                            .get("contextWindow")
                            .and_then(Value::as_u64)
                            .map(|n| n.min(u32::MAX as u64) as u32)
                            .unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Short-lived RPC: start pi, `get_available_models`, shut down.
pub(crate) fn list_models_uncached(cfg: &Config) -> Result<Vec<ModelInfo>> {
    let cwd = std::env::temp_dir();
    let conn = open(cfg, vec!["--no-tools".into()], Some(cwd), &[])?;
    let res = conn.inner.request(json!({"type": "get_available_models"}));
    conn.close();
    Ok(parse_models(&res?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(m: &mut Mapper, lines: &[&str], now: Instant) -> Vec<AgentEvent> {
        lines.iter().flat_map(|l| m.on_event(&serde_json::from_str(l).unwrap(), now)).collect()
    }

    #[test]
    fn maps_text_tools_usage_and_debounced_settle() {
        let mut m = Mapper::new();
        let t0 = Instant::now();
        let evs = feed(
            &mut m,
            &[
                r#"{"type":"agent_start"}"#,
                r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"hi"}}"#,
                r#"{"type":"tool_execution_start","toolCallId":"c1","toolName":"edit","args":{"path":"a.rs","edits":[]}}"#,
                r#"{"type":"tool_execution_end","toolCallId":"c1","toolName":"edit","isError":false,"result":{}}"#,
                r#"{"type":"message_end","message":{"role":"assistant","usage":{"input":10,"output":5,"cacheRead":2,"cacheWrite":1,"cost":{"total":0.5}}}}"#,
                r#"{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop"}]}"#,
            ],
            t0,
        );
        assert_eq!(evs[0], AgentEvent::Started);
        assert_eq!(evs[1], AgentEvent::TextDelta("hi".into()));
        assert!(matches!(&evs[2], AgentEvent::ToolStart { id, name, summary } if id == "c1" && name == "edit" && summary == "a.rs"));
        assert_eq!(evs[3], AgentEvent::ToolEnd { id: "c1".into(), name: "edit".into(), ok: true, path: Some("a.rs".into()) });
        assert_eq!(evs[4], AgentEvent::Usage { input: 13, output: 5, cost: Some(0.5) });
        assert_eq!(evs.len(), 5, "settle is debounced");
        assert!(m.tick(t0).is_empty());
        assert_eq!(m.tick(t0 + SETTLE_QUIET_PERIOD), vec![AgentEvent::Settled { ok: true, error: None }]);
    }

    #[test]
    fn retry_after_error_end_cancels_settle() {
        let mut m = Mapper::new();
        let t0 = Instant::now();
        feed(&mut m, &[r#"{"type":"agent_start"}"#, r#"{"type":"agent_end","messages":[{"role":"assistant","stopReason":"error","errorMessage":"overloaded"}]}"#, r#"{"type":"auto_retry_start","attempt":1}"#], t0);
        assert!(m.tick(t0 + SETTLE_QUIET_PERIOD * 2).is_empty());
        feed(&mut m, &[r#"{"type":"agent_start"}"#, r#"{"type":"agent_end","messages":[{"role":"assistant","stopReason":"error","errorMessage":"overloaded sk-abcdefghijklmnop12345"}]}"#], t0);
        let evs = m.tick(t0 + SETTLE_QUIET_PERIOD);
        match &evs[..] {
            [AgentEvent::Settled { ok: false, error: Some(e) }] => assert!(e.contains("overloaded") && !e.contains("abcdefghijklmnop")),
            x => panic!("{x:?}"),
        }
    }

    #[test]
    fn agent_settled_event_wins_over_agent_end() {
        let mut m = Mapper::new();
        let t0 = Instant::now();
        let evs = feed(&mut m, &[r#"{"type":"agent_start"}"#, r#"{"type":"agent_end","messages":[]}"#, r#"{"type":"agent_settled"}"#], t0);
        assert_eq!(evs.last(), Some(&AgentEvent::Settled { ok: true, error: None }));
        assert!(m.tick(t0 + SETTLE_QUIET_PERIOD * 2).is_empty());
    }

    #[test]
    fn outbox_stays_bounded_coalesces_text_and_never_drops_terminal_events() {
        let (tx, rx) = mpsc::sync_channel(8);
        let mut o = Outbox::new(tx);
        // consumer is stalled: none of this may block, and the queue holds at most its capacity
        for i in 0..200_000 {
            o.text("xy");
            if i % 1000 == 0 {
                o.stderr(format!("e{i}"));
            }
        }
        for i in 0..1000 {
            o.stderr(format!("noise{i}"));
        }
        assert!(o.errs.len() <= STDERR_RING && o.dropped > 0);
        let t = std::thread::spawn(move || {
            o.send(AgentEvent::Settled { ok: true, error: None });
            o
        });
        std::thread::sleep(Duration::from_millis(50));
        let mut text = 0;
        let mut last = None;
        while let Ok(e) = rx.recv_timeout(Duration::from_secs(2)) {
            if let AgentEvent::TextDelta(s) = &e {
                text += s.len();
            }
            let done = matches!(e, AgentEvent::Settled { .. });
            last = Some(e);
            if done {
                break;
            }
        }
        assert_eq!(text, 400_000);
        assert_eq!(last, Some(AgentEvent::Settled { ok: true, error: None }));
        let _ = t.join();
    }

    #[test]
    fn crash_mid_run_settles_failed_then_exits() {
        let mut m = Mapper::new();
        feed(&mut m, &[r#"{"type":"agent_start"}"#], Instant::now());
        let evs = m.on_exit(Some(3));
        assert!(matches!(&evs[0], AgentEvent::Settled { ok: false, error: Some(e) } if e.contains("code 3")));
        assert_eq!(evs[1], AgentEvent::Exited(Some(3)));
    }
}
