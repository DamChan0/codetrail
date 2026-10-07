//! `codetrail install [--claude] [--omp] [--codex] [--dry-run]` - idempotent, backup-before-modify,
//! merge-not-overwrite installation of hooks / skills.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub const SKILL_MD: &str = include_str!("../../../assets/skills/codetrail/SKILL.md");
pub const CODEX_BEGIN: &str = "<!-- codetrail BEGIN -->";
pub const CODEX_END: &str = "<!-- codetrail END -->";
const MATCHER: &str = "Edit|Write|MultiEdit|NotebookEdit";
const EVENTS: &[(&str, bool)] = &[("PreToolUse", true), ("PostToolUse", true), ("PostToolUseFailure", true), ("Stop", false)];

#[derive(Clone, Debug)]
pub struct InstallOpts {
    pub claude: bool,
    pub omp: bool,
    pub codex: bool,
    pub dry_run: bool,
    /// Directory that holds `.claude`, `.agents`, `.codex` (the CLI passes `$HOME`).
    pub home: PathBuf,
    /// Command used in hook entries (dedup key is the full command string).
    pub bin: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Created,
    Updated,
    Unchanged,
}

#[derive(Clone, Debug)]
pub struct Action {
    pub path: PathBuf,
    pub change: Change,
    pub backup: Option<PathBuf>,
}

impl Action {
    pub fn describe(&self, dry: bool) -> String {
        let verb = match (self.change, dry) {
            (Change::Unchanged, _) => "unchanged",
            (Change::Created, false) => "created",
            (Change::Created, true) => "would create",
            (Change::Updated, false) => "updated",
            (Change::Updated, true) => "would update",
        };
        match &self.backup {
            Some(b) => format!("{verb} {} (backup: {})", self.path.display(), b.display()),
            None => format!("{verb} {}", self.path.display()),
        }
    }
}

pub fn hook_command(bin: &str, event: &str) -> String {
    format!("{bin} hook claude {event}")
}

fn write_with_backup(path: &Path, new: &str, dry: bool) -> Result<Action> {
    let old = fs::read_to_string(path).ok();
    if old.as_deref() == Some(new) {
        return Ok(Action { path: path.into(), change: Change::Unchanged, backup: None });
    }
    let mut backup = None;
    if old.is_some() {
        let ts = ct_store::now_ms();
        let mut name = path.file_name().unwrap().to_os_string();
        name.push(format!(".codetrail-bak-{ts}"));
        backup = Some(path.with_file_name(name));
    }
    if !dry {
        if let Some(b) = &backup {
            fs::copy(path, b).with_context(|| format!("backup {}", path.display()))?;
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("codetrail-tmp{}", std::process::id()));
        fs::write(&tmp, new)?;
        fs::rename(&tmp, path)?;
    }
    Ok(Action { path: path.into(), change: if old.is_some() { Change::Updated } else { Change::Created }, backup })
}

/// Merge our hook entries into a parsed settings.json value. Returns true if anything was added.
pub fn merge_claude_settings(settings: &mut Value, bin: &str) -> Result<bool> {
    let root = settings.as_object_mut().ok_or_else(|| anyhow!("settings.json root is not an object"))?;
    let hooks = root.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks.as_object_mut().ok_or_else(|| anyhow!("settings.json `hooks` is not an object"))?;
    let mut changed = false;
    for (ev, with_matcher) in EVENTS {
        let cmd = hook_command(bin, ev);
        let arr = hooks.entry(*ev).or_insert_with(|| Value::Array(vec![]));
        let arr = arr.as_array_mut().ok_or_else(|| anyhow!("hooks.{ev} is not an array"))?;
        let present = arr.iter().any(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|hs| hs.iter().any(|h| h.get("command").and_then(Value::as_str) == Some(cmd.as_str())))
        });
        if present {
            continue;
        }
        let mut group = Map::new();
        if *with_matcher {
            group.insert("matcher".into(), json!(MATCHER));
        }
        group.insert("hooks".into(), json!([{"type": "command", "command": cmd, "timeout": 5}]));
        arr.push(Value::Object(group));
        changed = true;
    }
    Ok(changed)
}

fn skill_body() -> &'static str {
    // strip YAML front matter for AGENTS.md
    match SKILL_MD.strip_prefix("---\n").and_then(|r| r.split_once("\n---\n")) {
        Some((_, body)) => body.trim_start_matches('\n'),
        None => SKILL_MD,
    }
}

pub fn codex_block() -> String {
    format!("{CODEX_BEGIN}\n{}\n{CODEX_END}\n", skill_body().trim_end())
}

pub fn merge_codex_agents(existing: &str) -> String {
    let block = codex_block();
    if let (Some(b), Some(e)) = (existing.find(CODEX_BEGIN), existing.find(CODEX_END)) {
        if b < e {
            let end = e + CODEX_END.len();
            let tail = existing[end..].strip_prefix('\n').unwrap_or(&existing[end..]);
            return format!("{}{}{}", &existing[..b], block, tail);
        }
    }
    if existing.is_empty() {
        block
    } else {
        let sep = if existing.ends_with("\n\n") { "" } else if existing.ends_with('\n') { "\n" } else { "\n\n" };
        format!("{existing}{sep}{block}")
    }
}

pub fn install(o: &InstallOpts) -> Result<Vec<Action>> {
    let mut out = Vec::new();
    if o.claude {
        let p = o.home.join(".claude/settings.json");
        let mut v = match fs::read_to_string(&p) {
            Ok(s) if s.trim().is_empty() => Value::Object(Map::new()),
            Ok(s) => serde_json::from_str::<Value>(&s)
                .with_context(|| format!("{} is not valid JSON; refusing to modify it", p.display()))?,
            Err(_) => Value::Object(Map::new()),
        };
        let changed = merge_claude_settings(&mut v, &o.bin)?;
        if changed || !p.exists() {
            let mut txt = serde_json::to_string_pretty(&v)?;
            txt.push('\n');
            out.push(write_with_backup(&p, &txt, o.dry_run)?);
        } else {
            out.push(Action { path: p, change: Change::Unchanged, backup: None });
        }
        out.push(write_with_backup(&o.home.join(".claude/skills/codetrail/SKILL.md"), SKILL_MD, o.dry_run)?);
    }
    if o.omp {
        out.push(write_with_backup(&o.home.join(".agents/skills/codetrail/SKILL.md"), SKILL_MD, o.dry_run)?);
    }
    if o.codex {
        let p = o.home.join(".codex/AGENTS.md");
        let existing = fs::read_to_string(&p).unwrap_or_default();
        out.push(write_with_backup(&p, &merge_codex_agents(&existing), o.dry_run)?);
    }
    if out.is_empty() {
        bail!("nothing selected");
    }
    Ok(out)
}
