//! CLI handlers. `run_cli(args)` takes the arguments AFTER the program name (`install ...`, `hook claude Stop`, ...).

use crate::ask::{self, AgentKind, AskRequest, CodeRef, Include, RunOpts, RunStatus};
use crate::gitx::{self, RepoCtx};
use crate::hook;
use crate::install::{self, InstallOpts};
use anyhow::{anyhow, bail, Result};
use ct_store::{decode_reason, fmt_id, new_id, now_ms, Agent, Kind, Record, Store};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

pub const SUBCOMMANDS: &[&str] = &["install", "hook", "note", "record", "ask", "export"];

pub fn run_cli(args: &[String]) -> ExitCode {
    let stdin = std::io::stdin();
    let (stdout, stderr) = (std::io::stdout(), std::io::stderr());
    let code = dispatch(args, &mut stdin.lock(), &mut stdout.lock(), &mut stderr.lock());
    ExitCode::from(code as u8)
}

/// Testable core: returns the process exit code.
pub fn dispatch(args: &[String], input: &mut dyn Read, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let Some(cmd) = args.first() else {
        let _ = writeln!(err, "usage: codetrail <install|hook|note|record|ask|export> ...");
        return 2;
    };
    let rest = &args[1..];
    if cmd == "hook" {
        // always exit 0, never print errors (agent flow must not break)
        let (kind, event) = (rest.first().map(String::as_str), rest.get(1).map(String::as_str));
        if kind == Some("claude") {
            if let Some(ev) = event {
                // Last line of defence: whatever blocks (stdin, fs, a stuck child), exit 0 shortly after the
                // git deadline. Blocks in sleep, no polling.
                gitx::set_hard_deadline(std::time::Instant::now() + hook::STOP_BUDGET);
                std::thread::spawn(|| {
                    std::thread::sleep(hook::STOP_BUDGET + std::time::Duration::from_millis(500));
                    std::process::exit(0);
                });
                let mut s = String::new();
                let _ = input.read_to_string(&mut s);
                if let Some(o) = hook::handle_hook(ev, &s) {
                    let _ = writeln!(out, "{o}");
                }
            }
        }
        return 0;
    }
    let r = match cmd.as_str() {
        "install" => cmd_install(rest, out),
        "note" => cmd_note(rest, out),
        "record" => cmd_record(rest, out),
        "ask" => cmd_ask(rest, out, err),
        "export" => cmd_export(rest, out),
        other => Err(anyhow!("unknown subcommand '{other}'")),
    };
    match r {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(err, "codetrail: {e}");
            1
        }
    }
}

struct Args {
    flags: HashMap<String, String>,
    bools: HashSet<String>,
    pos: Vec<String>,
}

impl Args {
    fn parse(a: &[String], value_flags: &[&str], bool_flags: &[&str]) -> Result<Args> {
        let mut r = Args { flags: HashMap::new(), bools: HashSet::new(), pos: vec![] };
        let mut i = 0;
        while i < a.len() {
            let t = &a[i];
            if let Some(name) = t.strip_prefix("--") {
                let (k, inline) = match name.split_once('=') {
                    Some((k, v)) => (k, Some(v.to_string())),
                    None => (name, None),
                };
                if bool_flags.contains(&k) {
                    r.bools.insert(k.to_string());
                } else if value_flags.contains(&k) {
                    let v = match inline {
                        Some(v) => v,
                        None => {
                            i += 1;
                            a.get(i).cloned().ok_or_else(|| anyhow!("--{k} needs a value"))?
                        }
                    };
                    r.flags.insert(k.to_string(), v);
                } else {
                    bail!("unknown option --{k}");
                }
            } else {
                r.pos.push(t.clone());
            }
            i += 1;
        }
        Ok(r)
    }
    fn get(&self, k: &str) -> Option<&str> {
        self.flags.get(k).map(String::as_str)
    }
    fn has(&self, k: &str) -> bool {
        self.bools.contains(k)
    }
}

fn cwd() -> Result<PathBuf> {
    Ok(std::env::current_dir()?)
}

fn repo_here() -> Result<(RepoCtx, PathBuf)> {
    let c = cwd()?;
    let ctx = gitx::find_repo(&c).ok_or_else(|| anyhow!("not inside a git repository"))?;
    Ok((ctx, c))
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| anyhow!("$HOME is not set"))
}

fn cmd_install(a: &[String], out: &mut dyn Write) -> Result<i32> {
    let p = Args::parse(a, &[], &["claude", "omp", "codex", "dry-run"])?;
    let none = !(p.has("claude") || p.has("omp") || p.has("codex"));
    let o = InstallOpts {
        claude: none || p.has("claude"),
        omp: none || p.has("omp"),
        codex: none || p.has("codex"),
        dry_run: p.has("dry-run"),
        home: home()?,
        bin: std::env::var("CODETRAIL_BIN").unwrap_or_else(|_| "codetrail".into()),
    };
    for act in install::install(&o)? {
        writeln!(out, "{}", act.describe(o.dry_run))?;
    }
    Ok(0)
}

fn cmd_note(a: &[String], out: &mut dyn Write) -> Result<i32> {
    let p = Args::parse(a, &["record", "reason", "path", "session", "agent"], &[])?;
    let reason = p.get("reason").map(str::trim).filter(|r| !r.is_empty()).ok_or_else(|| anyhow!("--reason is required"))?;
    let (ctx, cwd) = repo_here()?;
    let store = Store::open(&ctx.git_dir)?;
    let rec = match p.get("record") {
        Some(id) => store.find_id(id).map_err(|e| anyhow!(e))?,
        None => {
            let (Some(sess), Some(path)) = (p.get("session"), p.get("path")) else {
                bail!("--record <id> is required (or both --session and --path)");
            };
            let rel = gitx::rel_path(&ctx, Path::new(path), &cwd).ok_or_else(|| anyhow!("{path} is outside the repository"))?;
            let cands: Vec<Record> = store.for_path(&rel).into_iter().filter(|r| r.session == sess).collect();
            let unannotated = cands.iter().filter(|r| r.reason.is_empty()).count();
            if cands.is_empty() {
                bail!("no record for session '{sess}' and path '{rel}'");
            }
            if unannotated > 1 {
                bail!("ambiguous: {unannotated} un-annotated records for that session+path; pass --record <id>");
            }
            cands.last().cloned().unwrap()
        }
    };
    let agent = p.get("agent").map(Agent::parse).unwrap_or_else(|| rec.agent.clone());
    store.append_note(rec.id, agent, p.get("session").unwrap_or(&rec.session), reason)?;
    writeln!(out, "noted {}", fmt_id(rec.id))?;
    Ok(0)
}

fn cmd_record(a: &[String], out: &mut dyn Write) -> Result<i32> {
    let p = Args::parse(a, &["path", "agent", "session", "tool"], &["json"])?;
    let (ctx, cwd) = repo_here()?;
    let store = Store::open(&ctx.git_dir)?;
    if let Some(id) = p.pos.first() {
        let rec = store.find_id(id).map_err(|e| anyhow!(e))?;
        if p.has("json") {
            let mut buf = Vec::new();
            store.export_json(&mut buf)?;
            let v: serde_json::Value = serde_json::from_slice(&buf)?;
            let want = fmt_id(rec.id);
            let one = v["records"].as_array().and_then(|r| r.iter().find(|x| x["id"] == want.as_str()));
            writeln!(out, "{}", serde_json::to_string_pretty(&one)?)?;
        } else {
            writeln!(out, "id: {}\nts: {}\nagent: {}\nsession: {}\nkind: {:?}\ntool: {}\npath: {}", fmt_id(rec.id), ask::iso_utc(rec.ts_ms), rec.agent.name(), rec.session, rec.kind, rec.tool, rec.path)?;
            for s in &rec.spans {
                writeln!(out, "span: {}+{}", s.new_start, s.new_len)?;
            }
            writeln!(out, "reason: {}", decode_reason(&rec))?;
        }
        return Ok(0);
    }
    // create an Edit record for a worktree file vs HEAD (agents without hooks: omp, Codex)
    let path = p.get("path").ok_or_else(|| anyhow!("usage: codetrail record <id> | codetrail record --path <file> [--agent A --session S]"))?;
    let rel = gitx::rel_path(&ctx, Path::new(path), &cwd).ok_or_else(|| anyhow!("{path} is outside the repository"))?;
    let abs = gitx::safe_join(&ctx.root, &rel).ok_or_else(|| anyhow!("{rel} resolves outside the repository"))?;
    let head = gitx::head(&ctx.root);
    let exists = abs.is_file();
    let post_blob = if exists { gitx::hash_object(&ctx.root, &rel) } else { None };
    let pre_blob = if head.is_empty() { None } else { gitx::blob_at(&ctx.root, "HEAD", &rel) };
    if pre_blob == post_blob {
        bail!("{rel} has no changes vs HEAD");
    }
    let spans = if exists {
        let pre = match &pre_blob {
            Some(_) => gitx::git_out(&ctx.root, &["show", &format!("HEAD:{rel}")]),
            None => Some(Vec::new()),
        };
        match (pre.filter(|b| b.len() <= gitx::MAX_DIFF_BYTES), gitx::read_text_limited(&abs)) {
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
        agent: p.get("agent").map(Agent::parse).unwrap_or(Agent::Other(0)),
        session: p.get("session").map(String::from).or_else(|| std::env::var("CODETRAIL_SESSION").ok()).unwrap_or_else(|| "manual".into()),
        kind: Kind::Edit,
        tool: p.get("tool").unwrap_or("manual").to_string(),
        tool_use_id: String::new(),
        head_at_edit: head,
        path: rel,
        pre_blob,
        post_blob,
        spans,
        reason: vec![],
        transcript: None,
    };
    store.append(&rec)?;
    writeln!(out, "recorded {}", fmt_id(rec.id))?;
    Ok(0)
}

fn cmd_export(a: &[String], out: &mut dyn Write) -> Result<i32> {
    let p = Args::parse(a, &[], &["json"])?;
    let (ctx, _) = repo_here()?;
    let store = Store::open(&ctx.git_dir)?;
    if p.has("json") {
        store.export_json(&mut *out)?;
    } else {
        let st = store.stats();
        for r in store.all() {
            writeln!(out, "{}\t{}\t{}\t{}\t{}", fmt_id(r.id), ask::iso_utc(r.ts_ms), r.agent.name(), r.path, decode_reason(&r).replace('\n', " "))?;
        }
        writeln!(out, "# records={} skipped_frames={}", st.records, st.skipped_frames)?;
    }
    Ok(0)
}

fn cmd_ask(a: &[String], out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
    let p = Args::parse(
        a,
        &["question", "agent", "timeout"],
        &["print", "no-code", "no-diff", "no-reasons", "no-transcript", "include-secrets"],
    )?;
    let r = p.pos.first().ok_or_else(|| anyhow!("usage: codetrail ask <path:L1-L2@rev> [--question Q] [--agent claude|omp|codex] [--print]"))?;
    let (ctx, _) = repo_here()?;
    let req = AskRequest {
        code_ref: CodeRef::parse(r)?,
        question: p.get("question").map(String::from),
        include: Include { code: !p.has("no-code"), diff: !p.has("no-diff"), reasons: !p.has("no-reasons"), transcript: !p.has("no-transcript") },
        include_secrets: p.has("include-secrets"),
    };
    let prompt = ask::build_prompt(&ctx, &req)?;
    for w in prompt.warnings() {
        writeln!(err, "warning: {w}{}", if req.include_secrets { "" } else { " (section excluded; --include-secrets to send)" })?;
    }
    let text = prompt.render();
    if p.has("print") {
        write!(out, "{text}")?;
        return Ok(0);
    }
    let agent = AgentKind::parse(p.get("agent").unwrap_or("claude"))?;
    let timeout = p.get("timeout").map(|t| t.parse::<u64>()).transpose().map_err(|_| anyhow!("bad --timeout"))?.unwrap_or(300);
    let opts = RunOpts { timeout: Duration::from_secs(timeout), cancel: Arc::new(AtomicBool::new(false)), cwd: ctx.root.clone() };
    let mut sink = |s: &str| {
        let _ = write!(out, "{s}");
        let _ = out.flush();
    };
    let res = ask::run_agent(agent, &text, &opts, &mut sink)?;
    match res.status {
        RunStatus::Exited(0) => Ok(0),
        RunStatus::Exited(c) => {
            writeln!(err, "codetrail: agent exited with {c}: {}", res.stderr.trim())?;
            Ok(1)
        }
        RunStatus::TimedOut => {
            writeln!(err, "codetrail: agent timed out after {timeout}s")?;
            Ok(124)
        }
        RunStatus::Cancelled => Ok(130),
    }
}
