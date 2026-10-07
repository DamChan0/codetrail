//! Adapter from the GUI `RunService` trait to `ct_runs::RunManager` (PLAN §10.7).

use crate::agents::*;
use crate::agents_real::{event_back, sel_back};
use ct_runs as r;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

struct RealRuns(r::RunManager);

fn model(m: &ModelSel) -> r::ModelSel {
    let kind = match m.backend {
        Backend::Pi => r::BackendKind::Pi,
        Backend::Claude => r::BackendKind::Claude,
        Backend::Codex => r::BackendKind::Codex,
    };
    r::ModelSel { backend: kind, provider: m.provider.clone(), id: m.id.clone(), thinking: m.thinking.clone() }
}

fn state(s: r::RunState) -> RunState {
    match s {
        r::RunState::Queued => RunState::Queued,
        r::RunState::Running => RunState::Running,
        r::RunState::Succeeded => RunState::Succeeded,
        r::RunState::Failed(e) => RunState::Failed(e),
        r::RunState::Aborted => RunState::Aborted,
        r::RunState::Interrupted => RunState::Interrupted,
    }
}

fn info(i: r::RunInfo) -> RunInfo {
    RunInfo {
        id: i.id,
        spec: RunSpec { repo: i.spec.repo, prompt: i.spec.prompt, model: sel_back(&i.spec.model), base_ref: i.spec.base_ref, isolate: i.spec.isolate },
        base_sha: i.base_sha,
        branch: i.branch,
        worktree: i.worktree,
        state: state(i.state),
        started_ms: i.started_ms,
        ended_ms: i.ended_ms,
        files_changed: i.files_changed,
    }
}

impl RunService for RealRuns {
    fn submit(&self, spec: RunSpec) -> Result<String, String> {
        self.0.submit(r::RunSpec { repo: spec.repo, prompt: spec.prompt, model: model(&spec.model), base_ref: spec.base_ref, isolate: spec.isolate }).map_err(|e| format!("{e:#}"))
    }
    fn abort(&self, id: &str) -> Result<(), String> {
        self.0.abort(id).map_err(|e| format!("{e:#}"))
    }
    fn steer(&self, id: &str, text: &str) -> Result<(), String> {
        self.0.steer(id, text).map_err(|e| format!("{e:#}"))
    }
    fn list(&self) -> Vec<RunInfo> {
        self.0.list().into_iter().map(info).collect()
    }
    fn subscribe(&self) -> Receiver<RunUpdate> {
        let src = self.0.subscribe();
        let (tx, rx) = channel();
        let _ = std::thread::Builder::new().name("ct-run-map".into()).spawn(move || {
            for u in src {
                let m = match u {
                    r::RunUpdate::State(i) => RunUpdate::State(info(i)),
                    r::RunUpdate::Event(id, e) => RunUpdate::Event(id, event_back(e)),
                };
                if tx.send(m).is_err() {
                    return;
                }
            }
        });
        rx
    }
    fn apply(&self, id: &str) -> Result<ApplyOutcome, String> {
        match self.0.apply(id).map_err(|e| format!("{e:#}"))? {
            r::ApplyOutcome::Merged(sha) => Ok(ApplyOutcome { ok: true, message: format!("Merged into the current branch ({}).", crate::timefmt::short(&sha)) }),
            r::ApplyOutcome::Conflict(files) => Ok(ApplyOutcome { ok: false, message: format!("The merge conflicts in {} and was aborted; nothing changed in your work tree.", files.join(", ")) }),
            r::ApplyOutcome::NothingToApply => Ok(ApplyOutcome { ok: true, message: "Nothing to apply: the run made no changes, or they are already in HEAD.".into() }),
        }
    }
    fn discard(&self, id: &str) -> Result<(), String> {
        self.0.discard(id).map_err(|e| format!("{e:#}"))
    }
    fn comparison(&self, id: &str) -> Result<(String, String), String> {
        self.0.comparison(id).map_err(|e| format!("{e:#}"))
    }
    fn set_max_concurrent(&self, n: usize) {
        self.0.set_max_concurrent(n);
    }
    fn shutdown(&self) {
        self.0.shutdown_all();
    }
}

pub fn open(max_concurrent: usize) -> Result<Arc<dyn RunService>, String> {
    let m = r::RunManager::open(&r::default_data_dir(), max_concurrent).map_err(|e| format!("{e:#}"))?;
    Ok(Arc::new(RealRuns(m)))
}
