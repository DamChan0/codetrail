//! Background agent runs: isolated git worktrees, queue, process supervision, artifact commits,
//! Apply / Discard and automatic ct-store reasons (PLAN §10.2 / §10.6 / §10.7).
//!
//! The agent itself is reached only through [`SessionFactory`] / `ct_agentd::AgentSession`.

mod artifact;
mod gitrun;
mod persist;
mod proc;
mod reasons;
mod resources;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
pub use ct_agentd::{AgentEvent, AgentSession, BackendKind, ModelSel, SessionOpts};
use parking_lot::Mutex;

use persist::{EventLog, Stored};

pub use proc::process_start_time;
pub use reasons::mask_secrets;
pub use resources::{Limits, Resource};

pub type Result<T, E = anyhow::Error> = std::result::Result<T, E>;

// ------------------------------------------------------------------ public contract

#[derive(Clone, Debug)]
pub struct RunSpec {
    pub repo: PathBuf,
    pub prompt: String,
    pub model: ModelSel,
    pub base_ref: String,
    pub isolate: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunState {
    Queued,
    Running,
    Succeeded,
    Failed(String),
    Aborted,
    Interrupted,
}

impl RunState {
    pub fn is_active(&self) -> bool {
        matches!(self, RunState::Queued | RunState::Running)
    }
}

#[derive(Clone, Debug)]
pub struct RunInfo {
    pub id: String,
    pub spec: RunSpec,
    /// Immutable sha `spec.base_ref` resolved to at submit time.
    pub base_sha: String,
    pub branch: Option<String>,
    pub worktree: Option<PathBuf>,
    pub state: RunState,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub files_changed: u32,
}

#[derive(Clone, Debug)]
pub enum RunUpdate {
    State(RunInfo),
    Event(String, AgentEvent),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// `--no-ff` merge commit created; its sha.
    Merged(String),
    /// Merge conflicted and was aborted; the conflicting files. Nothing changed in the work tree.
    Conflict(Vec<String>),
    /// Run produced no changes, or they are already contained in HEAD.
    NothingToApply,
}

/// Where sessions come from. Tests inject fakes; the default delegates to `ct_agentd::start_session`.
pub trait SessionFactory: Send + Sync {
    fn start(&self, b: BackendKind, o: SessionOpts) -> ct_agentd::Result<Box<dyn AgentSession>>;
}

pub struct DefaultFactory;

impl SessionFactory for DefaultFactory {
    fn start(&self, b: BackendKind, o: SessionOpts) -> ct_agentd::Result<Box<dyn AgentSession>> {
        ct_agentd::start_session(b, o)
    }
}

/// Supervisor timings (PLAN §10.6: abort → 5 s → SIGTERM(group) → 3 s → SIGKILL(group)).
#[derive(Clone, Copy, Debug)]
pub struct Timings {
    pub abort_grace: Duration,
    pub term_grace: Duration,
    /// Event-loop poll interval.
    pub tick: Duration,
    /// Pause between the artifact commit and the terminal state (test knob: widens the window in
    /// which an `abort()` still wins over a Settled that was already seen).
    pub finalize_delay: Duration,
    /// Resource sampler period.
    pub sample_interval: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            abort_grace: Duration::from_secs(5),
            term_grace: Duration::from_secs(3),
            tick: Duration::from_millis(25),
            finalize_delay: Duration::ZERO,
            sample_interval: Duration::from_secs(2),
        }
    }
}

/// `$CT_DATA_DIR`, else `$XDG_DATA_HOME/codetrail`, else `~/.local/share/codetrail`.
pub fn default_data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CT_DATA_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("codetrail");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".local/share/codetrail")
}

// ------------------------------------------------------------------ manager

enum Cmd {
    Abort,
    Steer(String, Sender<std::result::Result<(), String>>),
}

struct Entry {
    stored: Stored,
    ctl: Option<Sender<Cmd>>,
    thread: Option<JoinHandle<()>>,
    /// an `abort()` was accepted
    abort_req: bool,
    /// the terminal state is being written; `abort()` is refused from here on
    finalising: bool,
    /// resource-limit reason; turns the abort into `Failed(reason)`
    limit: Option<String>,
}

struct State {
    runs: HashMap<String, Entry>,
    /// submission order
    order: Vec<String>,
    queue: VecDeque<String>,
    running: usize,
    max: usize,
    shutting_down: bool,
    /// the single sampler thread is running (only while `running > 0`)
    sampler_active: bool,
}

struct Inner {
    data: PathBuf,
    factory: Arc<dyn SessionFactory>,
    timings: Timings,
    st: Mutex<State>,
    subs: Mutex<Vec<Sender<RunUpdate>>>,
    /// apply / discard are serialized (they mutate the user's repo)
    exclusive: Mutex<()>,
    limits: Mutex<Limits>,
    max_total_rss_mb: std::sync::atomic::AtomicU64,
    /// latest sample per running run
    res: Mutex<HashMap<String, Resource>>,
    /// signalled (with `st`) when a run finishes so the sampler can exit without waiting a tick
    sampler_wake: parking_lot::Condvar,
}

#[derive(Clone)]
pub struct RunManager {
    inner: Arc<Inner>,
}

impl RunManager {
    pub fn open(data_dir: &Path, max_concurrent: usize) -> Result<Self> {
        Self::open_with(data_dir, max_concurrent, Arc::new(DefaultFactory), Timings::default())
    }

    /// Opens the run store, reconciles runs left over from a previous process, resumes the queue.
    pub fn open_with(data_dir: &Path, max_concurrent: usize, factory: Arc<dyn SessionFactory>, timings: Timings) -> Result<Self> {
        std::fs::create_dir_all(persist::runs_dir(data_dir)).with_context(|| format!("create {}", data_dir.display()))?;
        let data = data_dir.canonicalize()?;
        let mut runs = HashMap::new();
        let mut order = Vec::new();
        let mut queue = VecDeque::new();
        for mut s in persist::load_all(&data) {
            if s.state == "running" {
                reconcile_orphan(&data, &mut s, timings);
                let _ = persist::save(&data, &s);
            }
            if s.state == "queued" {
                queue.push_back(s.id.clone());
            }
            order.push(s.id.clone());
            runs.insert(s.id.clone(), Entry { stored: s, ctl: None, thread: None, abort_req: false, finalising: false, limit: None });
        }
        let inner = Arc::new(Inner {
            data,
            factory,
            timings,
            st: Mutex::new(State { runs, order, queue, running: 0, max: max_concurrent.max(1), shutting_down: false, sampler_active: false }),
            subs: Mutex::new(Vec::new()),
            exclusive: Mutex::new(()),
            limits: Mutex::new(Limits::default()),
            max_total_rss_mb: std::sync::atomic::AtomicU64::new(resources::DEFAULT_MAX_TOTAL_RSS_MB),
            res: Mutex::new(HashMap::new()),
            sampler_wake: parking_lot::Condvar::new(),
        });
        pump(&inner);
        Ok(RunManager { inner })
    }

    pub fn set_max_concurrent(&self, n: usize) {
        self.inner.st.lock().max = n.max(1);
        pump(&self.inner);
    }

    pub fn submit(&self, spec: RunSpec) -> Result<String> {
        if spec.prompt.trim().is_empty() {
            return Err(anyhow!("empty prompt"));
        }
        let repo = ct_core::Repo::open(&spec.repo).map_err(|e| anyhow!("{e}"))?;
        let base_sha = repo.resolve(&spec.base_ref).map_err(|e| anyhow!("base ref {:?}: {e}", spec.base_ref))?;
        let root = repo.root.canonicalize().unwrap_or_else(|_| repo.root.clone());
        let ts = ct_store::now_ms();
        let id = format!("{:x}-{:08x}", ts / 1000, ct_store::new_id(ts) as u32);
        let mut spec = spec;
        spec.repo = root;
        let stored = Stored::new(id.clone(), &spec, base_sha, ts);
        {
            let mut st = self.inner.st.lock();
            if st.shutting_down {
                return Err(anyhow!("run manager is shutting down"));
            }
            persist::save(&self.inner.data, &stored)?;
            st.order.push(id.clone());
            st.queue.push_back(id.clone());
            st.runs.insert(id.clone(), Entry { stored: stored.clone(), ctl: None, thread: None, abort_req: false, finalising: false, limit: None });
        }
        self.inner.emit_state(stored.info());
        pump(&self.inner);
        Ok(id)
    }

    pub fn abort(&self, id: &str) -> Result<()> {
        let mut st = self.inner.st.lock();
        let e = st.runs.get_mut(id).ok_or_else(|| anyhow!("unknown run {id}"))?;
        match e.stored.state.as_str() {
            "queued" => {
                e.stored.set_state(&RunState::Aborted);
                e.stored.ended_ms = Some(ct_store::now_ms());
                persist::save(&self.inner.data, &e.stored)?;
                let info = e.stored.info();
                st.queue.retain(|q| q != id);
                drop(st);
                self.inner.emit_state(info);
                Ok(())
            }
            "running" => {
                if e.finalising {
                    return Err(anyhow!("run {id} is already finishing"));
                }
                let ctl = e.ctl.clone().ok_or_else(|| anyhow!("run {id} is starting"))?;
                // accepted: from here on this abort wins over any Settled that arrives later
                e.abort_req = true;
                drop(st);
                let _ = ctl.send(Cmd::Abort);
                Ok(())
            }
            _ => Err(anyhow!("run {id} is not active")),
        }
    }

    /// Resource totals (process group) of every running run, as of the last sample (≤ 2 s old).
    pub fn resources(&self) -> Vec<(String, Resource)> {
        let mut v: Vec<_> = self.inner.res.lock().iter().map(|(k, r)| (k.clone(), *r)).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Limits applied to every run (also already running ones).
    pub fn set_limits(&self, l: Limits) {
        *self.inner.limits.lock() = l;
    }

    pub fn limits(&self) -> Limits {
        *self.inner.limits.lock()
    }

    /// Global RSS budget over all running runs (0 = unlimited); new runs stay Queued while exceeded.
    pub fn set_max_total_rss_mb(&self, mb: u64) {
        self.inner.max_total_rss_mb.store(mb, std::sync::atomic::Ordering::Relaxed);
        pump(&self.inner);
    }

    pub fn steer(&self, id: &str, text: &str) -> Result<()> {
        let ctl = {
            let st = self.inner.st.lock();
            let e = st.runs.get(id).ok_or_else(|| anyhow!("unknown run {id}"))?;
            if e.stored.state != "running" {
                return Err(anyhow!("run {id} is not running"));
            }
            e.ctl.clone().ok_or_else(|| anyhow!("run {id} is starting"))?
        };
        let (tx, rx) = channel();
        ctl.send(Cmd::Steer(text.to_string(), tx)).map_err(|_| anyhow!("run {id} has finished"))?;
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow!(e)),
            Err(_) => Err(anyhow!("steer timed out")),
        }
    }

    /// Submission order (oldest first).
    pub fn list(&self) -> Vec<RunInfo> {
        let st = self.inner.st.lock();
        st.order.iter().filter_map(|id| st.runs.get(id)).map(|e| e.stored.info()).collect()
    }

    pub fn get(&self, id: &str) -> Option<RunInfo> {
        self.inner.st.lock().runs.get(id).map(|e| e.stored.info())
    }

    pub fn subscribe(&self) -> Receiver<RunUpdate> {
        let (tx, rx) = channel();
        self.inner.subs.lock().push(tx);
        rx
    }

    /// `(base_sha, run head sha)` for a ct-core comparison.
    pub fn comparison(&self, id: &str) -> Result<(String, String)> {
        let s = self.inner.stored(id)?;
        let head = self.inner.head_of(&s)?;
        Ok((s.base_sha, head))
    }

    /// Path of the run's JSONL event log (the transcript pointer used by auto reasons).
    pub fn events_log(&self, id: &str) -> PathBuf {
        EventLog::path(&self.inner.data, id)
    }

    /// Merges the run branch into the user's current branch with `--no-ff`. Requires a clean tracked
    /// work tree and a HEAD that descends from the run's base. A conflict is aborted and reported.
    pub fn apply(&self, id: &str) -> Result<ApplyOutcome> {
        let _x = self.inner.exclusive.lock();
        let s = self.inner.stored(id)?;
        if s.is_active() {
            return Err(anyhow!("run {id} is still active"));
        }
        if !s.isolate {
            return Err(anyhow!("run {id} executed in place; its changes are already in your work tree"));
        }
        let branch = s.branch.clone().ok_or_else(|| anyhow!("run {id} has no branch"))?;
        let root = &s.repo;
        let head = s.head_sha.clone().ok_or_else(|| anyhow!("run {id} has no recorded head commit"))?;
        let tip = gitrun::out(root, &["rev-parse", "--verify", &format!("refs/heads/{branch}")]).context("run branch is missing")?;
        if tip != head {
            return Err(anyhow!(
                "branch {branch} moved after the run finished ({} -> {}); refusing to merge unreviewed commits",
                &head[..12.min(head.len())],
                &tip[..12.min(tip.len())]
            ));
        }
        if head == s.base_sha || gitrun::test(root, &["merge-base", "--is-ancestor", &head, "HEAD"])? {
            return Ok(ApplyOutcome::NothingToApply);
        }
        if !gitrun::run(root, &["status", "--porcelain=v1", "-z", "--untracked-files=no"])?.is_empty() {
            return Err(anyhow!("work tree has uncommitted changes; commit or stash them before applying"));
        }
        if !gitrun::test(root, &["merge-base", "--is-ancestor", &s.base_sha, "HEAD"])? {
            return Err(anyhow!("current HEAD does not descend from the run's base {}", &s.base_sha[..12.min(s.base_sha.len())]));
        }
        let msg = format!("Merge codetrail run {id} ({branch})");
        // merge the reviewed commit itself, not the (mutable) branch name
        let o = gitrun::run_raw(root, &["merge", "--no-ff", "--no-edit", "-m", &msg, &head], &[], None)?;
        if o.code == 0 {
            return Ok(ApplyOutcome::Merged(gitrun::out(root, &["rev-parse", "HEAD"])?));
        }
        let files: Vec<String> = gitrun::run(root, &["diff", "--name-only", "--diff-filter=U", "-z"])
            .map(|b| b.split(|c| *c == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect())
            .unwrap_or_default();
        let merging = gitrun::test(root, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]).unwrap_or(false);
        if merging {
            gitrun::run(root, &["merge", "--abort"]).context("merge --abort failed")?;
        }
        if files.is_empty() {
            return Err(anyhow!("merge failed: {}", o.stderr));
        }
        Ok(ApplyOutcome::Conflict(files))
    }

    /// Deletes the run: worktree + branch + run ref + record. Only paths below
    /// `<data_dir>/worktrees` are ever removed. Active runs must be aborted first.
    pub fn discard(&self, id: &str) -> Result<()> {
        let _x = self.inner.exclusive.lock();
        let s = self.inner.stored(id)?;
        if s.is_active() {
            return Err(anyhow!("run {id} is still active; abort it first"));
        }
        // Validate everything before touching anything.
        if let Some(wt) = &s.worktree {
            artifact::check_worktree_path(&self.inner.data, wt)?;
        }
        if let Some(b) = &s.branch {
            if !b.starts_with("ct/") {
                return Err(anyhow!("refusing to delete branch {b:?}"));
            }
        }
        let root = s.repo.clone();
        if let Some(wt) = &s.worktree {
            artifact::remove_worktree(&self.inner.data, &root, wt, s.branch.as_deref())?;
        }
        if root.is_dir() {
            let r = artifact::run_ref(id);
            if gitrun::test(&root, &["show-ref", "--verify", "--quiet", &r]).unwrap_or(false) {
                gitrun::run(&root, &["update-ref", "-d", &r])?;
            }
        }
        let dir = persist::run_dir(&self.inner.data, id);
        if persist::valid_id(id) && dir.starts_with(persist::runs_dir(&self.inner.data)) {
            let _ = std::fs::remove_dir_all(&dir);
        }
        let mut st = self.inner.st.lock();
        st.runs.remove(id);
        st.order.retain(|o| o != id);
        Ok(())
    }

    /// App quit: abort every running run (staged kill), wait for them, keep queued runs queued.
    pub fn shutdown_all(&self) {
        let (ctls, handles): (Vec<Sender<Cmd>>, Vec<JoinHandle<()>>) = {
            let mut st = self.inner.st.lock();
            st.shutting_down = true;
            let mut c = Vec::new();
            let mut h = Vec::new();
            for e in st.runs.values_mut() {
                if let Some(ctl) = &e.ctl {
                    c.push(ctl.clone());
                }
                if let Some(t) = e.thread.take() {
                    h.push(t);
                }
            }
            (c, h)
        };
        for c in ctls {
            let _ = c.send(Cmd::Abort);
        }
        for h in handles {
            let _ = h.join();
        }
    }
}

impl Inner {
    fn emit_state(&self, info: RunInfo) {
        self.broadcast(RunUpdate::State(info));
    }

    fn broadcast(&self, u: RunUpdate) {
        let mut subs = self.subs.lock();
        subs.retain(|s| s.send(u.clone()).is_ok());
    }

    fn stored(&self, id: &str) -> Result<Stored> {
        self.st.lock().runs.get(id).map(|e| e.stored.clone()).ok_or_else(|| anyhow!("unknown run {id}"))
    }

    fn head_of(&self, s: &Stored) -> Result<String> {
        if let Some(h) = &s.head_sha {
            return Ok(h.clone());
        }
        if s.state == "queued" {
            return Err(anyhow!("run {} has not started", s.id));
        }
        match (&s.branch, s.isolate) {
            (Some(b), true) => gitrun::out(&s.repo, &["rev-parse", "--verify", &format!("refs/heads/{b}")]),
            _ => gitrun::out(&s.repo, &["rev-parse", "--verify", &artifact::run_ref(&s.id)]).or_else(|_| Ok(s.base_sha.clone())),
        }
    }

    /// Mutates + persists a record, then broadcasts the new state.
    fn update(&self, id: &str, f: impl FnOnce(&mut Stored)) -> Option<RunInfo> {
        let info = {
            let mut st = self.st.lock();
            let e = st.runs.get_mut(id)?;
            f(&mut e.stored);
            let _ = persist::save(&self.data, &e.stored);
            e.stored.info()
        };
        self.emit_state(info.clone());
        Some(info)
    }

    fn log_error(&self, id: &str, msg: &str) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(persist::run_dir(&self.data, id).join("errors.log")) {
            let _ = writeln!(f, "{} {msg}", ct_store::now_ms());
        }
    }
}

/// Starts queued runs while below the concurrency limit.
fn pump(inner: &Arc<Inner>) {
    let mut started: Vec<(String, Receiver<Cmd>, RunInfo)> = Vec::new();
    let mut spawn_sampler = false;
    {
        let mut st = inner.st.lock();
        if st.shutting_down {
            return;
        }
        let budget_kb = inner.max_total_rss_mb.load(std::sync::atomic::Ordering::Relaxed).saturating_mul(1024);
        while st.running < st.max {
            // global budget: while the running runs already use too much, new ones stay Queued
            // (with nothing running they always start, so the queue cannot deadlock)
            if st.running > 0 && budget_kb > 0 && inner.res.lock().values().map(|r| r.rss_kb).sum::<u64>() > budget_kb {
                break;
            }
            let Some(id) = st.queue.pop_front() else { break };
            let Some(e) = st.runs.get_mut(&id) else { continue };
            if e.stored.state != "queued" {
                continue;
            }
            e.stored.set_state(&RunState::Running);
            e.stored.started_ms = ct_store::now_ms();
            let _ = persist::save(&inner.data, &e.stored);
            let (tx, rx) = channel();
            e.ctl = Some(tx);
            let info = e.stored.info();
            st.running += 1;
            started.push((id, rx, info));
        }
        if st.running > 0 && !st.sampler_active {
            st.sampler_active = true;
            spawn_sampler = true;
        }
    }
    if spawn_sampler {
        let i2 = inner.clone();
        if std::thread::Builder::new().name("run-sampler".into()).spawn(move || sampler(i2)).is_err() {
            inner.st.lock().sampler_active = false;
        }
    }
    for (id, rx, info) in started {
        inner.emit_state(info);
        let i2 = inner.clone();
        let id2 = id.clone();
        let h = std::thread::Builder::new().name(format!("run-{id}")).spawn(move || supervise(&i2, &id2, &rx));
        let mut st = inner.st.lock();
        match h {
            Ok(h) => {
                if let Some(e) = st.runs.get_mut(&id) {
                    e.thread = Some(h);
                }
            }
            Err(e) => {
                drop(st);
                finish(inner, &id, RunState::Failed(format!("cannot start supervisor thread: {e}")), None);
            }
        }
    }
}

/// Final state transition: releases the concurrency slot and starts the next queued run.
fn finish(inner: &Arc<Inner>, id: &str, state: RunState, extra: Option<Box<dyn FnOnce(&mut Stored)>>) {
    // Terminal transitions are serialised with abort(): an abort that was accepted wins over
    // whatever the agent reported; once finalising starts, abort() is refused.
    let state = {
        let mut st = inner.st.lock();
        match st.runs.get_mut(id) {
            Some(e) => {
                e.finalising = true;
                if e.abort_req {
                    match e.limit.take() {
                        Some(l) => RunState::Failed(l),
                        None => RunState::Aborted,
                    }
                } else {
                    state
                }
            }
            None => state,
        }
    };
    inner.update(id, |s| {
        s.set_state(&state);
        s.ended_ms = Some(ct_store::now_ms());
        s.pid = None;
        s.pid_start = None;
        if let Some(f) = extra {
            f(s);
        }
    });
    {
        let mut st = inner.st.lock();
        if let Some(e) = st.runs.get_mut(id) {
            e.ctl = None;
        }
        st.running = st.running.saturating_sub(1);
        inner.sampler_wake.notify_all();
    }
    inner.res.lock().remove(id);
    pump(inner);
}

enum End {
    Settled { ok: bool, error: Option<String> },
    Exited(Option<i32>),
    Disconnected,
    /// Abort completed without a Settled/Exited event (process confirmed dead or kill stages ran out).
    Killed,
}

fn supervise(inner: &Arc<Inner>, id: &str, cmds: &Receiver<Cmd>) {
    let Ok(s) = inner.stored(id) else { return };
    let timings = inner.timings;
    let backend = match s.backend() {
        Ok(b) => b,
        Err(e) => return finish(inner, id, RunState::Failed(format!("{e:#}")), None),
    };
    let model = match s.model() {
        Ok(m) => m,
        Err(e) => return finish(inner, id, RunState::Failed(format!("{e:#}")), None),
    };
    let run_dir = persist::run_dir(&inner.data, id);

    // ---- isolation
    let (cwd, branch, worktree): (PathBuf, Option<String>, Option<PathBuf>) = if s.isolate {
        let short = &id[id.len().saturating_sub(6)..];
        let branch = format!("ct/{}-{short}", artifact::slug(&s.prompt));
        let wt = artifact::worktree_path(&inner.data, &s.repo, id);
        // Record intent first so a crash mid-creation is cleaned by discard.
        inner.update(id, |r| {
            r.branch = Some(branch.clone());
            r.worktree = Some(wt.clone());
        });
        match artifact::create_worktree(&inner.data, &s.repo, id, &branch, &s.base_sha) {
            Ok(wt) => (wt.clone(), Some(branch), Some(wt)),
            Err(e) => {
                let msg = format!("worktree: {e:#}");
                return finish(
                    inner,
                    id,
                    RunState::Failed(msg),
                    Some(Box::new(|r: &mut Stored| {
                        r.branch = None;
                        r.worktree = None;
                    })),
                );
            }
        }
    } else {
        (s.repo.clone(), None, None)
    };

    let abandon = |msg: String| {
        // Nothing to review: drop the worktree again so no half-state remains.
        if let (Some(wt), Some(b)) = (&worktree, &branch) {
            if let Err(e) = artifact::remove_worktree(&inner.data, &s.repo, wt, Some(b)) {
                inner.log_error(id, &format!("cleanup after failure: {e:#}"));
            }
        }
        finish(
            inner,
            id,
            RunState::Failed(msg),
            Some(Box::new(|r: &mut Stored| {
                if r.isolate {
                    r.branch = None;
                    r.worktree = None;
                }
            })),
        );
    };

    // ---- session
    let opts = SessionOpts { cwd: cwd.clone(), model: Some(model), read_only: false, env_allow: vec![], system_note: None };
    let mut sess = match inner.factory.start(backend, opts) {
        Ok(s) => s,
        Err(e) => return abandon(format!("cannot start agent: {e}")),
    };
    let pid = sess.pid();
    let pid_start = pid.and_then(process_start_time);
    inner.update(id, |r| {
        r.pid = pid;
        r.pid_start = pid_start;
    });
    if let Err(e) = sess.prompt(&s.prompt) {
        stop_process(pid, pid_start, timings);
        sess.close();
        return abandon(format!("cannot send prompt: {e}"));
    }

    // ---- event loop
    let mut log = EventLog::open(&inner.data, id);
    let mut acc = reasons::Acc::default();
    let mut aborted = false;
    let end = event_loop(inner, id, &mut *sess, cmds, pid, pid_start, &mut log, &mut acc, &mut aborted);
    sess.close();
    cleanup_group(pid, pid_start);

    // ---- artifact + reasons
    let artifacts = (|| -> Result<(String, Vec<artifact::Change>)> {
        let head = match &worktree {
            Some(wt) => artifact::commit_worktree(wt, id)?,
            None => artifact::snapshot_worktree(&s.repo, &run_dir, id, &s.base_sha)?,
        };
        let ch = artifact::changes(&s.repo, &s.base_sha, &head)?;
        Ok((head, ch))
    })();
    let (head, changes) = match artifacts {
        Ok(x) => x,
        Err(e) => {
            return finish(inner, id, RunState::Failed(format!("artifact commit: {e:#}")), None);
        }
    };
    if !changes.is_empty() {
        let store_dir = ct_agent::gitx::find_repo(&s.repo).map(|r| r.git_dir).unwrap_or_else(|| s.repo.join(".git"));
        let ctx = reasons::ReasonCtx {
            run_id: id,
            backend,
            prompt: &s.prompt,
            base_sha: &s.base_sha,
            repo_root: &s.repo,
            store_dir: &store_dir,
            cwd: &cwd,
            events_log: &EventLog::path(&inner.data, id),
        };
        if let Err(e) = reasons::record_reasons(&ctx, &changes, &acc) {
            inner.log_error(id, &format!("auto reasons: {e:#}"));
        }
    }
    if !inner.timings.finalize_delay.is_zero() {
        std::thread::sleep(inner.timings.finalize_delay);
    }
    let state = if aborted {
        RunState::Aborted
    } else {
        match end {
            End::Settled { ok: true, .. } => RunState::Succeeded,
            End::Settled { ok: false, error } => RunState::Failed(error.unwrap_or_else(|| "agent reported failure".into())),
            End::Exited(c) => RunState::Failed(format!(
                "agent exited before finishing{}",
                c.map(|c| format!(" (code {c})")).unwrap_or_default()
            )),
            End::Disconnected => RunState::Failed("agent connection lost".into()),
            End::Killed => RunState::Aborted,
        }
    };
    let n = changes.len() as u32;
    finish(
        inner,
        id,
        state,
        Some(Box::new(move |r: &mut Stored| {
            r.head_sha = Some(head);
            r.files_changed = n;
        })),
    );
}

/// Escalation timer for an accepted abort: after `abort_grace` SIGTERM the group, after a further
/// `term_grace` SIGKILL it. Runs on its own thread, so it never waits on the session (whose
/// `abort()` may block) or on the event loop. Dropping it cancels any pending escalation.
struct Watchdog(Arc<std::sync::atomic::AtomicBool>);

impl Watchdog {
    fn start(pid: Option<u32>, start: Option<u64>, t: Timings) -> Self {
        use std::sync::atomic::Ordering::SeqCst;
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = cancel.clone();
        if let Some(p) = pid {
            let spawned = std::thread::Builder::new().name("run-abort-watchdog".into()).spawn(move || {
                let wait = |d: Duration| -> bool {
                    let end = Instant::now() + d;
                    while Instant::now() < end {
                        if c.load(SeqCst) || proc::everything_gone(p, start) {
                            return false;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    true
                };
                if wait(t.abort_grace) {
                    proc::signal_all(p, start, libc::SIGTERM);
                    if wait(t.term_grace) {
                        proc::signal_all(p, start, libc::SIGKILL);
                    }
                }
            });
            drop(spawned);
        }
        Watchdog(cancel)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[allow(clippy::too_many_arguments)]
fn event_loop(
    inner: &Arc<Inner>,
    id: &str,
    sess: &mut dyn AgentSession,
    cmds: &Receiver<Cmd>,
    pid: Option<u32>,
    pid_start: Option<u64>,
    log: &mut EventLog,
    acc: &mut reasons::Acc,
    aborted: &mut bool,
) -> End {
    let t = inner.timings;
    let mut abort_at: Option<Instant> = None;
    let mut _watchdog: Option<Watchdog> = None;
    loop {
        while let Ok(c) = cmds.try_recv() {
            match c {
                Cmd::Abort if abort_at.is_none() => {
                    *aborted = true;
                    abort_at = Some(Instant::now());
                    // timer first: it must not depend on how long the session takes to answer
                    _watchdog = Some(Watchdog::start(pid, pid_start, t));
                    let _ = sess.abort();
                }
                Cmd::Abort => {}
                Cmd::Steer(text, reply) => {
                    let _ = reply.send(sess.steer(&text).map_err(|e| e.to_string()));
                }
            }
        }
        match sess.events().recv_timeout(t.tick) {
            Ok(ev) => {
                log.append(&ev);
                acc.feed(&ev);
                inner.broadcast(RunUpdate::Event(id.to_string(), ev.clone()));
                match ev {
                    AgentEvent::Settled { ok, error } => return End::Settled { ok, error },
                    AgentEvent::Exited(c) => return End::Exited(c),
                    _ => {}
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return End::Disconnected,
        }
        if let Some(at) = abort_at {
            if pid.is_some_and(|p| !proc::is_alive(p, pid_start)) {
                return End::Killed;
            }
            if at.elapsed() >= t.abort_grace + t.term_grace + Duration::from_secs(2) {
                return End::Killed;
            }
        }
    }
}

/// The only sampler thread: alive while at least one run is running. Reads each run's process
/// group from `/proc`, publishes [`Resource`]s, enforces [`Limits`], and re-runs the queue (the
/// global RSS budget may have freed up).
fn sampler(inner: Arc<Inner>) {
    let mut tracks: HashMap<String, resources::Track> = HashMap::new();
    let mut last = Instant::now();
    loop {
        {
            // wakes early when the last run finishes, so the thread is gone promptly
            let mut st = inner.st.lock();
            if st.running > 0 {
                inner.sampler_wake.wait_for(&mut st, inner.timings.sample_interval);
            }
        }
        let dt = last.elapsed().as_secs_f64();
        last = Instant::now();
        let live: Vec<(String, u32, Option<u64>, i64)> = {
            let mut st = inner.st.lock();
            if st.running == 0 {
                st.sampler_active = false;
                drop(st);
                inner.res.lock().clear();
                return;
            }
            st.runs
                .iter()
                .filter(|(_, e)| e.stored.state == "running" && !e.finalising)
                .filter_map(|(id, e)| Some((id.clone(), e.stored.pid?, e.stored.pid_start, e.stored.started_ms)))
                .collect()
        };
        let limits = *inner.limits.lock();
        let now = ct_store::now_ms();
        let mut seen = Vec::with_capacity(live.len());
        for (id, pid, start, started_ms) in live {
            let usage = resources::group_usage(pid, start);
            let (res, reason) = tracks.entry(id.clone()).or_default().sample(&limits, usage, dt, now.saturating_sub(started_ms).max(0) as u64);
            inner.res.lock().insert(id.clone(), res);
            if let Some(r) = reason {
                limit_abort(&inner, &id, r);
            }
            seen.push(id);
        }
        tracks.retain(|id, _| seen.contains(id));
        inner.res.lock().retain(|id, _| seen.contains(id));
        pump(&inner);
    }
}

/// Stops a run that broke a resource limit through the normal staged abort; `finish` reports it
/// as Failed with the reason.
fn limit_abort(inner: &Arc<Inner>, id: &str, reason: String) {
    let ctl = {
        let mut st = inner.st.lock();
        let Some(e) = st.runs.get_mut(id) else { return };
        if e.stored.state != "running" || e.finalising || e.abort_req {
            return;
        }
        let Some(ctl) = e.ctl.clone() else { return };
        e.limit = Some(reason.clone());
        e.abort_req = true;
        ctl
    };
    inner.log_error(id, &reason);
    let _ = ctl.send(Cmd::Abort);
}

/// SIGTERM → grace → SIGKILL of the session's group and any survivors of a dead leader.
fn stop_process(pid: Option<u32>, start: Option<u64>, t: Timings) {
    if let Some(p) = pid {
        proc::terminate(p, start, t.term_grace, Duration::from_secs(2));
    }
}

/// After the session ended: nothing the agent started (dev servers, shells) may outlive the run.
fn cleanup_group(pid: Option<u32>, start: Option<u64>) {
    if let Some(p) = pid {
        proc::terminate(p, start, Duration::from_millis(500), Duration::from_secs(2));
    }
}

/// A run left `running` by a previous process: kill whatever is still alive — including group
/// members that outlived a dead leader — mark Interrupted and keep the agent's uncommitted work as
/// an artifact commit (isolated runs only). Artifacts are committed only after the group is gone.
fn reconcile_orphan(data: &Path, s: &mut Stored, t: Timings) {
    let mut group_gone = true;
    if let Some(p) = s.pid {
        group_gone = proc::terminate(p, s.pid_start, t.term_grace, Duration::from_secs(2));
    }
    s.set_state(&RunState::Interrupted);
    s.ended_ms = Some(ct_store::now_ms());
    if group_gone {
        s.pid = None;
        s.pid_start = None;
    }
    if let (true, true, Some(wt)) = (group_gone, s.isolate, s.worktree.clone()) {
        if wt.is_dir() && artifact::check_worktree_path(data, &wt).is_ok() {
            if let Ok(head) = artifact::commit_worktree(&wt, &s.id) {
                if let Ok(ch) = artifact::changes(&s.repo, &s.base_sha, &head) {
                    s.files_changed = ch.len() as u32;
                }
                s.head_sha = Some(head);
            }
        }
    }
}
