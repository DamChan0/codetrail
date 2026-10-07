//! `codetrail ask <CodeRef>`: prompt builder (code + diff + matched reasons + transcript pointer),
//! secret-looking pattern exclusion (PLAN §9.5) and agent runners with timeout / cancel / streaming.

use crate::gitx::{self, RepoCtx};
use anyhow::{anyhow, bail, Result};
use ct_store::{decode_reason, fmt_id, Confidence, Record, Store};
use regex::Regex;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, LazyLock};
use std::time::{Duration, Instant};

pub const MAX_PROMPT_BYTES: usize = 100_000;
const MAX_DIFF_LINES: usize = 300;
pub const DEFAULT_QUESTION: &str = "Why was this code changed? Explain the intent behind the change.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefAt {
    Commit(String),
    Worktree,
}

/// `path:L1-L2@<commit|worktree>`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeRef {
    pub path: String,
    pub start: u32,
    pub end: u32,
    pub at: RefAt,
}

impl CodeRef {
    pub fn parse(s: &str) -> Result<CodeRef> {
        let (left, at) = match s.rsplit_once('@') {
            Some((l, r)) if r == "worktree" || r.is_empty() => (l, RefAt::Worktree),
            Some((l, r)) => (l, RefAt::Commit(r.to_string())),
            None => (s, RefAt::Worktree),
        };
        let (path, range) = left.rsplit_once(':').ok_or_else(|| anyhow!("code ref needs path:L1-L2[@rev]: {s}"))?;
        let num = |x: &str| -> Result<u32> {
            x.trim_start_matches(['L', 'l']).parse::<u32>().map_err(|_| anyhow!("bad line number '{x}'"))
        };
        let (a, b) = match range.split_once('-') {
            Some((a, b)) => (num(a)?, num(b)?),
            None => (num(range)?, num(range)?),
        };
        if a == 0 || b < a {
            bail!("bad line range {range}");
        }
        Ok(CodeRef { path: path.to_string(), start: a, end: b, at })
    }
}

#[derive(Clone, Debug)]
pub struct Include {
    pub code: bool,
    pub diff: bool,
    pub reasons: bool,
    pub transcript: bool,
}
impl Default for Include {
    fn default() -> Self {
        Include { code: true, diff: true, reasons: true, transcript: true }
    }
}

#[derive(Clone, Debug)]
pub struct AskRequest {
    pub code_ref: CodeRef,
    pub question: Option<String>,
    pub include: Include,
    /// Send sections even if they look like they contain secrets.
    pub include_secrets: bool,
}

#[derive(Clone, Debug)]
pub struct Section {
    pub id: &'static str,
    pub title: String,
    pub body: String,
    /// GUI checkbox state; secret-looking sections start excluded.
    pub included: bool,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Prompt {
    pub sections: Vec<Section>,
    pub question: String,
    /// Secret-scan findings for the user question; non-empty => see `question_included`.
    pub question_warnings: Vec<String>,
    /// False when the question looked secret-bearing and `include_secrets` was not set; render()
    /// then sends a placeholder instead of the question.
    pub question_included: bool,
}

pub const QUESTION_WITHHELD: &str = "[question withheld: it looks like it contains a secret; re-run with explicit approval to send it]";

impl Prompt {
    pub fn render(&self) -> String {
        let mut s = String::from("You are helping understand a code change. Answer using only the context below.\n\n");
        for sec in self.sections.iter().filter(|x| x.included) {
            s.push_str(&format!("## {}\n{}\n\n", sec.title, sec.body.trim_end()));
        }
        let q = if self.question_included { self.question.as_str() } else { QUESTION_WITHHELD };
        s.push_str(&format!("## Question\n{q}\n"));
        if s.len() > MAX_PROMPT_BYTES {
            let mut cut = MAX_PROMPT_BYTES;
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            s.truncate(cut);
            s.push_str("\n[prompt truncated]\n");
        }
        s
    }
    pub fn warnings(&self) -> Vec<String> {
        let mut v: Vec<String> = self.sections.iter().flat_map(|s| s.warnings.iter().map(move |w| format!("{}: {w}", s.id))).collect();
        v.extend(self.question_warnings.iter().map(|w| format!("question: {w}")));
        v

    }
}

static SECRET_PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        ("aws access key", r"AKIA[0-9A-Z]{16}"),
        ("private key block", r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
        ("api key / secret / token assignment", r#"(?i)(api[_-]?key|secret|token|passwd|password|auth)["']?\s*[:=]\s*["']?[A-Za-z0-9_\-/+=.]{12,}"#),
        ("sk- style key", r"\bsk-[A-Za-z0-9_-]{20,}"),
        ("github token", r"\bgh[pousr]_[A-Za-z0-9]{30,}"),
        ("slack token", r"\bxox[baprs]-[A-Za-z0-9-]{10,}"),
    ]
    .into_iter()
    .map(|(n, r)| (n, Regex::new(r).expect("static regex")))
    .collect()
});

pub fn scan_secrets(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (name, re) in SECRET_PATTERNS.iter() {
        if let Some(m) = re.find(text) {
            let line = text[..m.start()].matches('\n').count() + 1;
            out.push(format!("possible secret ({name}) at line {line}"));
        }
    }
    out
}

fn secret_filename(path: &str) -> bool {
    let n = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    n == ".env" || n.starts_with(".env.") || n.ends_with(".pem") || n.ends_with(".key") || n == "id_rsa" || n == "id_ed25519"
}

pub fn iso_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // civil_from_days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

struct Hunk {
    new_start: u32,
    new_len: u32,
    text: String,
    added: Vec<String>,
}

fn parse_hunks(diff: &str) -> (String, Vec<Hunk>) {
    let mut header = String::new();
    let mut hunks: Vec<Hunk> = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("@@ ") {
            let plus = rest.split_whitespace().find(|t| t.starts_with('+')).unwrap_or("+0");
            let plus = &plus[1..];
            let (s, l) = plus.split_once(',').unwrap_or((plus, "1"));
            hunks.push(Hunk {
                new_start: s.parse().unwrap_or(0),
                new_len: l.parse().unwrap_or(1),
                text: format!("{line}\n"),
                added: vec![],
            });
        } else if let Some(h) = hunks.last_mut() {
            if let Some(a) = line.strip_prefix('+') {
                h.added.push(a.to_string());
            }
            h.text.push_str(line);
            h.text.push('\n');
        } else {
            header.push_str(line);
            header.push('\n');
        }
    }
    (header, hunks)
}

fn numbered(text: &str, start: u32, end: u32) -> String {
    text.lines()
        .enumerate()
        .filter(|(i, _)| (*i as u32 + 1) >= start && (*i as u32 + 1) <= end)
        .map(|(i, l)| format!("{:>5}  {l}\n", i + 1))
        .collect()
}

fn conf_name(c: Confidence) -> &'static str {
    match c {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Unknown => "unknown",
    }
}

pub fn build_prompt(repo: &RepoCtx, req: &AskRequest) -> Result<Prompt> {
    let r = &req.code_ref;
    let root = &repo.root;
    // resolve revision / content
    let (content, rev_label, commit, blob): (Vec<u8>, String, Option<String>, Option<String>) = match &r.at {
        RefAt::Worktree => {
            let abs = gitx::safe_join(root, &r.path).ok_or_else(|| anyhow!("{} is not a valid repo-relative path", r.path))?;
            let c = std::fs::read(abs).map_err(|e| anyhow!("cannot read {}: {e}", r.path))?;
            (c, "worktree".into(), None, gitx::hash_object(root, &r.path))
        }
        RefAt::Commit(rev) => {
            gitx::safe_join(root, &r.path).ok_or_else(|| anyhow!("{} is not a valid repo-relative path", r.path))?;
            let full = gitx::git_str(root, &["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")])
                .ok_or_else(|| anyhow!("unknown revision '{rev}'"))?;
            let c = gitx::git_out(root, &["show", &format!("{full}:{}", r.path)])
                .ok_or_else(|| anyhow!("{} does not exist at {rev}", r.path))?;
            let blob = gitx::blob_at(root, &full, &r.path);
            (c, full[..full.len().min(12)].to_string(), Some(full), blob)
        }
    };
    let text = String::from_utf8_lossy(&content).into_owned();
    let secret_file = secret_filename(&r.path);
    let mut sections = Vec::new();

    // code
    sections.push(Section {
        id: "code",
        title: format!("Code: {}:{}-{}@{rev_label}", r.path, r.start, r.end),
        body: numbered(&text, r.start, r.end),
        included: req.include.code,
        warnings: vec![],
    });

    // diff
    let raw_diff = match &commit {
        Some(c) => gitx::git_str(root, &["show", "--format=", "--no-color", "--no-ext-diff", "-U3", c, "--", &r.path]),
        None => gitx::git_str(root, &["diff", "HEAD", "--no-color", "--no-ext-diff", "-U3", "--", &r.path]),
    }
    .unwrap_or_default();
    let (header, hunks) = parse_hunks(&raw_diff);
    let overlapping: Vec<&Hunk> = hunks
        .iter()
        .filter(|h| h.new_start <= r.end && h.new_start + h.new_len.max(1) - 1 >= r.start)
        .collect();
    let mut diff_body = String::new();
    if overlapping.is_empty() {
        diff_body.push_str("(no diff hunk overlaps this range)\n");
    } else {
        diff_body.push_str(&header);
        for h in &overlapping {
            diff_body.push_str(&h.text);
        }
        let n = diff_body.lines().count();
        if n > MAX_DIFF_LINES {
            diff_body = diff_body.lines().take(MAX_DIFF_LINES).collect::<Vec<_>>().join("\n");
            diff_body.push_str(&format!("\n[diff truncated: {n} lines]\n"));
        }
    }
    sections.push(Section {
        id: "diff",
        title: "Diff (hunks overlapping the range)".into(),
        body: diff_body,
        included: req.include.diff,
        warnings: vec![],
    });

    // reasons + transcript pointers
    let mut matched: BTreeMap<u128, (Record, Confidence)> = BTreeMap::new();
    if let Ok(store) = Store::open(&repo.git_dir) {
        let blobs: Vec<String> = blob.iter().cloned().collect();
        for h in &overlapping {
            for (rec, c) in store.match_hunk(&r.path, &h.added, &blobs) {
                matched
                    .entry(rec.id)
                    .and_modify(|e| e.1 = e.1.min(c))
                    .or_insert((rec, c));
            }
        }
        if commit.is_none() {
            for rec in store.for_range(&r.path, r.start, r.end, None) {
                matched.entry(rec.id).or_insert((rec, Confidence::High));
            }
        }
    }
    let mut recs: Vec<&(Record, Confidence)> = matched.values().collect();
    recs.sort_by_key(|(rec, _)| (rec.ts_ms, rec.id));
    let mut reasons = String::new();
    let mut transcripts = String::new();
    for (rec, c) in &recs {
        let reason = decode_reason(rec);
        reasons.push_str(&format!(
            "- [{}] {} session {} at {} (record {}): {}\n",
            conf_name(*c),
            rec.agent.name(),
            rec.session,
            iso_utc(rec.ts_ms),
            fmt_id(rec.id),
            if reason.is_empty() { "(no reason recorded)" } else { reason.trim() }
        ));
        if let Some(t) = &rec.transcript {
            transcripts.push_str(&format!("- record {}: {} @ byte {}\n", fmt_id(rec.id), t.path, t.offset));
        }
    }
    if reasons.is_empty() {
        reasons.push_str("(no recorded reasons linked to this range)\n");
    }
    if transcripts.is_empty() {
        transcripts.push_str("(none)\n");
    }
    sections.push(Section {
        id: "reasons",
        title: "Recorded reasons".into(),
        body: reasons,
        included: req.include.reasons,
        warnings: vec![],
    });
    sections.push(Section {
        id: "transcript",
        title: "Transcript pointers".into(),
        body: transcripts,
        included: req.include.transcript,
        warnings: vec![],
    });

    // secret handling: warn + exclude by default
    for s in &mut sections {
        let mut w = scan_secrets(&s.body);
        if secret_file && (s.id == "code" || s.id == "diff") {
            w.push(format!("file name looks sensitive ({})", r.path));
        }
        if !w.is_empty() {
            if !req.include_secrets {
                s.included = false;
            }
            s.warnings = w;
        }
    }
    let question = req.question.clone().filter(|q| !q.trim().is_empty()).unwrap_or_else(|| DEFAULT_QUESTION.to_string());
    let question_warnings = scan_secrets(&question);
    let question_included = question_warnings.is_empty() || req.include_secrets;
    Ok(Prompt { sections, question, question_warnings, question_included })
}

// ---------------------------------------------------------------- runners

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Omp,
    Codex,
}

impl AgentKind {
    pub fn parse(s: &str) -> Result<AgentKind> {
        match s {
            "claude" => Ok(AgentKind::Claude),
            "omp" => Ok(AgentKind::Omp),
            "codex" => Ok(AgentKind::Codex),
            o => bail!("unknown agent '{o}' (claude|omp|codex)"),
        }
    }
    fn env_name(self) -> &'static str {
        match self {
            AgentKind::Claude => "CODETRAIL_CLAUDE_BIN",
            AgentKind::Omp => "CODETRAIL_OMP_BIN",
            AgentKind::Codex => "CODETRAIL_CODEX_BIN",
        }
    }
    fn default_bin(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Omp => "omp",
            AgentKind::Codex => "codex",
        }
    }
    /// (args, prompt via stdin?)
    fn args(self, prompt: &str) -> (Vec<String>, bool) {
        match self {
            AgentKind::Claude => (vec!["-p".into()], true),
            AgentKind::Omp => (vec!["-p".into(), prompt.to_string()], false),
            AgentKind::Codex => (vec!["exec".into(), "-".into()], true),
        }
    }
}

#[derive(Clone)]
pub struct RunOpts {
    pub timeout: Duration,
    pub cancel: Arc<AtomicBool>,
    pub cwd: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Exited(i32),
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct RunResult {
    pub status: RunStatus,
    pub stderr: String,
}

/// Runs the agent CLI, calling `on_chunk` with stdout text as it arrives (for GUI streaming).
pub fn run_agent(agent: AgentKind, prompt: &str, o: &RunOpts, on_chunk: &mut dyn FnMut(&str)) -> Result<RunResult> {
    let bin = std::env::var(agent.env_name()).unwrap_or_else(|_| agent.default_bin().to_string());
    let (args, use_stdin) = agent.args(prompt);
    let mut child = Command::new(&bin)
        .args(&args)
        .current_dir(&o.cwd)
        .stdin(if use_stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| anyhow!("cannot start '{bin}': {e}"))?;
    if use_stdin {
        let mut si = child.stdin.take().unwrap();
        let p = prompt.to_string();
        std::thread::spawn(move || {
            let _ = si.write_all(p.as_bytes());
        });
    }
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let mut so = child.stdout.take().unwrap();
    let rd = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = so.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut se = child.stderr.take().unwrap();
    let er = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = se.read_to_end(&mut s);
        String::from_utf8_lossy(&s).into_owned()
    });

    let start = Instant::now();
    let mut pending: Vec<u8> = Vec::new();
    let emit = |pending: &mut Vec<u8>, force: bool, on_chunk: &mut dyn FnMut(&str)| {
        let valid = match std::str::from_utf8(pending) {
            Ok(_) => pending.len(),
            Err(e) => e.valid_up_to(),
        };
        let cut = if force { pending.len() } else { valid };
        if cut > 0 {
            on_chunk(&String::from_utf8_lossy(&pending[..cut]));
            pending.drain(..cut);
        }
    };
    let status = loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(b) => {
                pending.extend_from_slice(&b);
                emit(&mut pending, false, on_chunk);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(Duration::from_millis(20)),
        }
        if o.cancel.load(Ordering::Relaxed) {
            kill_tree(&mut child);
            break RunStatus::Cancelled;
        }
        if start.elapsed() > o.timeout {
            kill_tree(&mut child);
            break RunStatus::TimedOut;
        }
        if let Some(st) = child.try_wait()? {
            while let Ok(b) = rx.recv_timeout(Duration::from_millis(50)) {
                pending.extend_from_slice(&b);
            }
            break RunStatus::Exited(st.code().unwrap_or(-1));
        }
    };
    emit(&mut pending, true, on_chunk);
    let killed = matches!(status, RunStatus::Cancelled | RunStatus::TimedOut);
    let stderr = if killed {
        String::new() // reader threads end on their own once the pipes close
    } else {
        let _ = rd.join();
        er.join().unwrap_or_default()
    };
    Ok(RunResult { status, stderr })
}

/// Kill the child and its whole process group (agent CLIs spawn helpers).
fn kill_tree(child: &mut Child) {
    let _ = Command::new("kill").args(["-KILL", "--", &format!("-{}", child.id())]).stderr(Stdio::null()).status();
    let _ = child.kill();
    let _ = child.wait();
}
