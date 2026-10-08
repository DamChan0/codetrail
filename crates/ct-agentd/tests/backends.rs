use ct_agentd::with;
use ct_agentd::*;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

struct Env {
    dir: tempfile::TempDir,
    cfg: Config,
}

impl Env {
    fn new(scenario: &str) -> Env {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("scenario"), scenario).unwrap();
        // auth.json handling runs in node (the Rust side never reads tokens): give the runtime dir a node
        let bin = dir.path().join("data/agent/node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        if let Some(node) = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("node")).find(|c| c.exists())) {
            std::os::unix::fs::symlink(node, bin.join("node")).unwrap();
        }
        let mut cfg = Config::new(dir.path().join("data"));
        cfg.pi_path = Some(fixture("fake-pi"));
        cfg.claude_bin = fixture("fake-claude");
        cfg.codex_bin = fixture("fake-codex");
        cfg.rpc_timeout = Duration::from_secs(10);
        cfg.grace_abort = Duration::from_millis(400);
        cfg.grace_close = Duration::from_millis(400);
        cfg.grace_term = Duration::from_millis(400);
        Env { dir, cfg }
    }
    fn opts(&self, read_only: bool) -> SessionOpts {
        SessionOpts { cwd: self.dir.path().to_path_buf(), model: None, read_only, env_allow: vec![], system_note: None }
    }
    fn auth(&self) -> PathBuf {
        self.cfg.auth_json()
    }
}

/// Collects events until `Exited` (or the deadline).
fn until_exit(s: &dyn AgentSession, secs: u64) -> Vec<AgentEvent> {
    let end = Instant::now() + Duration::from_secs(secs);
    let mut v = Vec::new();
    while let Ok(e) = s.events().recv_timeout(end.saturating_duration_since(Instant::now())) {
        let done = matches!(e, AgentEvent::Exited(_));
        v.push(e);
        if done {
            break;
        }
    }
    v
}

/// Collects events until `Settled`.
fn until_settled(s: &dyn AgentSession, secs: u64) -> Vec<AgentEvent> {
    let end = Instant::now() + Duration::from_secs(secs);
    let mut v = Vec::new();
    while let Ok(e) = s.events().recv_timeout(end.saturating_duration_since(Instant::now())) {
        let done = matches!(e, AgentEvent::Settled { .. });
        v.push(e);
        if done {
            break;
        }
    }
    v
}

fn alive(pid: i32) -> bool {
    std::fs::metadata(format!("/proc/{pid}")).map(|_| true).unwrap_or(false)
        && std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "))
}

// ------------------------------------------------------------------ pi

const PI_RUN: &str = r#"OUT:{"type":"agent_start"}
OUT:{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"a\u2028b\u2029c"}}
OUT:this is not json
ERR:warning: something noisy
OUT:{"type":"tool_execution_start","toolCallId":"c1","toolName":"write","args":{"path":"src/new.rs","content":"x"}}
OUT:{"type":"tool_execution_end","toolCallId":"c1","toolName":"write","isError":false,"result":{}}
OUT:{"type":"message_end","message":{"role":"assistant","usage":{"input":7,"output":3,"cacheRead":0,"cacheWrite":0,"cost":{"total":0.002}}}}
OUT:{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop"}]}"#;

#[test]
fn pi_replays_events_with_unicode_separators_malformed_lines_and_stderr() {
    let env = Env::new(PI_RUN);
    let mut s = with::start_session(&env.cfg, BackendKind::Pi, env.opts(false)).unwrap();
    s.prompt("go").unwrap();
    let evs = until_settled(s.as_ref(), 10);
    s.close();
    assert!(evs.contains(&AgentEvent::Started));
    // U+2028/2029 stay inside the JSON string: one delta, not split
    assert!(evs.contains(&AgentEvent::TextDelta("a\u{2028}b\u{2029}c".into())), "{evs:?}");
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::Stderr(t) if t.contains("unparsable pi output"))));
    assert!(evs.contains(&AgentEvent::Stderr("warning: something noisy".into())));
    assert!(evs.contains(&AgentEvent::ToolEnd { id: "c1".into(), name: "write".into(), ok: true, path: Some("src/new.rs".into()) }));
    assert!(evs.contains(&AgentEvent::Usage { input: 7, output: 3, cost: Some(0.002) }));
    assert_eq!(evs.last(), Some(&AgentEvent::Settled { ok: true, error: None }));
}

#[test]
fn pi_stderr_is_never_protocol() {
    // fake-pi prints a response-shaped JSON line (id ct-1, success:false) on stderr at startup;
    // the handshake request is ct-1 and must still succeed.
    let env = Env::new("");
    let s = with::start_session(&env.cfg, BackendKind::Pi, env.opts(false)).expect("handshake must ignore stderr");
    s.close();
}

#[test]
fn pi_crash_mid_stream_settles_failed_then_exits() {
    let env = Env::new("OUT:{\"type\":\"agent_start\"}\nOUT:{\"type\":\"message_update\",\"assistantMessageEvent\":{\"type\":\"text_delta\",\"delta\":\"x\"}}\nEXIT:3");
    let mut s = with::start_session(&env.cfg, BackendKind::Pi, env.opts(false)).unwrap();
    s.prompt("go").unwrap();
    let evs = until_exit(s.as_ref(), 10);
    assert!(evs.contains(&AgentEvent::TextDelta("x".into())));
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::Settled { ok: false, error: Some(m) } if m.contains("code 3"))), "{evs:?}");
    assert_eq!(evs.last(), Some(&AgentEvent::Exited(Some(3))));
    s.close();
}

#[test]
fn pi_unanswered_request_times_out() {
    let mut env = Env::new("NOREPLY");
    env.cfg.rpc_timeout = Duration::from_millis(500);
    let mut s = with::start_session(&env.cfg, BackendKind::Pi, env.opts(false)).unwrap();
    let t = Instant::now();
    let err = s.prompt("go").unwrap_err();
    assert!(t.elapsed() < Duration::from_secs(5));
    assert!(err.to_string().contains("did not answer"), "{err}");
    s.close();
}

#[test]
fn pi_rejects_set_model_error_and_correlates_ids() {
    let env = Env::new("");
    let mut s = with::start_session(&env.cfg, BackendKind::Pi, env.opts(false)).unwrap();
    let bad = ModelSel { backend: BackendKind::Pi, provider: Some("p".into()), id: "bad".into(), thinking: None };
    assert!(s.set_model(&bad).unwrap_err().to_string().contains("Model not found"));
    let good = ModelSel { backend: BackendKind::Pi, provider: None, id: "m1".into(), thinking: Some("high".into()) };
    s.set_model(&good).expect("provider resolved via get_available_models");
    s.steer("x").unwrap();
    s.follow_up("y").unwrap();
    s.close();
}

#[test]
fn pi_drains_stdout_without_a_reader() {
    // far more output than a pipe buffer holds, consumed only after the run finished
    let mut sc = String::from("OUT:{\"type\":\"agent_start\"}\n");
    for _ in 0..20000 {
        sc.push_str("OUT:{\"type\":\"message_update\",\"assistantMessageEvent\":{\"type\":\"text_delta\",\"delta\":\"0123456789abcdef0123456789abcdef\"}}\n");
    }
    sc.push_str("OUT:{\"type\":\"agent_end\",\"messages\":[]}");
    let env = Env::new(&sc);
    let mut s = with::start_session(&env.cfg, BackendKind::Pi, env.opts(false)).unwrap();
    s.prompt("go").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let evs = until_settled(s.as_ref(), 20);
    s.close();
    let chars: usize = evs.iter().map(|e| if let AgentEvent::TextDelta(t) = e { t.len() } else { 0 }).sum();
    assert_eq!(chars, 20000 * 32, "text coalesced while nobody reads, none lost");
}

#[test]
fn pi_abort_is_fire_and_forget_even_if_pi_never_answers() {
    let mut env = Env::new("NOABORT\nOUT:{\"type\":\"agent_start\"}");
    env.cfg.rpc_timeout = Duration::from_secs(30);
    let mut s = with::start_session(&env.cfg, BackendKind::Pi, env.opts(false)).unwrap();
    s.prompt("go").unwrap();
    let t = Instant::now();
    s.abort().unwrap();
    assert!(t.elapsed() < Duration::from_millis(100), "abort blocked for {:?}", t.elapsed());
    s.close();
}

#[test]
fn pi_slow_consumer_gets_all_text_and_the_terminal_event() {
    let mut sc = String::from("OUT:{\"type\":\"agent_start\"}\n");
    sc.push_str("REPEAT:200000:{\"type\":\"message_update\",\"assistantMessageEvent\":{\"type\":\"text_delta\",\"delta\":\"abcd\"}}\n");
    sc.push_str("ERR:late stderr\nOUT:{\"type\":\"agent_end\",\"messages\":[]}");
    let env = Env::new(&sc);
    let mut s = with::start_session(&env.cfg, BackendKind::Pi, env.opts(false)).unwrap();
    s.prompt("go").unwrap();
    let mut chars = 0usize;
    let mut events = 0usize;
    let mut settled = false;
    let end = Instant::now() + Duration::from_secs(60);
    while let Ok(e) = s.events().recv_timeout(end.saturating_duration_since(Instant::now())) {
        events += 1;
        if events % 50 == 0 {
            std::thread::sleep(Duration::from_millis(2)); // slow UI
        }
        match e {
            AgentEvent::TextDelta(t) => chars += t.len(),
            AgentEvent::Settled { ok, .. } => {
                settled = true;
                assert!(ok);
                break;
            }
            _ => {}
        }
    }
    s.close();
    assert_eq!(chars, 200_000 * 4, "no text lost");
    assert!(settled, "terminal event lost");
    assert!(events < 200_000, "deltas were coalesced under lag ({events} events)");
}

#[test]
fn pi_close_with_unread_events_does_not_hang() {
    let mut sc = String::from("OUT:{\"type\":\"agent_start\"}\n");
    sc.push_str("REPEAT:50000:{\"type\":\"tool_execution_start\",\"toolCallId\":\"c\",\"toolName\":\"bash\",\"args\":{}}\n");
    let env = Env::new(&sc);
    let mut s = with::start_session(&env.cfg, BackendKind::Pi, env.opts(false)).unwrap();
    s.prompt("go").unwrap();
    std::thread::sleep(Duration::from_millis(500)); // queue full, dispatcher blocked
    let t = Instant::now();
    s.close();
    assert!(t.elapsed() < Duration::from_secs(8), "{:?}", t.elapsed());
}

#[test]
fn pi_close_escalates_to_sigkill_without_orphans() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("pid");
    let env = Env::new(&format!("IGNORE_TERM\nPIDFILE:{}", pidfile.display()));
    let s = with::start_session(&env.cfg, BackendKind::Pi, env.opts(false)).unwrap();
    let pid = s.pid().unwrap() as i32;
    assert_eq!(std::fs::read_to_string(&pidfile).unwrap().trim(), pid.to_string());
    assert!(alive(pid));
    let t = Instant::now();
    s.close();
    assert!(!alive(pid), "pi survived close()");
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

#[test]
fn pi_list_models_and_empty_when_unlogged_shape() {
    let env = Env::new("");
    let ms = with::list_models(&env.cfg, BackendKind::Pi).unwrap();
    assert_eq!(
        ms,
        vec![ModelInfo { backend: BackendKind::Pi, provider: "p".into(), id: "m1".into(), name: "Model One".into(), reasoning: true, context_window: 1000 }]
    );
}

// ------------------------------------------------------------------ claude / codex

fn argv(env: &Env) -> Vec<String> {
    serde_json::from_str(&std::fs::read_to_string(env.dir.path().join("argv.json")).unwrap()).unwrap()
}

#[test]
fn claude_contract_read_only_streams_and_exits() {
    let env = Env::new("");
    let mut o = env.opts(true);
    o.model = Some(ModelSel { backend: BackendKind::Claude, provider: None, id: "haiku".into(), thinking: None });
    let mut s = with::start_session(&env.cfg, BackendKind::Claude, o).unwrap();
    s.prompt("reply with the single word ok").unwrap();
    let evs = until_exit(s.as_ref(), 10);
    s.close();
    assert_eq!(evs.first(), Some(&AgentEvent::Started));
    assert!(evs.contains(&AgentEvent::TextDelta("ok".into())));
    assert!(evs.contains(&AgentEvent::Usage { input: 3, output: 1, cost: Some(0.01) }));
    assert!(evs.contains(&AgentEvent::Settled { ok: true, error: None }));
    assert_eq!(evs.last(), Some(&AgentEvent::Exited(Some(0))));
    let a = argv(&env);
    let i = a.iter().position(|x| x == "--tools").unwrap();
    assert_eq!(a[i + 1], "Read,Grep,Glob");
    assert!(a.windows(2).any(|w| w == ["--model", "haiku"]));
    assert!(!a.iter().any(|x| x == "acceptEdits" || x == "bypassPermissions"));
    assert!(!a.iter().any(|x| x.contains("reply with")), "prompt must not be on argv");
}

#[test]
fn claude_failure_exit_settles_failed_with_masked_stderr() {
    let env = Env::new("ERR:auth failed token=sk-abcdefghijklmnopqrstuv\nEXIT:2");
    let mut s = with::start_session(&env.cfg, BackendKind::Claude, env.opts(false)).unwrap();
    s.prompt("x").unwrap();
    let evs = until_exit(s.as_ref(), 10);
    s.close();
    let settled = evs.iter().find_map(|e| if let AgentEvent::Settled { ok, error } = e { Some((*ok, error.clone())) } else { None }).unwrap();
    assert!(!settled.0);
    let msg = settled.1.unwrap();
    assert!(msg.contains("code 2") && !msg.contains("abcdefghijklmnopqrstuv"), "{msg}");
    for e in &evs {
        if let AgentEvent::Stderr(t) = e {
            assert!(!t.contains("abcdefghijklmnopqrstuv"));
        }
    }
}

#[test]
fn oneshot_second_prompt_while_running_is_rejected_and_abort_terminates() {
    let env = Env::new("OUT:{\"type\":\"system\",\"subtype\":\"init\"}\nSLEEP:30");
    let mut s = with::start_session(&env.cfg, BackendKind::Claude, env.opts(false)).unwrap();
    s.prompt("one").unwrap();
    assert!(s.prompt("two").is_err());
    let pid = s.pid().unwrap() as i32;
    s.abort().unwrap();
    let evs = until_exit(s.as_ref(), 10);
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::Settled { ok: false, error: Some(m) } if m == "aborted")), "{evs:?}");
    assert!(!alive(pid));
    // a finished session accepts the next prompt
    std::fs::write(env.dir.path().join("scenario"), "").unwrap();
    s.prompt("three").unwrap();
    assert!(until_exit(s.as_ref(), 10).contains(&AgentEvent::Settled { ok: true, error: None }));
    s.close();
}

#[test]
fn codex_contract_models_and_args() {
    let env = Env::new("");
    let ms = with::list_models(&env.cfg, BackendKind::Codex).unwrap();
    assert_eq!(ms.len(), 1, "hidden models are not listed");
    assert_eq!((ms[0].id.as_str(), ms[0].context_window, ms[0].reasoning), ("gx-1", 4000, true));

    let mut o = env.opts(true);
    o.model = Some(ModelSel { backend: BackendKind::Codex, provider: None, id: "gx-1".into(), thinking: None });
    let mut s = with::start_session(&env.cfg, BackendKind::Codex, o).unwrap();
    s.prompt("ok?").unwrap();
    let evs = until_exit(s.as_ref(), 10);
    s.close();
    assert!(evs.contains(&AgentEvent::TextDelta("ok".into())));
    assert!(evs.contains(&AgentEvent::Settled { ok: true, error: None }));
    assert_eq!(evs.last(), Some(&AgentEvent::Exited(Some(0))));
    let a = argv(&env);
    assert!(a.windows(2).any(|w| w == ["-s", "read-only"]));
    assert!(a.windows(2).any(|w| w == ["-m", "gx-1"]));
    assert!(a.contains(&"--json".to_string()));
}

#[test]
fn oneshot_accepts_a_prompt_right_after_settled_or_exited() {
    for (kind, n) in [(BackendKind::Claude, 200), (BackendKind::Codex, 100)] {
        let env = Env::new("");
        let mut s = with::start_session(&env.cfg, kind, env.opts(false)).unwrap();
        for i in 0..n {
            // react to Settled by prompting at once (the process may still be exiting)
            s.prompt("go").unwrap_or_else(|e| panic!("{kind:?} iteration {i}: {e}"));
            let end = Instant::now() + Duration::from_secs(10);
            loop {
                let e = s.events().recv_timeout(end.saturating_duration_since(Instant::now())).expect("Settled");
                if matches!(e, AgentEvent::Settled { ok: true, .. }) {
                    break;
                }
            }
            if i % 2 == 0 {
                // and the other way: wait for Exited too
                loop {
                    if matches!(s.events().recv_timeout(Duration::from_secs(10)).unwrap(), AgentEvent::Exited(_)) {
                        break;
                    }
                }
            } else {
                // leave Exited unread: it must not be mistaken for the next run's events
                s.prompt("again").unwrap_or_else(|e| panic!("{kind:?} iteration {i} (after Settled): {e}"));
                let mut exits = 0;
                while exits < 2 {
                    if matches!(s.events().recv_timeout(Duration::from_secs(10)).unwrap(), AgentEvent::Exited(_)) {
                        exits += 1;
                    }
                }
            }
        }
        s.close();
    }
}

#[test]
fn unsupported_operations_are_errors() {
    let env = Env::new("");
    let mut s = with::start_session(&env.cfg, BackendKind::Codex, env.opts(false)).unwrap();
    assert!(matches!(s.steer("x"), Err(Error::Unsupported(_))));
    assert!(matches!(s.follow_up("x"), Err(Error::Unsupported(_))));
    s.close();
}

#[test]
fn capabilities_matrix() {
    let c = |b| capabilities(b);
    let pi = c(BackendKind::Pi);
    assert!(pi.list_models && pi.steer && pi.follow_up && pi.abort && pi.thinking_level && pi.streaming_tools && pi.persistent_session);
    for b in [BackendKind::Claude, BackendKind::Codex] {
        let x = c(b);
        assert!(x.list_models && x.abort && x.streaming_tools && x.thinking_level);
        assert!(!x.steer && !x.follow_up && !x.persistent_session, "{b:?}");
    }
}

// ------------------------------------------------------------------ accounts

#[test]
fn accounts_shape_and_states() {
    let env = Env::new("");
    let a = with::accounts(&env.cfg);
    let ids: Vec<&str> = a.iter().map(|x| x.id.as_str()).collect();
    assert_eq!(ids, ["openai-codex", "github-copilot", "claude", "codex"]);
    assert!(!ids.contains(&"anthropic"));
    for x in &a[..2] {
        assert_eq!(x.login, LoginKind::InApp);
        assert_eq!(x.state, AccountState::LoggedOut);
    }
    assert_eq!(a[2].login, LoginKind::ExternalCli { command: "claude auth login".into() });
    assert_eq!(a[3].login, LoginKind::ExternalCli { command: "codex login".into() });
    assert_eq!(a[2].state, AccountState::LoggedIn { method: "claude.ai".into() });
    assert_eq!(a[3].state, AccountState::LoggedIn { method: "ChatGPT".into() });
}

#[test]
fn missing_cli_is_unavailable_and_kill_switch_disables() {
    let mut env = Env::new("");
    env.cfg.claude_bin = "/nonexistent/claude".into();
    env.cfg.accounts_disabled = vec!["codex".into()];
    let a = with::accounts(&env.cfg);
    assert!(matches!(&a[2].state, AccountState::Unavailable { reason } if reason.contains("not installed")));
    assert!(matches!(&a[3].state, AccountState::Unavailable { reason } if reason.contains("disabled")));
    assert!(with::login_start(&env.cfg, "claude").is_err(), "claude/codex login is never in-app");
    assert!(with::login_start(&env.cfg, "anthropic").is_err());
}

fn write_auth(env: &Env, body: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(env.auth().parent().unwrap()).unwrap();
    std::fs::write(env.auth(), body).unwrap();
    std::fs::set_permissions(env.auth(), std::fs::Permissions::from_mode(mode)).unwrap();
}

const AUTH: &str = r#"{"openai-codex":{"type":"oauth","access":"SECRET-ACCESS-AAAA","refresh":"SECRET-REFRESH-BBBB","expires":1},"github-copilot":{"type":"oauth","access":"SECRET-GH-CCCC","refresh":"SECRET-GH-DDDD","expires":2}}"#;

#[test]
fn auth_state_reads_keys_only_and_logout_removes_only_that_provider() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("");
    write_auth(&env, AUTH, 0o644);
    let a = with::accounts(&env.cfg);
    assert_eq!(a[0].state, AccountState::LoggedIn { method: "oauth".into() });
    assert_eq!(a[1].state, AccountState::LoggedIn { method: "oauth".into() });
    assert!(!format!("{a:?}").contains("SECRET"), "token values must never surface");
    assert_eq!(std::fs::metadata(env.auth()).unwrap().permissions().mode() & 0o777, 0o600, "loose perms are tightened");

    with::logout(&env.cfg, "openai-codex").unwrap();
    let after = std::fs::read_to_string(env.auth()).unwrap();
    assert!(!after.contains("openai-codex") && !after.contains("SECRET-ACCESS-AAAA"));
    let v: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(v["github-copilot"]["access"], "SECRET-GH-CCCC", "other provider untouched");
    assert_eq!(std::fs::metadata(env.auth()).unwrap().permissions().mode() & 0o777, 0o600);
    let a = with::accounts(&env.cfg);
    assert_eq!(a[0].state, AccountState::LoggedOut);
    assert!(matches!(a[1].state, AccountState::LoggedIn { .. }));
    // idempotent; external accounts cannot be logged out here
    with::logout(&env.cfg, "openai-codex").unwrap();
    assert!(with::logout(&env.cfg, "claude").is_err());
}

#[test]
fn sentinel_tokens_never_reach_rust_values_or_errors() {
    let env = Env::new("");
    write_auth(&env, AUTH, 0o600);
    let mut seen = format!("{:?}", with::accounts(&env.cfg));
    with::logout(&env.cfg, "github-copilot").unwrap();
    // corrupt file containing a sentinel: the error/reason text must not echo content
    write_auth(&env, "{ SECRET-ACCESS-AAAA", 0o600);
    seen.push_str(&format!("{:?}", with::accounts(&env.cfg)));
    seen.push_str(&format!("{:?}", with::logout(&env.cfg, "openai-codex")));
    assert!(!seen.contains("SECRET"), "{seen}");
}

#[test]
fn corrupt_auth_json_is_unavailable_and_untouched() {
    let env = Env::new("");
    write_auth(&env, "{ not json SECRET", 0o600);
    assert!(matches!(with::accounts(&env.cfg)[0].state, AccountState::Unavailable { .. }));
    assert!(with::logout(&env.cfg, "openai-codex").is_err());
    assert_eq!(std::fs::read_to_string(env.auth()).unwrap(), "{ not json SECRET");
}

#[test]
fn runtime_status_with_custom_pi_path() {
    let env = Env::new("");
    let st = with::runtime_status(&env.cfg);
    assert!(st.installed && st.pi.unwrap().starts_with("custom:"));
    let mut c = env.cfg.clone();
    c.pi_path = None;
    let st = with::runtime_status(&c);
    assert!(!st.installed && st.node.is_none() && st.pi.is_none());
    assert!(with::start_session(&c, BackendKind::Pi, env.opts(false)).is_err());
}

// ------------------------------------------------------------------ live (opt-in)

fn live_prompt(b: BackendKind, model: Option<&str>) -> Vec<AgentEvent> {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::current();
    let o = SessionOpts {
        cwd: dir.path().to_path_buf(),
        model: model.map(|m| ModelSel { backend: b, provider: None, id: m.into(), thinking: None }),
        read_only: true,
        env_allow: vec![],
        system_note: None,
    };
    let mut s = with::start_session(&cfg, b, o).unwrap();
    s.prompt("reply with the single word ok").unwrap();
    let evs = until_exit(s.as_ref(), 180);
    s.close();
    evs
}

#[test]
fn live_claude_canary() {
    if std::env::var("CT_LIVE").as_deref() != Ok("1") {
        return;
    }
    let evs = live_prompt(BackendKind::Claude, Some("haiku"));
    eprintln!("claude canary: {evs:?}");
    let text: String = evs.iter().filter_map(|e| if let AgentEvent::TextDelta(t) = e { Some(t.as_str()) } else { None }).collect();
    assert!(text.to_lowercase().contains("ok"), "{text:?}");
    assert!(evs.contains(&AgentEvent::Settled { ok: true, error: None }), "{evs:?}");
    assert_eq!(evs.last(), Some(&AgentEvent::Exited(Some(0))));
}

#[test]
fn live_codex_canary() {
    if std::env::var("CT_LIVE").as_deref() != Ok("1") {
        return;
    }
    let evs = live_prompt(BackendKind::Codex, None);
    eprintln!("codex canary: {evs:?}");
    let text: String = evs.iter().filter_map(|e| if let AgentEvent::TextDelta(t) = e { Some(t.as_str()) } else { None }).collect();
    assert!(text.to_lowercase().contains("ok"), "{text:?}");
    assert!(evs.contains(&AgentEvent::Settled { ok: true, error: None }), "{evs:?}");
    assert_eq!(evs.last(), Some(&AgentEvent::Exited(Some(0))));
}

/// Real pi from the app-private runtime (`CT_DATA_DIR`): unlogged => no models; clean shutdown.
#[test]
fn live_pi_smoke() {
    if std::env::var("CT_LIVE_PI").as_deref() != Ok("1") {
        return;
    }
    let cfg = Config::current();
    assert!(with::runtime_status(&cfg).installed, "run runtime_setup into CT_DATA_DIR first");
    let auth_present = std::fs::read_to_string(cfg.auth_json()).is_ok_and(|t| t.trim().len() > 2);
    let models = with::list_models(&cfg, BackendKind::Pi).unwrap();
    if !auth_present {
        assert_eq!(models, vec![], "unlogged pi must offer no models");
    }
    let dir = tempfile::tempdir().unwrap();
    let o = SessionOpts { cwd: dir.path().to_path_buf(), model: None, read_only: true, env_allow: vec![], system_note: None };
    let s = with::start_session(&cfg, BackendKind::Pi, o).unwrap();
    let pid = s.pid().unwrap() as i32;
    assert!(alive(pid));
    s.close();
    std::thread::sleep(Duration::from_millis(200));
    assert!(!alive(pid), "pi orphaned after close()");
}

/// Real `npm ci` into `CT_DATA_DIR`; the second call must be a fast no-op.
#[test]
fn live_runtime_setup() {
    if std::env::var("CT_LIVE_SETUP").as_deref() != Ok("1") {
        return;
    }
    let lines = std::sync::Mutex::new(Vec::<String>::new());
    runtime_setup(&|l| lines.lock().unwrap().push(l.to_string())).unwrap();
    let st = runtime_status();
    eprintln!("runtime status: {st:?}");
    assert!(st.installed && st.node.is_some() && st.pi.is_some());
    let t = Instant::now();
    runtime_setup(&|_| {}).unwrap();
    assert!(t.elapsed() < Duration::from_secs(2), "second setup must be a no-op");
}

/// Real login helper (openai-codex): must surface an authorize URL, then cancel cleanly and leave auth.json alone.
#[test]
fn live_login_helper_url_and_cancel() {
    if std::env::var("CT_LIVE_PI").as_deref() != Ok("1") {
        return;
    }
    let h = login_start("openai-codex").unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    let mut url = None;
    while let Ok(e) = h.events.recv_timeout(end.saturating_duration_since(Instant::now())) {
        if let LoginEvent::OpenUrl(u) = e {
            url = Some(u);
            break;
        }
    }
    let url = url.expect("helper never produced a URL");
    assert!(url.starts_with("https://"), "{url}");
    h.cancel();
    assert!(!Config::current().auth_json().exists() || !std::fs::read_to_string(Config::current().auth_json()).unwrap().contains("openai-codex"));
}
