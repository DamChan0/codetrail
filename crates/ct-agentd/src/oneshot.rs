//! Claude / Codex backends: one official-CLI process per prompt, JSONL on stdout.
//!
//! The CLIs are used unmodified and their credentials are never read: the child inherits only
//! the env whitelist and authenticates itself.

use crate::config::Config;
use crate::proc::{self, Host, Spec};
use crate::util::{lossy_masked, mask_secrets, read_lf_lines, truncate};
use crate::{AgentEvent, AgentSession, BackendKind, Error, ModelSel, Result, SessionOpts};
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

// ---------------------------------------------------------------- protocol parsers

pub(crate) trait Proto: Send {
    fn on_line(&mut self, v: &Value) -> Vec<AgentEvent>;
    /// Has a `Settled` already been produced for this process?
    fn settled(&self) -> bool;
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn summary_of(name: &str, input: &Value) -> String {
    let pick = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
    let t = match name {
        "Bash" => pick("command"),
        "Read" | "Write" | "Edit" | "MultiEdit" => pick("file_path"),
        "NotebookEdit" => pick("notebook_path"),
        "Grep" | "Glob" => pick("pattern"),
        "WebFetch" => pick("url"),
        _ => None,
    }
    .unwrap_or_else(|| input.to_string());
    mask_secrets(&truncate(&t, 300))
}

#[derive(Default)]
pub(crate) struct ClaudeProto {
    tools: HashMap<String, (String, Option<String>)>,
    streamed_text: bool,
    settled: bool,
    started: bool,
}

impl Proto for ClaudeProto {
    fn settled(&self) -> bool {
        self.settled
    }

    fn on_line(&mut self, v: &Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "system" if v.get("subtype").and_then(Value::as_str) == Some("init") && !self.started => {
                self.started = true;
                out.push(AgentEvent::Started);
            }
            "stream_event" => {
                let e = v.get("event").unwrap_or(&Value::Null);
                match e.get("type").and_then(Value::as_str) {
                    Some("message_start") => self.streamed_text = false,
                    Some("content_block_delta") => {
                        let d = e.get("delta").unwrap_or(&Value::Null);
                        if d.get("type").and_then(Value::as_str) == Some("text_delta") {
                            if let Some(t) = d.get("text").and_then(Value::as_str) {
                                self.streamed_text = true;
                                out.push(AgentEvent::TextDelta(t.to_string()));
                            }
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                // sub-agent chatter is not the run's output
                if v.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) {
                    return out;
                }
                let blocks = v.get("message").and_then(|m| m.get("content")).and_then(Value::as_array);
                for b in blocks.into_iter().flatten() {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") if !self.streamed_text => {
                            if let Some(t) = b.get("text").and_then(Value::as_str) {
                                out.push(AgentEvent::TextDelta(t.to_string()));
                            }
                        }
                        Some("tool_use") => {
                            let id = s(b, "id");
                            let name = s(b, "name");
                            let input = b.get("input").cloned().unwrap_or(Value::Null);
                            let path = if matches!(name.as_str(), "Write" | "Edit" | "MultiEdit" | "NotebookEdit") {
                                input
                                    .get("file_path")
                                    .or_else(|| input.get("notebook_path"))
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                            } else {
                                None
                            };
                            self.tools.insert(id.clone(), (name.clone(), path));
                            out.push(AgentEvent::ToolStart { summary: summary_of(&name, &input), id, name });
                        }
                        _ => {}
                    }
                }
            }
            "user" => {
                let blocks = v.get("message").and_then(|m| m.get("content")).and_then(Value::as_array);
                for b in blocks.into_iter().flatten() {
                    if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                        let id = s(b, "tool_use_id");
                        let (name, path) = self.tools.remove(&id).unwrap_or_default();
                        let ok = !b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                        out.push(AgentEvent::ToolEnd { id, name, ok, path });
                    }
                }
            }
            "result" => {
                if let Some(u) = v.get("usage") {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    out.push(AgentEvent::Usage {
                        input: n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens"),
                        output: n("output_tokens"),
                        cost: v.get("total_cost_usd").and_then(Value::as_f64),
                    });
                }
                let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                    || v.get("subtype").and_then(Value::as_str).is_some_and(|t| t != "success");
                let error = if is_error {
                    let text = v.get("result").and_then(Value::as_str).filter(|t| !t.is_empty());
                    let sub = v.get("subtype").and_then(Value::as_str).unwrap_or("error");
                    Some(mask_secrets(&truncate(text.unwrap_or(sub), 2000)))
                } else {
                    None
                };
                self.settled = true;
                out.push(AgentEvent::Settled { ok: !is_error, error });
            }
            _ => {}
        }
        out
    }
}

#[derive(Default)]
pub(crate) struct CodexProto {
    settled: bool,
}

fn codex_changes(item: &Value) -> Vec<(String, String)> {
    item.get("changes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|c| Some((c.get("path")?.as_str()?.to_string(), c.get("kind")?.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

impl Proto for CodexProto {
    fn settled(&self) -> bool {
        self.settled
    }

    fn on_line(&mut self, v: &Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "turn.started" => out.push(AgentEvent::Started),
            "item.started" | "item.completed" => {
                let done = v["type"] == "item.completed";
                let item = v.get("item").unwrap_or(&Value::Null);
                let id = s(item, "id");
                match item.get("type").and_then(Value::as_str).unwrap_or("") {
                    "agent_message" if done => {
                        if let Some(t) = item.get("text").and_then(Value::as_str) {
                            out.push(AgentEvent::TextDelta(t.to_string()));
                        }
                    }
                    "command_execution" => {
                        if done {
                            let ok = item.get("exit_code").and_then(Value::as_i64) == Some(0)
                                && item.get("status").and_then(Value::as_str) != Some("failed");
                            out.push(AgentEvent::ToolEnd { id, name: "bash".into(), ok, path: None });
                        } else {
                            let cmd = mask_secrets(&truncate(&s(item, "command"), 300));
                            out.push(AgentEvent::ToolStart { id, name: "bash".into(), summary: cmd });
                        }
                    }
                    "file_change" => {
                        let ok = item.get("status").and_then(Value::as_str) == Some("completed");
                        for (k, (path, kind)) in codex_changes(item).into_iter().enumerate() {
                            let cid = format!("{id}#{k}");
                            if done {
                                out.push(AgentEvent::ToolEnd { id: cid, name: "edit".into(), ok, path: Some(path) });
                            } else {
                                out.push(AgentEvent::ToolStart { id: cid, name: "edit".into(), summary: format!("{kind} {path}") });
                            }
                        }
                    }
                    "error" if done => {
                        out.push(AgentEvent::Stderr(mask_secrets(&truncate(&s(item, "message"), 1000))));
                    }
                    "mcp_tool_call" | "web_search" | "collab_tool_call" => {
                        let name = item.get("type").and_then(Value::as_str).unwrap_or("tool").to_string();
                        if done {
                            let ok = item.get("status").and_then(Value::as_str) != Some("failed");
                            out.push(AgentEvent::ToolEnd { id, name, ok, path: None });
                        } else {
                            let sm = item.get("query").or_else(|| item.get("tool")).and_then(Value::as_str).unwrap_or("");
                            out.push(AgentEvent::ToolStart { id, name, summary: mask_secrets(&truncate(sm, 300)) });
                        }
                    }
                    _ => {}
                }
            }
            "turn.completed" => {
                if let Some(u) = v.get("usage") {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    out.push(AgentEvent::Usage { input: n("input_tokens"), output: n("output_tokens"), cost: None });
                }
                self.settled = true;
                out.push(AgentEvent::Settled { ok: true, error: None });
            }
            "turn.failed" => {
                let msg = v.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).unwrap_or("turn failed");
                self.settled = true;
                out.push(AgentEvent::Settled { ok: false, error: Some(mask_secrets(&truncate(&codex_error_text(msg), 2000))) });
            }
            "error" => {
                out.push(AgentEvent::Stderr(mask_secrets(&truncate(&codex_error_text(&s(v, "message")), 1000))));
            }
            _ => {}
        }
        out
    }
}

/// Codex wraps API errors as a JSON string; pull out the human message when it is.
fn codex_error_text(raw: &str) -> String {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| raw.to_string())
}

// ---------------------------------------------------------------- command lines

fn claude_effort(t: &str) -> Result<Option<&'static str>> {
    Ok(match t {
        "off" => None,
        "minimal" | "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        "max" => Some("max"),
        other => return Err(Error::Other(format!("unsupported thinking level for claude: {other}"))),
    })
}

pub(crate) fn claude_args(o: &SessionOpts, model: &Option<ModelSel>) -> Result<Vec<OsString>> {
    let mut a: Vec<OsString> = ["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--no-session-persistence"]
        .map(OsString::from)
        .into();
    // the user's interactive-session hooks (SessionStart/Stop/SessionEnd plugins) do not belong in an
    // embedded run: with a reduced environment they stalled a one-word prompt for ~100s
    a.extend(["--settings", r#"{"disableAllHooks":true}"#].map(OsString::from));
    if let Some(m) = model {
        a.extend(["--model".into(), (&m.id).into()]);
        if let Some(e) = m.thinking.as_deref().map(claude_effort).transpose()?.flatten() {
            a.extend(["--effort".into(), e.into()]);
        }
    }
    if o.read_only {
        // no edit/exec tools at all; anything that would still prompt is denied, MCP servers are not loaded
        a.extend(["--tools", "Read,Grep,Glob", "--permission-mode", "dontAsk", "--permission-prompts", "none", "--strict-mcp-config"].map(OsString::from));
    } else {
        a.extend(["--permission-mode", "acceptEdits", "--permission-prompts", "none"].map(OsString::from));
    }
    if let Some(n) = &o.system_note {
        a.extend(["--append-system-prompt".into(), n.into()]);
    }
    Ok(a)
}

pub(crate) fn codex_args(o: &SessionOpts, model: &Option<ModelSel>) -> Result<Vec<OsString>> {
    let mut a: Vec<OsString> = ["exec", "--json", "--skip-git-repo-check", "--ephemeral", "--color", "never"].map(OsString::from).into();
    a.extend(["-s".into(), if o.read_only { "read-only" } else { "workspace-write" }.into()]);
    if let Some(m) = model {
        a.extend(["-m".into(), (&m.id).into()]);
        if let Some(t) = &m.thinking {
            let level = match t.as_str() {
                "off" => None,
                "minimal" => Some("minimal"),
                "low" | "medium" | "high" | "xhigh" => Some(t.as_str()),
                other => return Err(Error::Other(format!("unsupported thinking level for codex: {other}"))),
            };
            if let Some(l) = level {
                a.extend(["-c".into(), format!("model_reasoning_effort=\"{l}\"").into()]);
            }
        }
    }
    if let Some(n) = &o.system_note {
        // codex has no system-prompt flag; the note rides in the config as developer instructions
        a.extend(["-c".into(), format!("developer_instructions={}", serde_json::to_string(n).unwrap_or_default()).into()]);
    }
    a.push("-".into()); // prompt from stdin
    Ok(a)
}

// ---------------------------------------------------------------- session

struct Cur {
    host: Host,
    aborted: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    /// A terminal `Settled` was emitted (the process is only finishing up).
    settled: Arc<AtomicBool>,
    dispatcher: Option<std::thread::JoinHandle<()>>,
}

pub(crate) struct OneShotSession {
    cfg: Config,
    kind: BackendKind,
    opts: SessionOpts,
    model: Option<ModelSel>,
    tx: Sender<AgentEvent>,
    rx: Receiver<AgentEvent>,
    cur: Mutex<Option<Cur>>,
}

pub(crate) fn start(cfg: &Config, kind: BackendKind, o: SessionOpts) -> Result<Box<dyn AgentSession>> {
    debug_assert!(matches!(kind, BackendKind::Claude | BackendKind::Codex));
    let model = o.model.clone();
    // fail early on a bad model/thinking combination
    build_args(kind, &o, &model)?;
    let (tx, rx) = mpsc::channel();
    Ok(Box::new(OneShotSession { cfg: cfg.clone(), kind, opts: o, model, tx, rx, cur: Mutex::new(None) }))
}

fn build_args(kind: BackendKind, o: &SessionOpts, m: &Option<ModelSel>) -> Result<Vec<OsString>> {
    match kind {
        BackendKind::Claude => claude_args(o, m),
        BackendKind::Codex => codex_args(o, m),
        BackendKind::Pi => Err(Error::Other("not a one-shot backend".into())),
    }
}

enum Msg {
    Out(Vec<u8>),
    Err(Vec<u8>),
    Oversize(usize),
    OutEof,
    ErrEof,
}

impl OneShotSession {
    fn name(&self) -> &'static str {
        if self.kind == BackendKind::Claude {
            "claude"
        } else {
            "codex"
        }
    }

    fn running(&self) -> bool {
        self.cur.lock().as_ref().is_some_and(|c| !c.finished.load(Ordering::SeqCst))
    }

    fn stop_current(&self, block: bool) {
        let g = self.cur.lock();
        if let Some(c) = g.as_ref() {
            if c.finished.load(Ordering::SeqCst) {
                return;
            }
            c.aborted.store(true, Ordering::SeqCst);
            if block {
                c.host.terminate(self.cfg.grace_term);
            } else {
                c.host.signal_group(libc::SIGTERM);
                c.host.terminate_detached(self.cfg.grace_term);
            }
        }
    }
}

impl AgentSession for OneShotSession {
    fn pid(&self) -> Option<u32> {
        self.cur.lock().as_ref().filter(|c| !c.finished.load(Ordering::SeqCst)).map(|c| c.host.pid())
    }

    fn prompt(&mut self, text: &str) -> Result<()> {
        // A run that has already reported Settled is only waiting for its process to exit: let it finish
        // (its dispatcher ends right after Exited) instead of rejecting a follow-up prompt.
        let wait = {
            let mut g = self.cur.lock();
            match g.as_mut() {
                Some(c) if !c.finished.load(Ordering::SeqCst) => {
                    if !c.settled.load(Ordering::SeqCst) {
                        return Err(Error::Other(format!("a {} prompt is already running", self.name())));
                    }
                    c.dispatcher.take()
                }
                _ => None,
            }
        };
        if let Some(h) = wait {
            let _ = h.join();
        }
        let (program, backend_vars): (_, &[&str]) = match self.kind {
            BackendKind::Claude => (self.cfg.claude_bin.clone(), &["CLAUDE_CONFIG_DIR"]),
            _ => (self.cfg.codex_bin.clone(), &["CODEX_HOME"]),
        };
        let args = build_args(self.kind, &self.opts, &self.model)?;
        let env = proc::whitelist_env(backend_vars, &self.opts.env_allow);
        let sp = proc::spawn(Spec { program: program.clone(), args, cwd: Some(self.opts.cwd.clone()), env, ceiling: Some(self.cfg.ask_ceiling) })
            .map_err(|e| Error::Other(format!("cannot start {}: {e}", program.display())))?;

        // prompt goes through stdin (no argv length limit, no leading-dash ambiguity, not in `ps`)
        let mut stdin = sp.stdin;
        let body = text.as_bytes().to_vec();
        std::thread::Builder::new().name("ct-cli-stdin".into()).spawn(move || {
            let _ = stdin.write_all(&body);
        })?;

        let (mtx, mrx) = mpsc::channel::<Msg>();
        let t = mtx.clone();
        let mut out = sp.stdout;
        std::thread::Builder::new().name("ct-cli-stdout".into()).spawn(move || {
            let t2 = t.clone();
            read_lf_lines(&mut out, |l| drop(t.send(Msg::Out(l))), |n| drop(t2.send(Msg::Oversize(n))));
            let _ = t2.send(Msg::OutEof);
        })?;
        let t = mtx;
        let mut err = sp.stderr;
        std::thread::Builder::new().name("ct-cli-stderr".into()).spawn(move || {
            let t2 = t.clone();
            read_lf_lines(&mut err, |l| drop(t.send(Msg::Err(l))), |_| {});
            let _ = t2.send(Msg::ErrEof);
        })?;

        let aborted = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let host = sp.host;
        let settled = Arc::new(AtomicBool::new(false));
        let (ev, ab, fin, st, h2) = (self.tx.clone(), aborted.clone(), finished.clone(), settled.clone(), host.clone());
        let kind = self.kind;
        let name = self.name();
        let dispatcher = std::thread::Builder::new().name("ct-cli-dispatch".into()).spawn(move || {
            let mut proto: Box<dyn Proto> = match kind {
                BackendKind::Claude => Box::new(ClaudeProto::default()),
                _ => Box::new(CodexProto::default()),
            };
            let mut tail: VecDeque<String> = VecDeque::new();
            let (mut oe, mut ee) = (false, false);
            while !(oe && ee) {
                let Ok(m) = mrx.recv() else { break };
                match m {
                    Msg::Out(l) => match serde_json::from_slice::<Value>(&l) {
                        Ok(v) if v.is_object() => {
                            for e in proto.on_line(&v) {
                                if matches!(e, AgentEvent::Settled { .. }) {
                                    st.store(true, Ordering::SeqCst);
                                }
                                let _ = ev.send(e);
                            }
                        }
                        _ if l.is_empty() => {}
                        _ => {
                            let _ = ev.send(AgentEvent::Stderr(format!("unparsable {name} output: {}", lossy_masked(&l[..l.len().min(300)]))));
                        }
                    },
                    Msg::Err(l) => {
                        let text = lossy_masked(&l[..l.len().min(4000)]);
                        if tail.len() == 5 {
                            tail.pop_front();
                        }
                        tail.push_back(text.clone());
                        let _ = ev.send(AgentEvent::Stderr(text));
                    }
                    Msg::Oversize(n) => {
                        let _ = ev.send(AgentEvent::Stderr(format!("dropped an oversized {name} output line ({n} bytes)")));
                    }
                    Msg::OutEof => oe = true,
                    Msg::ErrEof => ee = true,
                }
            }
            let code = h2.wait_exit(std::time::Duration::from_secs(10)).unwrap_or(None);
            // The run is over for the caller once its terminal events can be observed: clear the running
            // state FIRST, so a consumer reacting to Settled/Exited can prompt again immediately.
            fin.store(true, Ordering::SeqCst);
            if !proto.settled() {
                let error = if ab.load(Ordering::SeqCst) {
                    "aborted".to_string()
                } else {
                    let last = tail.back().cloned().unwrap_or_default();
                    match code {
                        Some(0) => format!("{name} ended without a result"),
                        Some(c) => format!("{name} exited with code {c}{}", if last.is_empty() { String::new() } else { format!(": {last}") }),
                        None => format!("{name} was killed by a signal"),
                    }
                };
                let _ = ev.send(AgentEvent::Settled { ok: false, error: Some(error) });
            }
            let _ = ev.send(AgentEvent::Exited(code));
        })?;

        let mut g = self.cur.lock();
        if let Some(mut old) = g.take() {
            if let Some(h) = old.dispatcher.take() {
                let _ = h.join();
            }
        }
        *g = Some(Cur { host, aborted, finished, settled, dispatcher: Some(dispatcher) });
        Ok(())
    }

    fn steer(&mut self, _text: &str) -> Result<()> {
        Err(Error::Unsupported("steer"))
    }

    fn follow_up(&mut self, _text: &str) -> Result<()> {
        Err(Error::Unsupported("follow_up"))
    }

    fn abort(&mut self) -> Result<()> {
        self.stop_current(false);
        Ok(())
    }

    fn set_model(&mut self, m: &ModelSel) -> Result<()> {
        if m.backend != self.kind {
            return Err(Error::Other("model belongs to a different backend".into()));
        }
        let m = Some(m.clone());
        build_args(self.kind, &self.opts, &m)?;
        self.model = m;
        Ok(())
    }

    fn events(&self) -> &Receiver<AgentEvent> {
        &self.rx
    }

    fn close(self: Box<Self>) {
        self.stop_current(true);
        let mut g = self.cur.lock();
        if let Some(mut c) = g.take() {
            if let Some(h) = c.dispatcher.take() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for OneShotSession {
    fn drop(&mut self) {
        // dropped without close(): never leave the child running
        if self.running() {
            self.stop_current(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(p: &mut dyn Proto, lines: &[&str]) -> Vec<AgentEvent> {
        lines.iter().flat_map(|l| p.on_line(&serde_json::from_str(l).unwrap())).collect()
    }

    #[test]
    fn claude_stream_text_tools_usage_settled() {
        let mut p = ClaudeProto::default();
        let evs = run(&mut p, &[
            r#"{"type":"system","subtype":"hook_started"}"#,
            r#"{"type":"system","subtype":"init","cwd":"/x"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start"}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hi"},{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/w/a.rs","old_string":"a","new_string":"b"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Hi","total_cost_usd":0.25,"usage":{"input_tokens":2,"cache_creation_input_tokens":3,"cache_read_input_tokens":4,"output_tokens":5}}"#,
        ]);
        assert_eq!(evs[0], AgentEvent::Started);
        assert_eq!(evs[1], AgentEvent::TextDelta("Hi".into()));
        assert_eq!(evs.iter().filter(|e| matches!(e, AgentEvent::TextDelta(_))).count(), 1, "streamed text is not repeated");
        assert_eq!(evs[2], AgentEvent::ToolStart { id: "t1".into(), name: "Edit".into(), summary: "/w/a.rs".into() });
        assert_eq!(evs[3], AgentEvent::ToolEnd { id: "t1".into(), name: "Edit".into(), ok: true, path: Some("/w/a.rs".into()) });
        assert_eq!(evs[4], AgentEvent::Usage { input: 9, output: 5, cost: Some(0.25) });
        assert_eq!(evs[5], AgentEvent::Settled { ok: true, error: None });
        assert!(p.settled());
    }

    #[test]
    fn claude_error_result_and_subagent_filtering() {
        let mut p = ClaudeProto::default();
        let evs = run(&mut p, &[
            r#"{"type":"assistant","parent_tool_use_id":"x","message":{"content":[{"type":"text","text":"sub"}]}}"#,
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":""}"#,
        ]);
        assert_eq!(evs, vec![AgentEvent::Settled { ok: false, error: Some("error_max_turns".into()) }]);
    }

    #[test]
    fn codex_items_map_and_failures_settle() {
        let mut p = CodexProto::default();
        let evs = run(&mut p, &[
            r#"{"type":"thread.started","thread_id":"t"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"working"}}"#,
            r#"{"type":"item.started","item":{"id":"i1","type":"file_change","changes":[{"path":"/w/a.txt","kind":"add"},{"path":"/w/b.txt","kind":"update"}],"status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"i1","type":"file_change","changes":[{"path":"/w/a.txt","kind":"add"},{"path":"/w/b.txt","kind":"update"}],"status":"completed"}}"#,
            r#"{"type":"item.started","item":{"id":"i2","type":"command_execution","command":"false","status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"i2","type":"command_execution","command":"false","exit_code":1,"status":"failed"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":2}}"#,
        ]);
        assert_eq!(evs[0], AgentEvent::Started);
        assert_eq!(evs[1], AgentEvent::TextDelta("working".into()));
        assert_eq!(evs[2], AgentEvent::ToolStart { id: "i1#0".into(), name: "edit".into(), summary: "add /w/a.txt".into() });
        assert!(evs.contains(&AgentEvent::ToolEnd { id: "i1#1".into(), name: "edit".into(), ok: true, path: Some("/w/b.txt".into()) }));
        assert!(evs.contains(&AgentEvent::ToolEnd { id: "i2".into(), name: "bash".into(), ok: false, path: None }));
        assert_eq!(evs[evs.len() - 1], AgentEvent::Settled { ok: true, error: None });

        let mut p = CodexProto::default();
        let evs = run(&mut p, &[r#"{"type":"turn.failed","error":{"message":"{\"type\":\"error\",\"error\":{\"message\":\"not supported\"}}"}}"#]);
        assert_eq!(evs, vec![AgentEvent::Settled { ok: false, error: Some("not supported".into()) }]);
    }

    #[test]
    fn read_only_args_drop_edit_tools() {
        let o = SessionOpts { cwd: ".".into(), model: None, read_only: true, env_allow: vec![], system_note: None };
        let a: Vec<String> = claude_args(&o, &None).unwrap().iter().map(|x| x.to_string_lossy().into_owned()).collect();
        let i = a.iter().position(|x| x == "--tools").unwrap();
        assert_eq!(a[i + 1], "Read,Grep,Glob");
        assert!(a.contains(&"dontAsk".to_string()) && !a.contains(&"acceptEdits".to_string()));
        let c: Vec<String> = codex_args(&o, &None).unwrap().iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert!(c.windows(2).any(|w| w == ["-s", "read-only"]));
        let mut rw = o.clone();
        rw.read_only = false;
        let c: Vec<String> = codex_args(&rw, &None).unwrap().iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert!(c.windows(2).any(|w| w == ["-s", "workspace-write"]));
    }
}
