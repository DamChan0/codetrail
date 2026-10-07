use ct_agent::ask::{self, AgentKind, RunOpts, RunStatus};
use ct_agent::hook::handle_hook;
use ct_agent::install::{self, Change, InstallOpts};
use ct_store::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ct-agent");

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_DATE", "2024-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2024-01-01T00:00:00Z")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    git(d.path(), &["init", "-q", "-b", "main"]);
    fs::write(d.path().join("f.txt"), "a\nb\nc\nd\ne\n").unwrap();
    git(d.path(), &["add", "-A"]);
    git(d.path(), &["commit", "-q", "-m", "base"]);
    d
}

fn cli(dir: &Path, _home: &Path, args: &[&str]) -> Output {
    Command::new(BIN).current_dir(dir).args(args).output().unwrap()
}
fn s(o: &[u8]) -> String {
    String::from_utf8_lossy(o).into_owned()
}

fn git_dir(d: &Path) -> PathBuf {
    d.canonicalize().unwrap().join(".git")
}

// ------------------------------------------------------------ install

fn opts(home: &Path, dry: bool) -> InstallOpts {
    InstallOpts { claude: true, omp: true, codex: true, dry_run: dry, home: home.into(), bin: "codetrail".into() }
}

fn baks(dir: &Path) -> usize {
    fs::read_dir(dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains("codetrail-bak")).count()
}

#[test]
fn install_dry_run_writes_nothing() {
    let h = tempfile::tempdir().unwrap();
    let acts = install::install(&opts(h.path(), true)).unwrap();
    assert!(acts.iter().all(|a| a.change == Change::Created));
    assert_eq!(fs::read_dir(h.path()).unwrap().count(), 0);
}

#[test]
fn install_merges_backs_up_and_is_idempotent() {
    let h = tempfile::tempdir().unwrap();
    let claude = h.path().join(".claude");
    fs::create_dir_all(&claude).unwrap();
    let existing = json!({
        "model": "opus",
        "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "other-tool stop"}]}],
                  "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "x"}]}]}
    });
    fs::write(claude.join("settings.json"), serde_json::to_string_pretty(&existing).unwrap()).unwrap();
    fs::create_dir_all(h.path().join(".codex")).unwrap();
    fs::write(h.path().join(".codex/AGENTS.md"), "# my rules\nkeep me\n").unwrap();

    let acts = install::install(&opts(h.path(), false)).unwrap();
    assert!(acts.iter().any(|a| a.backup.is_some()), "existing files are backed up");
    assert_eq!(baks(&claude), 1);
    let v: Value = serde_json::from_str(&fs::read_to_string(claude.join("settings.json")).unwrap()).unwrap();
    assert_eq!(v["model"], "opus", "unrelated keys preserved");
    assert_eq!(v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"], "x");
    let stop = v["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 2, "foreign Stop hook kept, ours appended");
    assert_eq!(stop[1]["hooks"][0]["command"], "codetrail hook claude Stop");
    assert_eq!(stop[1]["hooks"][0]["timeout"], 5);
    for ev in ["PreToolUse", "PostToolUse", "PostToolUseFailure"] {
        assert_eq!(v["hooks"][ev][0]["matcher"], "Edit|Write|MultiEdit|NotebookEdit");
        assert_eq!(v["hooks"][ev][0]["hooks"][0]["command"], format!("codetrail hook claude {ev}"));
    }
    assert!(h.path().join(".claude/skills/codetrail/SKILL.md").exists());
    assert!(h.path().join(".agents/skills/codetrail/SKILL.md").exists());
    let agents = fs::read_to_string(h.path().join(".codex/AGENTS.md")).unwrap();
    assert!(agents.starts_with("# my rules\nkeep me\n"));
    assert_eq!(agents.matches(install::CODEX_BEGIN).count(), 1);

    // second run: nothing changes, no new backups
    let before = fs::read_to_string(claude.join("settings.json")).unwrap();
    let acts = install::install(&opts(h.path(), false)).unwrap();
    assert!(acts.iter().all(|a| a.change == Change::Unchanged), "{acts:?}");
    assert_eq!(fs::read_to_string(claude.join("settings.json")).unwrap(), before);
    assert_eq!(baks(&claude), 1);
    assert_eq!(fs::read_to_string(h.path().join(".codex/AGENTS.md")).unwrap(), agents);
}

#[test]
fn install_refuses_invalid_settings_json() {
    let h = tempfile::tempdir().unwrap();
    fs::create_dir_all(h.path().join(".claude")).unwrap();
    fs::write(h.path().join(".claude/settings.json"), "{ not json").unwrap();
    assert!(install::install(&opts(h.path(), false)).is_err());
    assert_eq!(fs::read_to_string(h.path().join(".claude/settings.json")).unwrap(), "{ not json");
}

#[test]
fn codex_block_is_replaced_not_duplicated() {
    let old = format!("top\n\n{}\nSTALE\n{}\nbottom\n", install::CODEX_BEGIN, install::CODEX_END);
    let new = install::merge_codex_agents(&old);
    assert!(new.starts_with("top\n\n") && new.ends_with("bottom\n"));
    assert!(!new.contains("STALE"));
    assert_eq!(new.matches(install::CODEX_BEGIN).count(), 1);
}

// ------------------------------------------------------------ hooks

fn ev(event: &str, cwd: &Path, extra: Value) -> String {
    let mut v = json!({"session_id":"sess1","transcript_path":"/nonexistent/t.jsonl","cwd":cwd,"hook_event_name":event});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v.to_string()
}

#[test]
fn hook_pre_post_failure_stop_fixtures() {
    let d = repo();
    let root = d.path().canonicalize().unwrap();
    let f = root.join("f.txt");
    let ti = json!({"file_path": f, "old_string":"b","new_string":"B1\nB2"});

    // Pre: snapshot only, no output
    assert!(handle_hook("PreToolUse", &ev("PreToolUse", &root, json!({"tool_name":"Edit","tool_input":ti,"tool_use_id":"toolu_1"}))).is_none());
    assert!(git_dir(&root).join("codetrail/pending/toolu_1").exists());

    // tool runs
    fs::write(&f, "a\nB1\nB2\nc\nd\ne\n").unwrap();
    let out = handle_hook("PostToolUse", &ev("PostToolUse", &root, json!({"tool_name":"Edit","tool_input":ti,"tool_response":{},"tool_use_id":"toolu_1"}))).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    let ctx = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(ctx.contains("codetrail note --record ") && ctx.contains("--reason"), "{ctx}");
    assert!(!git_dir(&root).join("codetrail/pending/toolu_1").exists(), "pending consumed");

    let store = Store::open(&git_dir(&root)).unwrap();
    let recs = store.for_path("f.txt");
    assert_eq!(recs.len(), 1);
    let r = &recs[0];
    assert!(ctx.contains(&fmt_id(r.id)));
    assert_eq!(r.agent, Agent::Claude);
    assert_eq!(r.session, "sess1");
    assert_eq!(r.tool_use_id, "toolu_1");
    assert_eq!(r.head_at_edit, git(&root, &["rev-parse", "HEAD"]));
    assert_eq!(r.pre_blob.as_deref(), Some(git(&root, &["rev-parse", "HEAD:f.txt"]).as_str()));
    assert_eq!(r.post_blob.as_deref(), Some(git(&root, &["hash-object", "f.txt"]).as_str()));
    assert_eq!(r.spans.len(), 1);
    assert_eq!((r.spans[0].new_start, r.spans[0].new_len), (2, 2));
    assert_eq!(r.spans[0].fingerprint, fingerprint_lines(&["B1", "B2"]));
    assert_eq!(r.transcript.as_ref().unwrap().path, "/nonexistent/t.jsonl");

    // a committed result links High, and note attaches the reason
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "c"]);
    let blob = git(&root, &["rev-parse", "HEAD:f.txt"]);
    let m = store.match_hunk("f.txt", &["B1".into(), "B2".into()], &[blob]);
    assert_eq!(best_confidence(&m), Confidence::High);

    // Failure: pending removed, no record
    handle_hook("PreToolUse", &ev("PreToolUse", &root, json!({"tool_name":"Write","tool_input":{"file_path":f,"content":"x"},"tool_use_id":"toolu_2"})));
    assert!(git_dir(&root).join("codetrail/pending/toolu_2").exists());
    assert!(handle_hook("PostToolUseFailure", &ev("PostToolUseFailure", &root, json!({"tool_name":"Write","tool_input":{"file_path":f},"tool_use_id":"toolu_2","error":"boom"}))).is_none());
    assert!(!git_dir(&root).join("codetrail/pending/toolu_2").exists());
    assert_eq!(Store::open(&git_dir(&root)).unwrap().for_path("f.txt").len(), 1);

    // no-op edit (file unchanged) -> no record
    handle_hook("PreToolUse", &ev("PreToolUse", &root, json!({"tool_name":"Write","tool_input":{"file_path":f},"tool_use_id":"toolu_3"})));
    assert!(handle_hook("PostToolUse", &ev("PostToolUse", &root, json!({"tool_name":"Write","tool_input":{"file_path":f},"tool_use_id":"toolu_3"}))).is_none());

    // Stop: Bash-made change (not covered by an Edit record) -> Unattributed, once
    fs::write(root.join("g.txt"), "made by bash\n").unwrap();
    fs::write(&f, "a\nB1\nB2\nc\nd\ne\nbash-added\n").unwrap();
    handle_hook("Stop", &ev("Stop", &root, json!({"stop_hook_active":false})));
    handle_hook("Stop", &ev("Stop", &root, json!({"stop_hook_active":false})));
    let store = Store::open(&git_dir(&root)).unwrap();
    let ua = store.for_path("g.txt");
    assert_eq!(ua.len(), 1, "idempotent across repeated Stop");
    assert_eq!(ua[0].kind, Kind::Unattributed);
    let uf: Vec<_> = store.for_path("f.txt").into_iter().filter(|r| r.kind == Kind::Unattributed).collect();
    assert_eq!(uf.len(), 1);
    assert_eq!(uf[0].spans[0].fingerprint, fingerprint_lines(&["bash-added"]));
    // path covered by an Edit record is not duplicated: revert f.txt to the Edit post state
    git(&root, &["checkout", "-q", "--", "f.txt"]);
}

#[test]
fn hook_non_git_and_garbage_are_silent() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("x.txt");
    fs::write(&f, "1").unwrap();
    let start = Instant::now();
    assert!(handle_hook("PostToolUse", &ev("PostToolUse", d.path(), json!({"tool_name":"Edit","tool_input":{"file_path":f},"tool_use_id":"t"}))).is_none());
    assert!(handle_hook("PreToolUse", "not json").is_none());
    assert!(start.elapsed() < Duration::from_millis(200));
    // CLI: always exit 0
    let o = Command::new(BIN).args(["hook", "claude", "PostToolUse"]).current_dir(d.path()).output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert!(o.stdout.is_empty());
}

#[test]
fn hook_error_goes_to_errors_log_and_exits_zero() {
    let d = repo();
    let root = d.path().canonicalize().unwrap();
    // make the log un-appendable: a directory where log.ct must be
    fs::create_dir_all(git_dir(&root).join("codetrail/log.ct")).unwrap();
    let f = root.join("f.txt");
    let ti = json!({"file_path": f});
    handle_hook("PreToolUse", &ev("PreToolUse", &root, json!({"tool_name":"Write","tool_input":ti,"tool_use_id":"t9"})));
    fs::write(&f, "changed\n").unwrap();
    let inp = ev("PostToolUse", &root, json!({"tool_name":"Write","tool_input":ti,"tool_use_id":"t9"}));
    let mut child = Command::new(BIN)
        .args(["hook", "claude", "PostToolUse"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(inp.as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert!(o.stdout.is_empty() && o.stderr.is_empty());
    let log = fs::read_to_string(git_dir(&root).join("codetrail/errors.log")).unwrap();
    assert!(log.contains("posttooluse"), "{log}");
}

// ------------------------------------------------------------ CLI: record / note / export

#[test]
fn record_note_export_linkage() {
    let d = repo();
    let h = tempfile::tempdir().unwrap();
    let root = d.path().canonicalize().unwrap();
    fs::write(root.join("f.txt"), "a\nb\nNEW\nc\nd\ne\n").unwrap();
    let o = cli(&root, h.path(), &["record", "--path", "f.txt", "--agent", "omp", "--session", "S1"]);
    assert!(o.status.success(), "{}", s(&o.stderr));
    let id = s(&o.stdout).trim().strip_prefix("recorded ").unwrap().to_string();
    assert_eq!(id.len(), 32);

    // note requires --record (or session+path)
    let o = cli(&root, h.path(), &["note", "--reason", "x"]);
    assert!(!o.status.success());
    let o = cli(&root, h.path(), &["note", "--record", &id, "--reason", "user asked for NEW line"]);
    assert!(o.status.success(), "{}", s(&o.stderr));
    // fallback: session + path -> most recent
    let o = cli(&root, h.path(), &["note", "--session", "S1", "--path", "f.txt", "--reason", "second thought"]);
    assert!(o.status.success(), "{}", s(&o.stderr));
    // unknown / ambiguous
    assert!(!cli(&root, h.path(), &["note", "--record", "deadbeefdead", "--reason", "x"]).status.success());

    let st = Store::open(&git_dir(&root)).unwrap();
    let r = st.find_id(&id).unwrap();
    assert_eq!(r.agent, Agent::Omp);
    assert_eq!(decode_reason(&r), "second thought");
    assert_eq!((r.spans[0].new_start, r.spans[0].new_len), (3, 1));

    // commit -> High link via post_blob
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "c"]);
    let blob = git(&root, &["rev-parse", "HEAD:f.txt"]);
    assert_eq!(best_confidence(&st.match_hunk("f.txt", &["NEW".into()], &[blob])), Confidence::High);

    // show + export
    let o = cli(&root, h.path(), &["record", &id[..10]]);
    assert!(s(&o.stdout).contains("reason: second thought"));
    let o = cli(&root, h.path(), &["export", "--json"]);
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["records"][0]["reason"], "second thought");
    assert_eq!(v["notes"].as_array().unwrap().len(), 2);

    // ambiguous session+path (two un-annotated records)
    for _ in 0..2 {
        fs::write(root.join("f.txt"), format!("a\nb\nNEW\nc\nd\ne\n{}\n", now_ms())).unwrap();
        assert!(cli(&root, h.path(), &["record", "--path", "f.txt", "--session", "S2"]).status.success());
        std::thread::sleep(Duration::from_millis(3));
    }
    let o = cli(&root, h.path(), &["note", "--session", "S2", "--path", "f.txt", "--reason", "x"]);
    assert!(!o.status.success() && s(&o.stderr).contains("ambiguous"), "{}", s(&o.stderr));
}

// ------------------------------------------------------------ ask

fn ask_fixture() -> (tempfile::TempDir, PathBuf) {
    let d = repo();
    let root = d.path().canonicalize().unwrap();
    fs::write(root.join("f.txt"), "a\nb\nNEW1\nNEW2\nc\nd\ne\n").unwrap();
    let blob = git(&root, &["hash-object", "f.txt"]);
    let st = Store::open(&git_dir(&root)).unwrap();
    let id = 0x0001_8d00_0000_0000_0000_0000_0000_00ab_u128;
    st.append(&Record {
        id,
        ts_ms: 1_700_000_000_000,
        agent: Agent::Claude,
        session: "sess1".into(),
        kind: Kind::Edit,
        tool: "Edit".into(),
        tool_use_id: "toolu_g".into(),
        head_at_edit: git(&root, &["rev-parse", "HEAD"]),
        path: "f.txt".into(),
        pre_blob: None,
        post_blob: Some(blob),
        spans: vec![Span { new_start: 3, new_len: 2, fingerprint: fingerprint_lines(&["NEW1", "NEW2"]), occurrence: 0 }],
        reason: vec![],
        transcript: Some(TranscriptPtr { path: "/t/session.jsonl".into(), offset: 4096 }),
    })
    .unwrap();
    st.append_note(id, Agent::Claude, "sess1", "User asked to add NEW lines for the feature.").unwrap();
    (d, root)
}

#[test]
fn ask_print_golden() {
    let (_d, root) = ask_fixture();
    let h = tempfile::tempdir().unwrap();
    let o = cli(&root, h.path(), &["ask", "f.txt:3-4@worktree", "--print", "--question", "Why NEW?"]);
    assert!(o.status.success(), "{}", s(&o.stderr));
    let got = s(&o.stdout);
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/ask_print.txt");
    if std::env::var("CT_BLESS").is_ok() {
        fs::write(&path, &got).unwrap();
    }
    assert_eq!(got, fs::read_to_string(&path).unwrap());
}

#[test]
fn ask_excludes_secret_sections_by_default() {
    let (_d, root) = ask_fixture();
    let h = tempfile::tempdir().unwrap();
    fs::write(root.join("f.txt"), "a\nb\nAPI_KEY = \"abcdef1234567890xyzXYZ\"\nc\nd\ne\n").unwrap();
    let o = cli(&root, h.path(), &["ask", "f.txt:3-3@worktree", "--print"]);
    let out = s(&o.stdout);
    assert!(!out.contains("abcdef1234567890xyzXYZ"), "secret leaked:\n{out}");
    assert!(s(&o.stderr).contains("possible secret"), "{}", s(&o.stderr));
    let o = cli(&root, h.path(), &["ask", "f.txt:3-3@worktree", "--print", "--include-secrets"]);
    assert!(s(&o.stdout).contains("abcdef1234567890xyzXYZ"));
}

#[test]
fn ask_at_commit_links_reason() {
    let (_d, root) = ask_fixture();
    let h = tempfile::tempdir().unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "feat"]);
    let o = cli(&root, h.path(), &["ask", "f.txt:3-4@HEAD", "--print"]);
    let out = s(&o.stdout);
    assert!(out.contains("[high]") && out.contains("User asked to add NEW lines"), "{out}");
    assert!(out.contains("+NEW1"));
    assert!(out.contains("/t/session.jsonl @ byte 4096"));
}

#[test]
fn code_ref_parse() {
    let r = ask::CodeRef::parse("src/a:b.rs:10-24@abc1234").unwrap();
    assert_eq!((r.path.as_str(), r.start, r.end), ("src/a:b.rs", 10, 24));
    assert_eq!(r.at, ask::RefAt::Commit("abc1234".into()));
    assert_eq!(ask::CodeRef::parse("a.rs:7@worktree").unwrap().at, ask::RefAt::Worktree);
    assert!(ask::CodeRef::parse("a.rs:9-3").is_err());
    assert!(ask::CodeRef::parse("a.rs").is_err());
}

// ------------------------------------------------------------ runners (single test: process-global env)

#[test]
fn runner_streams_times_out_and_cancels() {
    let d = tempfile::tempdir().unwrap();
    let script = |name: &str, body: &str| -> PathBuf {
        let p = d.path().join(name);
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    };
    let opts = |secs: u64| RunOpts { timeout: Duration::from_secs(secs), cancel: Arc::new(AtomicBool::new(false)), cwd: d.path().into() };

    // claude: prompt on stdin, streamed back
    std::env::set_var("CODETRAIL_CLAUDE_BIN", script("echo_stdin", "echo \"args:$*\"; cat"));
    let mut got = String::new();
    let r = ask::run_agent(AgentKind::Claude, "hello 한글", &opts(10), &mut |c| got.push_str(c)).unwrap();
    assert_eq!(r.status, RunStatus::Exited(0));
    assert_eq!(got, "args:-p\nhello 한글");

    // non-zero exit reports stderr
    std::env::set_var("CODETRAIL_CLAUDE_BIN", script("fail", "echo oops >&2; exit 3"));
    let r = ask::run_agent(AgentKind::Claude, "x", &opts(10), &mut |_| {}).unwrap();
    assert_eq!(r.status, RunStatus::Exited(3));
    assert!(r.stderr.contains("oops"));

    // omp: prompt as argv
    std::env::set_var("CODETRAIL_OMP_BIN", script("omp_echo", "echo \"$1|$2\""));
    let mut got = String::new();
    ask::run_agent(AgentKind::Omp, "P", &opts(10), &mut |c| got.push_str(c)).unwrap();
    assert_eq!(got.trim(), "-p|P");

    // timeout kills the child
    std::env::set_var("CODETRAIL_CODEX_BIN", script("slow", "echo start; sleep 30"));
    let t = Instant::now();
    let mut got = String::new();
    let r = ask::run_agent(AgentKind::Codex, "x", &RunOpts { timeout: Duration::from_millis(300), ..opts(1) }, &mut |c| got.push_str(c)).unwrap();
    assert_eq!(r.status, RunStatus::TimedOut);
    assert!(got.contains("start"));
    assert!(t.elapsed() < Duration::from_secs(5));

    // cancel from another thread
    let o = opts(60);
    let flag = o.cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        flag.store(true, Ordering::Relaxed);
    });
    let t = Instant::now();
    let r = ask::run_agent(AgentKind::Codex, "x", &o, &mut |_| {}).unwrap();
    assert_eq!(r.status, RunStatus::Cancelled);
    assert!(t.elapsed() < Duration::from_secs(5));

    // missing binary -> clear error
    std::env::set_var("CODETRAIL_CODEX_BIN", "/nonexistent/codex-bin");
    assert!(ask::run_agent(AgentKind::Codex, "x", &opts(1), &mut |_| {}).is_err());
}
