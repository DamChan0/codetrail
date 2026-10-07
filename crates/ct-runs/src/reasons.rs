//! Auto reasons (PLAN §10.6): one ct-store Edit record per changed file, reason = run prompt +
//! assistant summary (+ `intent`), secret-masked.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use ct_agentd::{AgentEvent, BackendKind};
use ct_store::{encode_reason, new_id, now_ms, Agent, Kind, Record, Store, TranscriptPtr};
use regex::Regex;

use crate::artifact::Change;

const PROMPT_CHARS: usize = 1000;
const SUMMARY_CHARS: usize = 500;
const INTENT_CHARS: usize = 300;
/// Per text segment memory cap (only the first few hundred chars are ever used).
const SEGMENT_CAP: usize = 8 * 1024;
pub const MAX_RECORDS_PER_RUN: usize = 500;

static MASKS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"AKIA[0-9A-Z]{16}", "[REDACTED]"),
        (r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(-----END [A-Z ]*PRIVATE KEY-----|\z)", "[REDACTED PRIVATE KEY]"),
        (r#"(?i)((?:api[_-]?key|secret|token|passwd|password|auth)["']?\s*[:=]\s*["']?)[A-Za-z0-9_\-/+=.]{12,}"#, "${1}[REDACTED]"),
        (r"\bsk-[A-Za-z0-9_-]{20,}", "[REDACTED]"),
        (r"\bgh[pousr]_[A-Za-z0-9]{30,}", "[REDACTED]"),
        (r"\bxox[baprs]-[A-Za-z0-9-]{10,}", "[REDACTED]"),
        (r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]{20,}", "${1}[REDACTED]"),
    ]
    .into_iter()
    .map(|(r, rep)| (Regex::new(r).expect("static regex"), rep))
    .collect()
});

/// Masks secret-looking substrings; if ct-agent's scanner still flags the result the whole text is
/// withheld (fail closed).
pub fn mask_secrets(text: &str) -> String {
    let mut s = text.to_string();
    for (re, rep) in MASKS.iter() {
        s = re.replace_all(&s, *rep).into_owned();
    }
    if ct_agent::ask::scan_secrets(&s).is_empty() {
        s
    } else {
        "[withheld: possible secret detected]".to_string()
    }
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Accumulates what the reason needs from the event stream.
#[derive(Default)]
pub struct Acc {
    cur: String,
    last_nonempty: String,
    /// text segment before a tool start, by tool id
    before_tool: HashMap<String, String>,
    /// first intent per edited path (as reported by the backend)
    pub intents: HashMap<String, String>,
}

impl Acc {
    pub fn feed(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::TextDelta(t) => {
                if self.cur.len() < SEGMENT_CAP {
                    self.cur.push_str(t);
                }
            }
            AgentEvent::ToolStart { id, .. } => {
                let seg = self.cur.trim();
                if !seg.is_empty() {
                    self.last_nonempty = seg.to_string();
                    if self.before_tool.len() < 1024 {
                        self.before_tool.insert(id.clone(), seg.to_string());
                    }
                }
                self.cur.clear();
            }
            AgentEvent::ToolEnd { id, ok: true, path: Some(p), .. } => {
                if let Some(seg) = self.before_tool.get(id) {
                    self.intents.entry(p.clone()).or_insert_with(|| seg.clone());
                }
            }
            _ => {}
        }
    }

    /// The assistant's closing message: text after the last tool call, else the last text seen.
    pub fn summary(&self) -> String {
        let c = self.cur.trim();
        if c.is_empty() {
            self.last_nonempty.clone()
        } else {
            c.to_string()
        }
    }
}

pub struct ReasonCtx<'a> {
    pub run_id: &'a str,
    pub backend: BackendKind,
    pub prompt: &'a str,
    pub base_sha: &'a str,
    pub repo_root: &'a Path,
    pub store_dir: &'a Path,
    /// worktree / repo dir the agent ran in, to normalise absolute tool paths
    pub cwd: &'a Path,
    pub events_log: &'a Path,
}

pub fn agent_for(b: BackendKind) -> Agent {
    match b {
        BackendKind::Claude => Agent::Claude,
        BackendKind::Codex => Agent::Codex,
        BackendKind::Pi => Agent::parse("pi"),
    }
}

fn norm_tool_path(p: &str, cwd: &Path) -> String {
    let path = Path::new(p);
    let rel = path.strip_prefix(cwd).unwrap_or(path);
    rel.to_string_lossy().trim_start_matches("./").to_string()
}

/// Writes one Edit record per change; returns how many were written.
pub fn record_reasons(ctx: &ReasonCtx, changes: &[Change], acc: &Acc) -> Result<usize> {
    if changes.is_empty() {
        return Ok(0);
    }
    let store = Store::open(ctx.store_dir).map_err(|e| anyhow!("open store: {e}"))?;
    let prompt = first_chars(&mask_secrets(ctx.prompt), PROMPT_CHARS);
    let summary = first_chars(&mask_secrets(&acc.summary()), SUMMARY_CHARS);
    let mut base = format!("Run prompt: {}", prompt.trim());
    if !summary.trim().is_empty() {
        base.push_str(&format!("\n\nAgent summary: {}", summary.trim()));
    }
    let intents: HashMap<String, &String> =
        acc.intents.iter().map(|(p, t)| (norm_tool_path(p, ctx.cwd), t)).collect();

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut oids: Vec<String> = Vec::new();
    for c in changes.iter().take(MAX_RECORDS_PER_RUN) {
        oids.extend(c.pre_blob.clone());
        oids.extend(c.post_blob.clone());
    }
    oids.sort();
    oids.dedup();
    let blobs = ct_agent::gitx::blob_contents(ctx.repo_root, &oids, deadline);

    let agent = agent_for(ctx.backend);
    let mut n = 0;
    for c in changes.iter().take(MAX_RECORDS_PER_RUN) {
        let get = |o: &Option<String>| -> Option<&Vec<u8>> { o.as_ref().and_then(|o| blobs.get(o)) };
        let spans = match (&c.post_blob, get(&c.post_blob)) {
            (Some(_), Some(post)) if post.len() <= ct_agent::gitx::MAX_DIFF_BYTES => match (&c.pre_blob, get(&c.pre_blob)) {
                (None, _) => ct_agent::gitx::whole_file_spans(post),
                (Some(_), Some(pre)) if pre.len() <= ct_agent::gitx::MAX_DIFF_BYTES => ct_agent::gitx::spans_between(pre, post),
                _ => vec![],
            },
            _ => vec![],
        };
        let mut reason = base.clone();
        if let Some(i) = intents.get(&c.path) {
            let snip = first_chars(&mask_secrets(i), INTENT_CHARS);
            reason.push_str(&format!("\n\nIntent: {}", snip.trim()));
        }
        let ts = now_ms();
        let rec = Record {
            id: new_id(ts),
            ts_ms: ts,
            agent: agent.clone(),
            session: ctx.run_id.to_string(),
            kind: Kind::Edit,
            tool: "run".into(),
            tool_use_id: String::new(),
            head_at_edit: ctx.base_sha.to_string(),
            path: c.path.clone(),
            pre_blob: c.pre_blob.clone(),
            post_blob: c.post_blob.clone(),
            spans,
            reason: encode_reason(&reason),
            transcript: Some(TranscriptPtr { path: ctx.events_log.to_string_lossy().into_owned(), offset: 0 }),
        };
        store.append(&rec).map_err(|e| anyhow!("append record for {}: {e}", c.path))?;
        n += 1;
    }
    Ok(n)
}
