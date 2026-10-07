//! `codetrail hook claude <event>` - Claude Code hook handler (PLAN §9.4).
//!
//! Verified schema (https://code.claude.com/docs/en/hooks): stdin JSON has `session_id`,
//! `transcript_path`, `cwd`, `hook_event_name`; tool events add `tool_name`, `tool_input`
//! (`file_path` / `notebook_path`), `tool_use_id`; PostToolUse adds `tool_response`.
//! PostToolUse stdout: `{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":".."}}`.
//!
//! Never fails the agent: the caller exits 0 whatever happens; errors go to
//! `<git_dir>/codetrail/errors.log`.

use crate::gitx::{self, RepoCtx};
use ct_store::{fmt_id, new_id, now_ms, Agent, Kind, Record, Store, TranscriptPtr};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];
const MAX_UNATTRIBUTED: usize = 200;

#[derive(Serialize, Deserialize)]
struct Pending {
    path: String,
    pre_blob: Option<String>,
    pre_content: Option<Vec<u8>>,
}

fn norm_event(e: &str) -> String {
    e.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

fn sanitize_id(s: &str) -> Option<String> {
    let ok = !s.is_empty() && s.len() < 128 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    ok.then(|| s.to_string())
}

fn tool_file(v: &Value) -> Option<PathBuf> {
    let ti = v.get("tool_input")?;
    ["file_path", "notebook_path", "path"]
        .iter()
        .find_map(|k| ti.get(*k).and_then(Value::as_str))
        .map(PathBuf::from)
}

/// Returns the stdout payload (if any). Never panics, never returns an error to the caller.
pub fn handle_hook(event: &str, stdin: &str) -> Option<String> {
    let v: Value = serde_json::from_str(stdin).ok()?;
    let cwd = v
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;
    let ev = norm_event(event);
    let probe = if ev == "stop" {
        cwd.clone()
    } else {
        let f = tool_file(&v)?;
        let f = if f.is_absolute() { f } else { cwd.join(f) };
        f.parent().map(Path::to_path_buf).unwrap_or(cwd.clone())
    };
    let ctx = gitx::find_repo(&probe)?; // non-git: exit immediately
    let r = std::panic::catch_unwind(|| run_event(&ctx, &ev, &v, &cwd));
    match r {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            log_error(&ctx, &ev, &e);
            None
        }
        Err(_) => {
            log_error(&ctx, &ev, "panic");
            None
        }
    }
}

pub fn log_error(ctx: &RepoCtx, ev: &str, msg: &str) {
    let dir = ctx.git_dir.join("codetrail");
    let _ = fs::create_dir_all(&dir);
    let p = dir.join("errors.log");
    if fs::metadata(&p).map(|m| m.len() > (1 << 20)).unwrap_or(false) {
        let _ = fs::remove_file(&p);
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{} {ev}: {}", now_ms(), msg.replace('\n', " "));
    }
}

fn run_event(ctx: &RepoCtx, ev: &str, v: &Value, cwd: &Path) -> Result<Option<String>, String> {
    match ev {
        "pretooluse" => pre(ctx, v, cwd).map(|_| None),
        "posttooluse" => post(ctx, v, cwd),
        "posttoolusefailure" => {
            if let Some(id) = v.get("tool_use_id").and_then(Value::as_str).and_then(sanitize_id) {
                let _ = fs::remove_file(pending_path(ctx, &id));
            }
            Ok(None)
        }
        "stop" => stop(ctx, v).map(|_| None),
        other => Err(format!("unknown event {other}")),
    }
}

fn pending_path(ctx: &RepoCtx, id: &str) -> PathBuf {
    ctx.git_dir.join("codetrail").join("pending").join(id)
}

fn tool_target(ctx: &RepoCtx, v: &Value, cwd: &Path) -> Option<(String, PathBuf)> {
    let tool = v.get("tool_name").and_then(Value::as_str)?;
    if !TOOLS.contains(&tool) {
        return None;
    }
    let f = tool_file(v)?;
    let rel = gitx::rel_path(ctx, &f, cwd)?;
    let abs = ctx.root.join(&rel);
    Some((rel, abs))
}

fn pre(ctx: &RepoCtx, v: &Value, cwd: &Path) -> Result<(), String> {
    let Some((rel, abs)) = tool_target(ctx, v, cwd) else { return Ok(()) };
    let Some(id) = v.get("tool_use_id").and_then(Value::as_str).and_then(sanitize_id) else { return Ok(()) };
    let exists = abs.is_file();
    let pend = Pending {
        pre_blob: if exists { gitx::hash_object(&ctx.root, &rel) } else { None },
        pre_content: if exists { gitx::read_text_limited(&abs) } else { Some(Vec::new()) },
        path: rel,
    };
    let p = pending_path(ctx, &id);
    fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
    let bytes = postcard::to_stdvec(&pend).map_err(|e| e.to_string())?;
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

fn transcript_ptr(v: &Value) -> Option<TranscriptPtr> {
    let p = v.get("transcript_path").and_then(Value::as_str)?;
    let off = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    Some(TranscriptPtr { path: p.to_string(), offset: off })
}

pub fn context_message(id: u128) -> String {
    let id = fmt_id(id);
    format!(
        "codetrail record {id} recorded; run: codetrail note --record {id} --reason \"<why, 1-3 sentences, include the user's request intent>\""
    )
}

fn post(ctx: &RepoCtx, v: &Value, cwd: &Path) -> Result<Option<String>, String> {
    let Some((rel, abs)) = tool_target(ctx, v, cwd) else { return Ok(None) };
    let Some(tuid) = v.get("tool_use_id").and_then(Value::as_str).and_then(sanitize_id) else { return Ok(None) };
    let pp = pending_path(ctx, &tuid);
    let pend: Option<Pending> = fs::read(&pp).ok().and_then(|b| postcard::from_bytes(&b).ok());
    let _ = fs::remove_file(&pp);
    let exists = abs.is_file();
    let post_blob = if exists { gitx::hash_object(&ctx.root, &rel) } else { None };
    let pre_blob = pend.as_ref().and_then(|p| p.pre_blob.clone());
    if pend.is_some() && pre_blob == post_blob {
        return Ok(None); // no-op edit
    }
    let spans = if exists {
        match (gitx::read_text_limited(&abs), pend.as_ref()) {
            (Some(post), Some(p)) => match &p.pre_content {
                Some(pre) => gitx::spans_between(pre, &post),
                None => vec![],
            },
            (Some(post), None) => gitx::whole_file_spans(&post), // pending lost: whole file
            _ => vec![],
        }
    } else {
        vec![]
    };
    let ts = now_ms();
    let rec = Record {
        id: new_id(ts),
        ts_ms: ts,
        agent: Agent::Claude,
        session: v.get("session_id").and_then(Value::as_str).unwrap_or("").to_string(),
        kind: Kind::Edit,
        tool: v.get("tool_name").and_then(Value::as_str).unwrap_or("").to_string(),
        tool_use_id: tuid,
        head_at_edit: gitx::head(&ctx.root),
        path: rel,
        pre_blob,
        post_blob,
        spans,
        reason: vec![],
        transcript: transcript_ptr(v),
    };
    Store::open(&ctx.git_dir)
        .and_then(|s| s.append(&rec))
        .map_err(|e| e.to_string())?;
    let out = json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":context_message(rec.id)}});
    Ok(Some(out.to_string()))
}

fn stop(ctx: &RepoCtx, v: &Value) -> Result<(), String> {
    cleanup_pending(ctx);
    let head = gitx::head(&ctx.root);
    let mut paths: Vec<String> = Vec::new();
    let split = |b: Vec<u8>| -> Vec<String> {
        b.split(|c| *c == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect()
    };
    if !head.is_empty() {
        if let Some(b) = gitx::git_out(&ctx.root, &["-c", "core.quotepath=off", "diff", "--name-only", "-z", "HEAD"]) {
            paths.extend(split(b));
        }
    }
    if let Some(b) = gitx::git_out(&ctx.root, &["ls-files", "-o", "--exclude-standard", "-z"]) {
        paths.extend(split(b));
    }
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return Ok(());
    }
    paths.truncate(MAX_UNATTRIBUTED);
    let store = Store::open(&ctx.git_dir).map_err(|e| e.to_string())?;
    let session = v.get("session_id").and_then(Value::as_str).unwrap_or("").to_string();
    for rel in paths {
        let abs = ctx.root.join(&rel);
        let exists = abs.is_file();
        if !exists && !head.is_empty() && gitx::blob_at(&ctx.root, "HEAD", &rel).is_none() {
            continue;
        }
        let cur = if exists { gitx::hash_object(&ctx.root, &rel) } else { None };
        let known = store.for_path(&rel);
        // covered by an Edit record or already recorded as Unattributed with the same blob
        if known.iter().any(|r| r.post_blob == cur) {
            continue;
        }
        let pre_blob = gitx::blob_at(&ctx.root, "HEAD", &rel);
        let spans = if exists {
            let post = gitx::read_text_limited(&abs);
            let pre = match &pre_blob {
                Some(_) => gitx::git_out(&ctx.root, &["show", &format!("HEAD:{rel}")]).filter(|b| b.len() <= gitx::MAX_DIFF_BYTES),
                None => Some(Vec::new()),
            };
            match (pre, post) {
                (Some(a), Some(b)) => gitx::spans_between(&a, &b),
                _ => vec![],
            }
        } else {
            vec![]
        };
        let ts = now_ms();
        let rec = Record {
            id: new_id(ts),
            ts_ms: ts,
            agent: Agent::Claude,
            session: session.clone(),
            kind: Kind::Unattributed,
            tool: "worktree".into(),
            tool_use_id: String::new(),
            head_at_edit: head.clone(),
            path: rel,
            pre_blob,
            post_blob: cur,
            spans,
            reason: vec![],
            transcript: transcript_ptr(v),
        };
        store.append(&rec).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn cleanup_pending(ctx: &RepoCtx) {
    let dir = ctx.git_dir.join("codetrail").join("pending");
    let Ok(rd) = fs::read_dir(&dir) else { return };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 3600);
    for e in rd.flatten() {
        if e.metadata().and_then(|m| m.modified()).map(|t| t < cutoff).unwrap_or(false) {
            let _ = fs::remove_file(e.path());
        }
    }
}
