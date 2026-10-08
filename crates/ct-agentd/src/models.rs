//! Model lists: pi (short-lived RPC, cached), claude (static aliases), codex (`codex debug models`, static fallback).

use crate::config::Config;
use crate::proc::{self, Spec};
use crate::{BackendKind, ModelInfo, Result};
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

const PI_TTL: Duration = Duration::from_secs(300);

static PI_CACHE: LazyLock<Mutex<HashMap<PathBuf, (Instant, Vec<ModelInfo>)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Drop cached pi models for this runtime (call after login/logout).
pub(crate) fn invalidate(cfg: &Config) {
    PI_CACHE.lock().remove(&cfg.pi_home());
}

fn claude_models() -> Vec<ModelInfo> {
    [("sonnet", "Claude Sonnet (latest)"), ("opus", "Claude Opus (latest)"), ("haiku", "Claude Haiku (latest)")]
        .into_iter()
        .map(|(id, name)| ModelInfo {
            backend: BackendKind::Claude,
            provider: "anthropic".into(),
            id: id.into(),
            name: name.into(),
            reasoning: id != "haiku",
            context_window: 200_000,
        })
        .collect()
}

fn codex_fallback() -> Vec<ModelInfo> {
    ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"]
        .into_iter()
        .map(|id| ModelInfo { backend: BackendKind::Codex, provider: "openai".into(), id: id.into(), name: id.into(), reasoning: true, context_window: 0 })
        .collect()
}

fn codex_models(cfg: &Config) -> Vec<ModelInfo> {
    let spec = Spec {
        program: cfg.codex_bin.clone(),
        args: vec!["debug".into(), "models".into()],
        cwd: Some(std::env::temp_dir()),
        env: proc::whitelist_env(&["CODEX_HOME"], &[]),
        ceiling: None,
    };
    let Ok(cap) = proc::run_capture(spec, cfg.status_timeout) else { return codex_fallback() };
    if cap.timed_out || cap.code != Some(0) {
        return codex_fallback();
    }
    let Ok(v) = serde_json::from_slice::<Value>(&cap.stdout) else { return codex_fallback() };
    let list: Vec<ModelInfo> = v
        .get("models")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|m| m.get("visibility").and_then(Value::as_str) == Some("list"))
                .filter_map(|m| {
                    let id = m.get("slug")?.as_str()?.to_string();
                    Some(ModelInfo {
                        backend: BackendKind::Codex,
                        provider: "openai".into(),
                        name: m.get("display_name").and_then(Value::as_str).unwrap_or(&id).to_string(),
                        id,
                        reasoning: m.get("supported_reasoning_levels").and_then(Value::as_array).is_some_and(|l| !l.is_empty()),
                        context_window: m.get("context_window").and_then(Value::as_u64).map_or(0, |n| n.min(u32::MAX as u64) as u32),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if list.is_empty() {
        codex_fallback()
    } else {
        list
    }
}

pub(crate) fn list_models(cfg: &Config, b: BackendKind) -> Result<Vec<ModelInfo>> {
    match b {
        BackendKind::Claude => Ok(claude_models()),
        BackendKind::Codex => Ok(codex_models(cfg)),
        BackendKind::Pi => {
            let key = cfg.pi_home();
            if let Some((t, v)) = PI_CACHE.lock().get(&key) {
                if t.elapsed() < PI_TTL {
                    return Ok(v.clone());
                }
            }
            let v = crate::pi::list_models_uncached(cfg)?;
            PI_CACHE.lock().insert(key, (Instant::now(), v.clone()));
            Ok(v)
        }
    }
}
