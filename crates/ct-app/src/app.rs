//! Application state, async plumbing (PLAN §9.2) and actions. Panels live in `ui_*.rs`.

use crate::diffmodel::{DiffModel, LARGE_DIFF_LINES};
use crate::editor::{self, Disk, FileStamp, Loaded, ReadOnly, SaveOutcome};
use crate::highlight::{self, Span};
use crate::jobs::{JobCtx, JobId, JobKind, Jobs};
use crate::selection::{at_commit, RowSel};
use crate::settings::Settings;
use crate::theme::{Theme, ThemeFile};
use ct_agent::ask as agent_ask;
use ct_core::{BlameLine, CodeRef, CommitMeta, Comparison, DiffOpts, DiffSet, FileIndex, LogQuery, RefAt, RefInfo, Repo, SearchHit, SearchQuery, SearchStats, Treeish};
use ct_store::{Confidence, Record, Store};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub type Ctx = JobCtx<Msg>;
pub const PAGE: usize = 200;
pub const MAX_SEARCH_HITS: usize = 5000;

#[derive(Default)]
pub enum Loadable<T> {
    #[default]
    Idle,
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Loadable<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Loadable::Ready(t) => Some(t),
            _ => None,
        }
    }
    pub fn is_loading(&self) -> bool {
        matches!(self, Loadable::Loading)
    }
}

// ---------------------------------------------------------------- base / comparison

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TargetSel {
    Commit(String),
    Worktree,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BaseMode {
    Parent,
    Ref(String),
    MergeBase(String),
    Previous,
    WorkingTree,
}

impl BaseMode {
    pub fn label(&self, parent_n: u8, multi_parent: bool) -> String {
        match self {
            BaseMode::Parent if multi_parent => format!("parent {parent_n}"),
            BaseMode::Parent => "parent".into(),
            BaseMode::Ref(r) => r.clone(),
            BaseMode::MergeBase(r) => format!("merge-base({r})"),
            BaseMode::Previous => "previous selection".into(),
            BaseMode::WorkingTree => "working tree".into(),
        }
    }
}

pub struct CmpInput<'a> {
    pub base: &'a BaseMode,
    pub range: Option<(&'a str, &'a str)>,
    pub parent_n: u8,
    pub target: &'a TargetSel,
    pub prev: Option<&'a str>,
    pub head: &'a str,
}

/// UI base mode -> ct-core `Comparison`. `merge_base` is injected so this stays unit-testable.
pub fn build_comparison(i: &CmpInput, merge_base: &dyn Fn(&str, &str) -> ct_core::Result<String>) -> ct_core::Result<Comparison> {
    if let Some((a, b)) = i.range {
        return Comparison::range(a, b);
    }
    let sha = match i.target {
        TargetSel::Worktree => return Comparison::commit_vs_worktree(i.head),
        TargetSel::Commit(s) => s.as_str(),
    };
    match i.base {
        BaseMode::Parent => Comparison::parent_n_of(sha, i.parent_n.max(1)),
        BaseMode::Ref(r) => Comparison::range(r, sha),
        BaseMode::MergeBase(r) => Comparison::range(&merge_base(r, sha)?, sha),
        BaseMode::Previous => match i.prev {
            Some(p) if p != sha => Comparison::range(p, sha),
            _ => Comparison::parent_n_of(sha, i.parent_n.max(1)),
        },
        BaseMode::WorkingTree => Comparison::commit_vs_worktree(sha),
    }
}

#[derive(Clone, Debug)]
pub struct CurCmp {
    pub cmp: Comparison,
    pub old_at: Option<RefAt>,
    pub new_at: Option<RefAt>,
    pub old_label: String,
    pub new_label: String,
}

fn treeish_label(t: &Treeish) -> String {
    match t {
        Treeish::Commit(c) => crate::timefmt::short(c).to_string(),
        Treeish::Parent { commit, n } => format!("{}^{}", crate::timefmt::short(commit), n),
        Treeish::Index => "index".into(),
        Treeish::Worktree => "worktree".into(),
    }
}

// ---------------------------------------------------------------- selection / why

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelSource {
    Diff,
    Blame,
    Search,
    Editor,
}

#[derive(Clone, Debug)]
pub struct Sel {
    pub code_ref: CodeRef,
    pub source: SelSource,
    /// Selected text (new side), for the prompt preview.
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct WhyItem {
    pub rec: Record,
    /// `None` = linked only by line-range overlap in the worktree (no commit linking).
    pub conf: Option<Confidence>,
    pub reason: String,
}

pub struct Prepared {
    pub model: DiffModel,
    /// Per hunk: records linked by ct-store.
    pub why: Vec<Vec<(Record, Confidence)>>,
}

// ---------------------------------------------------------------- messages

pub struct Opened {
    pub repo: Repo,
    pub head: String,
    pub head_name: String,
    pub refs: Vec<RefInfo>,
    pub store: Option<Arc<Store>>,
    pub store_note: Option<String>,
}

pub struct BlameData {
    pub lines: Vec<BlameLine>,
    pub spans: Vec<Vec<Span>>,
}

pub enum Msg {
    Opened(Result<Opened, String>),
    Log { token: u64, append: bool, res: Result<Vec<CommitMeta>, String> },
    Diff { res: Result<(DiffSet, CurCmp), String> },
    FileDiff { path: String, gen: u64, res: Result<Box<Prepared>, String> },
    Hits(Vec<SearchHit>),
    SearchDone(Result<SearchStats, String>),
    Files(Result<(Arc<Vec<String>>, Arc<FileIndex>), String>),
    Blame { path: String, res: Result<Box<BlameData>, String> },
    History { path: String, line: u32, res: Result<Vec<CommitMeta>, String> },
    FileLoaded { path: String, res: Result<Loaded, String> },
    Saved { path: String, written: String, res: Result<SaveOutcome, String> },
    Disk { path: String, res: Disk },
    Prompt(Result<agent_ask::Prompt, String>),
    AskChunk(String),
    AskDone(Result<(), String>),
    Browse { token: u64, res: Result<crate::projects::Listing, String> },
    Accounts { accounts: Vec<crate::agents::Account>, runtime: crate::agents::RuntimeStatus },
    RuntimeProgress(String),
    RuntimeDone(Result<(), String>),
    Models { backend: crate::agents::Backend, res: Result<Vec<crate::agents::ModelInfo>, String> },
    LoginSession(Arc<dyn crate::agents::LoginSession>),
    LoginEv(crate::agents::LoginEvent),
    LoggedOut { id: String, res: Result<(), String> },
    RunsOpened(Result<(Arc<dyn crate::agents::RunService>, Vec<crate::agents::RunInfo>), String>),
    RunUpd(crate::agents::RunUpdate),
    RunSubmitted(Result<String, String>),
    RunControl { what: &'static str, res: Result<(), String> },
    RunFinished { what: &'static str, res: Result<String, String> },
    RunReview { res: Result<(String, String), String> },
}

// ---------------------------------------------------------------- panel state

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RailTab {
    Commits,
    Search,
    Files,
    Runs,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Centre {
    Diff,
    Run,
    Blame,
    Editor,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InspTab {
    Why,
    Blame,
    Ask,
}

#[derive(Default)]
pub struct SearchState {
    pub query: String,
    pub globs: String,
    pub regex: bool,
    pub case: bool,
    pub running: bool,
    pub groups: Vec<SearchGroup>,
    pub total: usize,
    pub capped: bool,
    pub stats: Option<SearchStats>,
    pub error: Option<String>,
    pub selected: Option<(usize, usize)>,
    pub edited_at: Option<Instant>,
    pub focus: bool,
    /// Query that produced `groups` (to avoid re-running on repaint).
    pub ran: Option<(String, bool, bool, String)>,
    pub started: Option<Instant>,
}

pub struct SearchGroup {
    pub path: String,
    pub hits: Vec<SearchHit>,
    pub collapsed: bool,
}

#[derive(Default)]
pub struct FilesState {
    pub all: Loadable<(Arc<Vec<String>>, Arc<FileIndex>)>,
    pub filter: String,
    pub results: Vec<(String, u32, Vec<u32>)>,
    pub filtered_for: String,
    pub selected: usize,
}

pub struct EditorState {
    pub path: String,
    pub abs: PathBuf,
    pub load: Loadable<()>,
    pub text: String,
    pub saved_text: String,
    pub stamp: Option<FileStamp>,
    pub crlf: bool,
    pub read_only: Option<ReadOnly>,
    pub conflict: Option<Disk>,
    pub notice: Option<(String, Instant)>,
    pub goto: Option<u32>,
    pub goto_open: bool,
    pub goto_input: String,
    pub last_check: Instant,
    pub checking: bool,
    pub saving: bool,
    pub layout_cache: Option<(u64, egui::text::LayoutJob)>,
    pub cursor_line: u32,
    pub spans_key: u64,
}

impl EditorState {
    pub fn dirty(&self) -> bool {
        self.text != self.saved_text
    }
}

#[derive(Default)]
pub struct BlameState {
    pub path: String,
    pub rev: Option<String>,
    pub data: Loadable<Box<BlameData>>,
    pub selected: Option<(usize, usize)>,
    pub scroll_to: Option<usize>,
}

#[derive(Default)]
pub struct HistoryState {
    pub path: String,
    pub line: u32,
    pub data: Loadable<Vec<CommitMeta>>,
}

#[derive(Default)]
pub enum AskPhase {
    #[default]
    Idle,
    Building,
    Preview(Box<agent_ask::Prompt>),
    Running,
    Done,
    Failed(String),
}

pub struct AskState {
    pub question: String,
    pub phase: AskPhase,
    pub output: String,
    pub started: Option<Instant>,
    pub for_ref: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaletteMode {
    Files,
    Commands,
}

#[derive(Default)]
pub struct PaletteState {
    pub open: Option<PaletteMode>,
    pub input: String,
    pub sel: usize,
    pub focus: bool,
}

#[derive(Default)]
pub struct BasePopover {
    pub open: bool,
    pub range_a: String,
    pub range_b: String,
    pub ref_input: String,
    pub merge_input: String,
}

pub struct Smoke {
    pub scene: String,
    pub out: PathBuf,
    pub query: String,
    pub started: Instant,
    pub stable: u32,
    pub settled_at: Option<Instant>,
    pub requested: bool,
    pub prepared: bool,
}

pub struct App {
    pub pj: crate::projectapp::ProjectState,
    pub ag: crate::agentapp::AgentState,
    pub th: Theme,
    pub settings: Settings,
    pub banners: Vec<(crate::widgets::Level, String)>,
    pub fonts_note: String,
    pub jobs: Jobs<Msg>,
    pub ctx: egui::Context,

    pub repo_arg: PathBuf,
    pub repo: Option<Repo>,
    pub open_state: Loadable<()>,
    pub head: String,
    pub head_name: String,
    pub refs: Vec<RefInfo>,
    pub store: Option<Arc<Store>>,
    pub store_note: Option<String>,

    pub commits: Vec<CommitMeta>,
    pub commits_loading: bool,
    pub commits_done: bool,
    pub commits_err: Option<String>,
    pub commit_token: u64,
    pub commit_filter: String,
    pub commit_filter_mode: usize,
    pub commit_filter_applied: (String, usize),
    pub commit_filter_at: Option<Instant>,
    pub commit_scroll_to: Option<usize>,
    pub target: Option<TargetSel>,
    pub prev_sha: Option<String>,

    pub base: BaseMode,
    pub parent_n: u8,
    pub range: Option<(String, String)>,
    pub base_pop: BasePopover,
    pub cur: Option<CurCmp>,
    pub diffset: Loadable<DiffSet>,
    pub diff_opts: DiffOpts,
    pub file_sel: Option<String>,
    pub file_filter: String,
    pub prepared: Loadable<Box<Prepared>>,
    pub show_large: bool,
    pub diff_sel: Option<RowSel>,
    pub diff_anchor: Option<usize>,
    pub diff_scroll_to: Option<usize>,
    pub diff_top_row: usize,
    pub cmp_gen: u64,
    pub base_chip_rect: Option<egui::Rect>,

    pub rail: RailTab,
    pub centre: Centre,
    pub insp: InspTab,
    pub insp_open: bool,
    pub selection: Option<Sel>,
    pub why: Vec<WhyItem>,
    pub search: SearchState,
    pub files: FilesState,
    pub palette: PaletteState,
    pub editor: Option<EditorState>,
    pub blame: BlameState,
    pub history: HistoryState,
    pub ask: AskState,
    pub settings_open: bool,
    pub settings_dirty_at: Option<Instant>,
    pub status_msg: Option<(String, Instant)>,
    pub smoke: Option<Smoke>,
    pub last_search_stats_line: String,
}

pub fn short_repo_name(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

impl App {
    pub fn new(ctx: egui::Context, repo_arg: PathBuf, theme_file: ThemeFile, settings: Settings, banners: Vec<(crate::widgets::Level, String)>, fonts_note: String, smoke: Option<Smoke>, svc: Arc<dyn crate::agents::AgentService>) -> App {
        let th = Theme { file: theme_file, ui_font: settings.ui_font_size, code_font: settings.code_font_size };
        let rc = ctx.clone();
        let jobs = Jobs::new(move || rc.request_repaint());
        let mut app = App {
            pj: Default::default(),
            ag: crate::agentapp::AgentState::new(svc),
            th,
            diff_opts: DiffOpts::default(),
            ask: AskState { question: String::new(), phase: AskPhase::Idle, output: String::new(), started: None, for_ref: None },
            settings,
            banners,
            fonts_note,
            jobs,
            ctx,
            repo_arg,
            repo: None,
            open_state: Loadable::Idle,
            head: String::new(),
            head_name: String::new(),
            refs: Vec::new(),
            store: None,
            store_note: None,
            commits: Vec::new(),
            commits_loading: false,
            commits_done: false,
            commits_err: None,
            commit_token: 0,
            commit_filter: String::new(),
            commit_filter_mode: 0,
            commit_filter_applied: (String::new(), 0),
            commit_filter_at: None,
            commit_scroll_to: None,
            target: None,
            prev_sha: None,
            base: BaseMode::Parent,
            parent_n: 1,
            range: None,
            base_pop: BasePopover::default(),
            cur: None,
            diffset: Loadable::Idle,
            file_sel: None,
            file_filter: String::new(),
            prepared: Loadable::Idle,
            show_large: false,
            diff_sel: None,
            diff_anchor: None,
            diff_scroll_to: None,
            diff_top_row: 0,
            cmp_gen: 0,
            base_chip_rect: None,
            rail: RailTab::Commits,
            centre: Centre::Diff,
            insp: InspTab::Why,
            insp_open: true,
            selection: None,
            why: Vec::new(),
            search: SearchState { case: true, ..Default::default() },
            files: FilesState::default(),
            palette: PaletteState::default(),
            editor: None,
            blame: BlameState::default(),
            history: HistoryState::default(),
            settings_open: false,
            settings_dirty_at: None,
            status_msg: None,
            smoke,
            last_search_stats_line: String::new(),
        };
        if app.repo_arg.as_os_str().is_empty() {
            app.pj_open_browser();
        } else {
            app.open_repo();
        }
        app
    }

    // ------------------------------------------------------------ helpers

    pub fn git_timeout(&self) -> Duration {
        Duration::from_secs(self.settings.git_timeout_secs)
    }

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.status_msg = Some((msg.into(), Instant::now()));
    }

    /// True while any async work is outstanding (used by smoke mode to know a scene settled).
    pub fn busy(&self) -> bool {
        self.open_state.is_loading()
            || self.commits_loading
            || self.diffset.is_loading()
            || self.prepared.is_loading()
            || self.search.running
            || self.files.all.is_loading()
            || self.blame.data.is_loading()
            || self.editor.as_ref().is_some_and(|e| e.load.is_loading())
            || matches!(self.ask.phase, AskPhase::Building | AskPhase::Running)
    }

    pub fn commit_by_sha(&self, sha: &str) -> Option<&CommitMeta> {
        self.commits.iter().find(|c| c.sha == sha)
    }

    pub fn selected_commit(&self) -> Option<&CommitMeta> {
        match &self.target {
            Some(TargetSel::Commit(s)) => self.commit_by_sha(s),
            _ => None,
        }
    }

    pub fn is_merge_selected(&self) -> bool {
        self.selected_commit().is_some_and(|c| c.parents.len() > 1)
    }

    pub fn base_label(&self) -> String {
        if let Some((a, b)) = &self.range {
            return format!("{}..{}", short_ref(a), short_ref(b));
        }
        if matches!(self.target, Some(TargetSel::Worktree)) {
            return "HEAD".into();
        }
        self.base.label(self.parent_n, self.is_merge_selected())
    }

    // ------------------------------------------------------------ open repo

    pub fn open_repo(&mut self) {
        self.open_state = Loadable::Loading;
        let path = self.repo_arg.clone();
        let timeout = self.git_timeout();
        self.jobs.spawn(JobKind::Open, move |c| {
            let res = (|| -> Result<Opened, String> {
                let repo = Repo::open(&path).map_err(|e| e.to_string())?.with_timeout(timeout);
                let head = repo.head().unwrap_or_default();
                let refs = repo.refs().map_err(|e| e.to_string())?;
                let head_name = refs.iter().find(|r| r.is_head).map(|r| r.name.clone()).unwrap_or_else(|| "(detached)".into());
                let (store, store_note) = match Store::open(&store_dir(&repo.root, &repo.common_dir)) {
                    Ok(s) => (Some(Arc::new(s)), None),
                    Err(e) => (None, Some(e.to_string())),
                };
                Ok(Opened { repo, head, head_name, refs, store, store_note })
            })();
            c.finish(Msg::Opened(res));
        });
    }

    pub fn request_log(&mut self, append: bool) {
        let Some(repo) = self.repo.clone() else { return };
        if self.commits_loading && append {
            return;
        }
        self.commits_loading = true;
        self.commits_err = None;
        self.commit_token += 1;
        let token = self.commit_token;
        let (text, mode) = self.commit_filter_applied.clone();
        let skip = if append { self.commits.len() } else { 0 };
        let timeout = self.git_timeout();
        self.jobs.spawn(JobKind::Log, move |c| {
            let mut q = LogQuery { skip, limit: PAGE, all_refs: true, ..Default::default() };
            if !text.trim().is_empty() {
                if mode == 0 {
                    q.message = Some(text.trim().to_string());
                } else {
                    q.author = Some(text.trim().to_string());
                }
            }
            let repo = repo.with_timeout(timeout).with_cancel(c.cancel.clone());
            c.finish(Msg::Log { token, append, res: repo.log(&q).map_err(|e| e.to_string()) });
        });
    }

    // ------------------------------------------------------------ comparison / diff

    pub fn select_commit(&mut self, sha: &str) {
        if self.target.as_ref() == Some(&TargetSel::Commit(sha.to_string())) {
            return;
        }
        if let Some(TargetSel::Commit(old)) = &self.target {
            self.prev_sha = Some(old.clone());
        }
        self.target = Some(TargetSel::Commit(sha.to_string()));
        self.parent_n = 1;
        self.after_target_change();
    }

    pub fn select_worktree(&mut self) {
        if let Some(TargetSel::Commit(old)) = &self.target {
            self.prev_sha = Some(old.clone());
        }
        self.target = Some(TargetSel::Worktree);
        self.after_target_change();
    }

    fn after_target_change(&mut self) {
        self.clear_selection();
        self.request_diff();
    }

    pub fn set_base(&mut self, b: BaseMode) {
        self.base = b;
        self.range = None;
        self.clear_selection();
        self.request_diff();
    }

    pub fn set_range(&mut self, a: String, b: String) {
        self.range = Some((a, b));
        self.clear_selection();
        self.request_diff();
    }

    pub fn request_diff(&mut self) {
        let (Some(repo), Some(target)) = (self.repo.clone(), self.target.clone()) else { return };
        self.diffset = Loadable::Loading;
        self.prepared = Loadable::Idle;
        let base = self.base.clone();
        let range = self.range.clone();
        let parent_n = self.parent_n;
        let prev = self.prev_sha.clone();
        let head = self.head.clone();
        let opts = self.diff_opts.clone();
        let timeout = self.git_timeout();
        self.cmp_gen += 1;
        self.jobs.cancel(JobKind::FileDiff);
        self.jobs.spawn(JobKind::Diff, move |c| {
            let repo = repo.with_timeout(timeout).with_cancel(c.cancel.clone());
            let res = (|| -> Result<(DiffSet, CurCmp), String> {
                let input = CmpInput { base: &base, range: range.as_ref().map(|(a, b)| (a.as_str(), b.as_str())), parent_n, target: &target, prev: prev.as_deref(), head: &head };
                let cmp = build_comparison(&input, &|a, b| repo.merge_base(a, b)).map_err(|e| e.to_string())?;
                let set = repo.diff(&cmp, &opts).map_err(|e| e.to_string())?;
                let at_of = |t: &Treeish| -> Option<RefAt> {
                    match t {
                        Treeish::Commit(r) => repo.resolve(r).ok().map(|o| at_commit(&o)),
                        Treeish::Parent { commit, n } => repo.resolve(&format!("{commit}^{n}")).ok().map(|o| at_commit(&o)),
                        Treeish::Index => None,
                        Treeish::Worktree => Some(RefAt::Worktree),
                    }
                };
                let cur = CurCmp { old_at: at_of(&cmp.old), new_at: at_of(&cmp.new), old_label: treeish_label(&cmp.old), new_label: treeish_label(&cmp.new), cmp };
                Ok((set, cur))
            })();
            c.finish(Msg::Diff { res });
        });
    }

    pub fn open_file_diff(&mut self, path: &str) {
        self.file_sel = Some(path.to_string());
        self.show_large = false;
        self.diff_sel = None;
        self.diff_anchor = None;
        self.request_file_diff();
        if self.centre == Centre::Editor || self.centre == Centre::Blame {
            // keep the centre; user switches explicitly
        }
    }

    pub fn request_file_diff(&mut self) {
        let (Some(repo), Some(cur), Some(path)) = (self.repo.clone(), self.cur.clone(), self.file_sel.clone()) else { return };
        self.prepared = Loadable::Loading;
        let opts = self.diff_opts.clone();
        let store = self.store.clone();
        let tab = self.settings.tab_width;
        let timeout = self.git_timeout();
        let gen = self.cmp_gen;
        self.jobs.spawn(JobKind::FileDiff, move |c| {
            let repo = repo.with_timeout(timeout).with_cancel(c.cancel.clone());
            let res = (|| -> Result<Box<Prepared>, String> {
                let file = repo.file_diff(&cur.cmp, &path, &opts).map_err(|e| e.to_string())?;
                let blobs: Vec<String> = file.new_oid.iter().cloned().collect();
                let model = DiffModel::build(file, tab);
                let why = (0..model.file.hunks.len())
                    .map(|h| store.as_ref().map(|s| s.match_hunk(&model.file.path, &model.added_lines(h), &blobs)).unwrap_or_default())
                    .collect();
                Ok(Box::new(Prepared { model, why }))
            })();
            c.finish(Msg::FileDiff { path, gen, res });
        });
    }

    // ------------------------------------------------------------ selection

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.diff_sel = None;
        self.diff_anchor = None;
        self.why.clear();
    }

    pub fn set_selection(&mut self, code_ref: CodeRef, source: SelSource, text: String) {
        self.why = self.compute_why(&code_ref, source);
        self.selection = Some(Sel { code_ref, source, text });
        if self.ask.for_ref.as_deref() != self.selection.as_ref().map(|s| s.code_ref.to_string()).as_deref() && !matches!(self.ask.phase, AskPhase::Running | AskPhase::Building | AskPhase::Preview(_)) {
            self.ask.phase = AskPhase::Idle;
            self.ask.output.clear();
        }
    }

    fn compute_why(&self, r: &CodeRef, source: SelSource) -> Vec<WhyItem> {
        let Some(store) = &self.store else { return Vec::new() };
        let mut items: Vec<WhyItem> = Vec::new();
        let mut push = |rec: Record, conf: Option<Confidence>| {
            if let Some(e) = items.iter_mut().find(|i| i.rec.id == rec.id) {
                if conf < e.conf {
                    e.conf = conf;
                }
                return;
            }
            let reason = ct_store::decode_reason(&rec);
            items.push(WhyItem { rec, conf, reason });
        };
        match (source, self.prepared.ready()) {
            (SelSource::Diff, Some(p)) if self.file_sel.as_deref() == Some(r.path.as_str()) || p.model.file.old_path.as_deref() == Some(r.path.as_str()) => {
                let hunks: Vec<usize> = match self.diff_sel {
                    Some(s) if !p.model.lines.is_empty() => {
                        let (lo, hi) = (s.lo().min(p.model.lines.len() - 1), s.hi().min(p.model.lines.len() - 1));
                        let mut h: Vec<usize> = p.model.lines[lo..=hi].iter().map(|l| l.hunk).collect();
                        h.dedup();
                        h
                    }
                    _ => (0..p.why.len()).collect(),
                };
                for h in hunks {
                    for (rec, conf) in &p.why[h] {
                        push(rec.clone(), Some(*conf));
                    }
                }
            }
            _ => {
                for rec in store.for_range(&r.path, r.start, r.end, None) {
                    push(rec, None);
                }
            }
        }
        items.sort_by_key(|i| (i.conf.map_or(3, |c| c as u8), std::cmp::Reverse(i.rec.ts_ms)));
        items
    }

    /// Records for the whole selected file diff (no line selection).
    pub fn refresh_why_for_file(&mut self) {
        if self.selection.is_none() {
            if let (Some(p), Some(path)) = (self.prepared.ready(), self.file_sel.clone()) {
                let r = CodeRef { path, start: 1, end: 1, at: RefAt::Worktree };
                let _ = p;
                self.why = self.compute_why(&r, SelSource::Diff);
            }
        }
    }

    pub fn copy_ref(&mut self) {
        if let Some(s) = &self.selection {
            let text = s.code_ref.to_string();
            self.ctx.copy_text(text.clone());
            self.flash(format!("Copied {text}"));
        } else {
            self.flash("Select lines first (click a line, shift-click to extend).");
        }
    }

    // ------------------------------------------------------------ search / files

    pub fn start_search(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        let q = self.search.query.clone();
        if q.is_empty() {
            self.jobs.cancel(JobKind::Search);
            self.search.running = false;
            self.search.groups.clear();
            self.search.total = 0;
            self.search.stats = None;
            self.search.error = None;
            self.search.ran = None;
            return;
        }
        self.search.ran = Some((q.clone(), self.search.regex, self.search.case, self.search.globs.clone()));
        self.search.running = true;
        self.search.groups.clear();
        self.search.total = 0;
        self.search.capped = false;
        self.search.stats = None;
        self.search.error = None;
        self.search.selected = None;
        self.search.started = Some(Instant::now());
        let query = SearchQuery { pattern: q, regex: self.search.regex, case_sensitive: self.search.case, globs: self.search.globs.split_whitespace().map(String::from).collect() };
        let root = repo.root.clone();
        self.jobs.spawn(JobKind::Search, move |c| {
            let (tx, rx) = sync_channel::<SearchHit>(1024);
            let cancel = c.cancel.clone();
            let h = std::thread::spawn(move || ct_core::search_content(&root, &query, cancel, tx));
            let mut batch: Vec<SearchHit> = Vec::new();
            let mut sent = 0usize;
            let mut stopped = false;
            loop {
                match rx.recv_timeout(Duration::from_millis(30)) {
                    Ok(hit) => {
                        batch.push(hit);
                        if batch.len() >= 64 {
                            sent += batch.len();
                            if !c.stream(Msg::Hits(std::mem::take(&mut batch))) {
                                stopped = true;
                                break;
                            }
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if !batch.is_empty() {
                            sent += batch.len();
                            if !c.stream(Msg::Hits(std::mem::take(&mut batch))) {
                                stopped = true;
                                break;
                            }
                        }
                        if c.cancelled() {
                            stopped = true;
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if sent >= MAX_SEARCH_HITS {
                    break;
                }
            }
            if !batch.is_empty() && !stopped {
                c.stream(Msg::Hits(batch));
            }
            if stopped || sent >= MAX_SEARCH_HITS {
                c.cancel.store(true, Ordering::Relaxed);
            }
            drop(rx);
            let res = h.join().unwrap_or_else(|_| Err(ct_core::Error::Search("search thread panicked".into())));
            // the cancel flag may have been set by us (cap) — still report stats
            let _ = c.cancel.load(Ordering::Relaxed);
            c.finish_forced(Msg::SearchDone(res.map_err(|e| e.to_string())));
        });
    }

    pub fn request_files(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        self.files.all = Loadable::Loading;
        let timeout = self.git_timeout();
        self.jobs.spawn(JobKind::Files, move |c| {
            let repo = repo.with_timeout(timeout).with_cancel(c.cancel.clone());
            let res = repo.list_files().map_err(|e| e.to_string()).map(|v| {
                let idx = FileIndex::build(v.clone());
                (Arc::new(v), Arc::new(idx))
            });
            c.finish(Msg::Files(res));
        });
    }

    // ------------------------------------------------------------ editor / blame

    pub fn open_editor(&mut self, path: &str, line: Option<u32>) {
        let Some(repo) = self.repo.clone() else { return };
        self.centre = Centre::Editor;
        if let Some(e) = &mut self.editor {
            if e.path == path {
                if line.is_some() {
                    e.goto = line;
                }
                return;
            }
        }
        let abs = repo.root.join(path);
        let root = repo.root.clone();
        self.editor = Some(EditorState {
            path: path.to_string(),
            abs: abs.clone(),
            load: Loadable::Loading,
            text: String::new(),
            saved_text: String::new(),
            stamp: None,
            crlf: false,
            read_only: None,
            conflict: None,
            notice: None,
            goto: line,
            goto_open: false,
            goto_input: String::new(),
            last_check: Instant::now(),
            checking: false,
            saving: false,
            layout_cache: None,
            cursor_line: 1,
            spans_key: 0,
        });
        let p = path.to_string();
        self.jobs.spawn(JobKind::Load, move |c| {
            let res = editor::load_in(&root, &abs).map_err(|e| format!("Cannot open {p}: {e}"));
            c.finish(Msg::FileLoaded { path: p, res });
        });
    }

    pub fn reload_editor(&mut self) {
        let Some(e) = &mut self.editor else { return };
        let (abs, p) = (e.abs.clone(), e.path.clone());
        let root = self.repo.as_ref().map(|r| r.root.clone()).unwrap_or_default();
        e.load = Loadable::Loading;
        e.conflict = None;
        self.jobs.spawn(JobKind::Load, move |c| {
            let res = editor::load_in(&root, &abs).map_err(|er| format!("Cannot open {p}: {er}"));
            c.finish(Msg::FileLoaded { path: p, res });
        });
    }

    pub fn save_editor(&mut self, force: bool) {
        let root = self.repo.as_ref().map(|r| r.root.clone()).unwrap_or_default();
        let Some(e) = &mut self.editor else { return };
        if e.read_only.is_some() || e.saving || !matches!(e.load, Loadable::Ready(())) {
            return;
        }
        let Some(stamp) = e.stamp.clone() else { return };
        e.saving = true;
        let (abs, p, text, crlf) = (e.abs.clone(), e.path.clone(), e.text.clone(), e.crlf);
        self.jobs.spawn(JobKind::Save, move |c| {
            let res = editor::save_in(&root, &abs, &text, crlf, &stamp, force).map_err(|er| format!("Cannot save {p}: {er}"));
            c.finish(Msg::Saved { path: p, written: text, res });
        });
    }

    pub fn check_editor_disk(&mut self) {
        let Some(e) = &mut self.editor else { return };
        if e.checking || e.saving || e.read_only == Some(ReadOnly::Outside) || !matches!(e.load, Loadable::Ready(())) || e.last_check.elapsed() < Duration::from_millis(1000) {
            return;
        }
        let Some(stamp) = e.stamp.clone() else { return };
        e.last_check = Instant::now();
        e.checking = true;
        let (abs, p) = (e.abs.clone(), e.path.clone());
        self.jobs.spawn(JobKind::Watch, move |c| {
            c.finish(Msg::Disk { path: p, res: editor::check_disk(&abs, &stamp) });
        });
    }

    pub fn open_blame(&mut self, path: &str, line: Option<usize>) {
        let Some(repo) = self.repo.clone() else { return };
        self.centre = Centre::Blame;
        self.insp = InspTab::Blame;
        let at = match (&self.cur, self.target.as_ref()) {
            (Some(c), Some(TargetSel::Commit(_))) => match &c.new_at {
                Some(RefAt::Commit(sha)) => Some(sha.clone()),
                _ => None,
            },
            _ => None,
        };
        if self.blame.path == path && self.blame.rev == at && self.blame.data.ready().is_some() {
            self.blame.scroll_to = line;
            return;
        }
        self.blame = BlameState { path: path.to_string(), rev: at.clone(), data: Loadable::Loading, selected: None, scroll_to: line };
        let p = path.to_string();
        let timeout = self.git_timeout();
        self.jobs.spawn(JobKind::Blame, move |c| {
            let repo = repo.with_timeout(timeout).with_cancel(c.cancel.clone());
            let target = match &at {
                Some(sha) => Treeish::Commit(sha.clone()),
                None => Treeish::Worktree,
            };
            let res = repo.blame(&target, &p, None).map_err(|e| e.to_string()).map(|lines| {
                let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
                let spans = highlight::highlight(highlight::lang_for_path(&p), &texts);
                Box::new(BlameData { lines, spans })
            });
            c.finish(Msg::Blame { path: p, res });
        });
    }

    pub fn request_history(&mut self, path: &str, line: u32) {
        let Some(repo) = self.repo.clone() else { return };
        self.history = HistoryState { path: path.to_string(), line, data: Loadable::Loading };
        let p = path.to_string();
        let timeout = self.git_timeout();
        self.jobs.spawn(JobKind::History, move |c| {
            let repo = repo.with_timeout(timeout).with_cancel(c.cancel.clone());
            let res = repo.file_history(&p, Some((line, line)), 50).map_err(|e| e.to_string());
            c.finish(Msg::History { path: p, line, res });
        });
    }

    pub fn jump_to_commit(&mut self, sha: &str) {
        self.rail = RailTab::Commits;
        self.centre = Centre::Diff;
        if let Some(i) = self.commits.iter().position(|c| c.sha == sha) {
            self.commit_scroll_to = Some(i);
        }
        self.select_commit(sha);
    }

    // ------------------------------------------------------------ Ask AI

    pub fn start_ask(&mut self) {
        let (Some(repo), Some(sel)) = (self.repo.clone(), self.selection.clone()) else {
            self.flash("Select lines first, then Ask AI.");
            return;
        };
        self.insp_open = true;
        self.insp = InspTab::Ask;
        self.ask.phase = AskPhase::Building;
        self.ask.output.clear();
        self.ask.for_ref = Some(sel.code_ref.to_string());
        let question = self.ask.question.clone();
        self.jobs.spawn(JobKind::Prompt, move |c| {
            let res = (|| -> Result<agent_ask::Prompt, String> {
                let ctx = ct_agent::gitx::find_repo(&repo.root).ok_or_else(|| "not a git repository".to_string())?;
                let r = &sel.code_ref;
                let req = agent_ask::AskRequest {
                    code_ref: agent_ask::CodeRef {
                        path: r.path.clone(),
                        start: r.start,
                        end: r.end,
                        at: match &r.at {
                            RefAt::Commit(s) => agent_ask::RefAt::Commit(s.clone()),
                            RefAt::Worktree => agent_ask::RefAt::Worktree,
                        },
                    },
                    question: Some(question).filter(|q| !q.trim().is_empty()),
                    include: agent_ask::Include::default(),
                    include_secrets: false,
                };
                agent_ask::build_prompt(&ctx, &req).map_err(|e| e.to_string())
            })();
            c.finish(Msg::Prompt(res));
        });
    }

    pub fn send_ask(&mut self, prompt: &agent_ask::Prompt) {
        let Some(repo) = self.repo.clone() else { return };
        let sel = match self.ag_ask_target() {
            Ok(s) => s,
            Err(why) => return self.ag_ask_phase_failed(why),
        };
        self.ask.phase = AskPhase::Running;
        self.ask.output.clear();
        self.ask.started = Some(Instant::now());
        let text = prompt.render();
        let timeout = Duration::from_secs(self.settings.ask_timeout_secs);
        let svc = self.ag.svc.clone();
        self.jobs.spawn(JobKind::Ask, move |c| {
            let opts = crate::agents::SessionOpts {
                cwd: repo.root.clone(),
                model: Some(sel.clone()),
                read_only: true,
                env_allow: Vec::new(),
                system_note: Some("Answer from the context given. Read-only question: do not modify files or run commands that change anything.".into()),
            };
            let mut chunk = |s: &str| {
                c.stream(Msg::AskChunk(s.to_string()));
            };
            let res = crate::agentapp::run_ask_session(&*svc, sel.backend, opts, &text, &c.cancel, timeout, &mut chunk);
            c.finish_forced(Msg::AskDone(res));
        });
    }

    pub fn cancel_ask(&mut self) {
        self.jobs.cancel(JobKind::Ask);
        self.jobs.cancel(JobKind::Prompt);
        self.ask.phase = AskPhase::Failed("Cancelled.".into());
    }

    // ------------------------------------------------------------ message pump

    pub fn pump(&mut self) {
        while let Ok(m) = self.ag.side_rx.try_recv() {
            self.handle(m);
        }
        for (_, msg) in self.jobs.poll() {
            self.handle(msg);
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Opened(res) => match res {
                Ok(o) => {
                    self.pj_record_open(&o.repo.root);
                    self.repo = Some(o.repo);
                    self.head = o.head;
                    self.head_name = o.head_name;
                    self.refs = o.refs;
                    self.store = o.store;
                    self.store_note = o.store_note;
                    self.open_state = Loadable::Ready(());
                    self.request_log(false);
                    self.request_files();
                }
                Err(e) => self.open_state = Loadable::Failed(e),
            },
            Msg::Log { token, append, res } => {
                if token != self.commit_token {
                    return;
                }
                self.commits_loading = false;
                match res {
                    Ok(v) => {
                        self.commits_done = v.len() < PAGE;
                        if append {
                            self.commits.extend(v);
                        } else {
                            self.commits = v;
                        }
                        if self.target.is_none() && self.commit_filter_applied.0.is_empty() {
                            if let Some(first) = self.commits.first().map(|c| c.sha.clone()) {
                                self.select_commit(&first);
                            }
                        }
                    }
                    Err(e) => self.commits_err = Some(e),
                }
            }
            Msg::Diff { res } => match res {
                Ok((set, cur)) => {
                    let keep = self.file_sel.clone().filter(|p| set.files.iter().any(|f| &f.path == p));
                    let first = set.files.first().map(|f| f.path.clone());
                    self.cur = Some(cur);
                    self.diffset = Loadable::Ready(set);
                    match keep.or(first) {
                        Some(p) => self.open_file_diff(&p),
                        None => {
                            self.file_sel = None;
                            self.prepared = Loadable::Idle;
                        }
                    }
                }
                Err(e) => self.diffset = Loadable::Failed(e),
            },
            Msg::FileDiff { path, gen, res } => {
                if file_diff_is_stale(gen, self.cmp_gen, self.file_sel.as_deref(), &path) {
                    return;
                }
                self.prepared = match res {
                    Ok(p) => Loadable::Ready(p),
                    Err(e) => Loadable::Failed(e),
                };
                self.refresh_why_for_file();
            }
            Msg::Hits(hits) => {
                for h in hits {
                    if self.search.total >= MAX_SEARCH_HITS {
                        self.search.capped = true;
                        break;
                    }
                    self.search.total += 1;
                    match self.search.groups.last_mut() {
                        Some(g) if g.path == h.path => g.hits.push(h),
                        _ => self.search.groups.push(SearchGroup { path: h.path.clone(), hits: vec![h], collapsed: false }),
                    }
                }
            }
            Msg::SearchDone(res) => {
                self.search.running = false;
                match res {
                    Ok(st) => {
                        self.last_search_stats_line = format!("{} matches in {} files · {} ms{}", self.search.total, st.files_matched, st.elapsed_ms, if self.search.capped { " (capped)" } else { "" });
                        self.search.stats = Some(st);
                    }
                    Err(e) => self.search.error = Some(e),
                }
            }
            Msg::Files(res) => {
                self.files.all = match res {
                    Ok(v) => Loadable::Ready(v),
                    Err(e) => Loadable::Failed(e),
                };
                self.files.filtered_for = "\u{0}".into();
            }
            Msg::Blame { path, res } => {
                if self.blame.path != path {
                    return;
                }
                self.blame.data = match res {
                    Ok(b) => Loadable::Ready(b),
                    Err(e) => Loadable::Failed(e),
                };
            }
            Msg::History { path, line, res } => {
                if self.history.path == path && self.history.line == line {
                    self.history.data = match res {
                        Ok(v) => Loadable::Ready(v),
                        Err(e) => Loadable::Failed(e),
                    };
                }
            }
            Msg::FileLoaded { path, res } => {
                let Some(e) = &mut self.editor else { return };
                if e.path != path {
                    return;
                }
                match res {
                    Ok(l) => {
                        e.text = l.text.clone();
                        e.saved_text = l.text;
                        e.stamp = Some(l.stamp);
                        e.crlf = l.crlf;
                        e.read_only = l.read_only;
                        e.layout_cache = None;
                        e.load = Loadable::Ready(());
                    }
                    Err(er) => e.load = Loadable::Failed(er),
                }
            }
            Msg::Saved { path, written, res } => {
                let Some(e) = &mut self.editor else { return };
                if e.path != path {
                    return;
                }
                e.saving = false;
                match res {
                    Ok(SaveOutcome::Saved(st)) => {
                        e.stamp = Some(st);
                        e.saved_text = written;
                        e.conflict = None;
                        e.notice = Some((format!("Saved {path}"), Instant::now()));
                    }
                    Ok(SaveOutcome::Conflict(d)) => e.conflict = Some(d),
                    Err(er) => e.notice = Some((er, Instant::now())),
                }
            }
            Msg::Disk { path, res } => {
                let Some(e) = &mut self.editor else { return };
                if e.path != path {
                    return;
                }
                e.checking = false;
                match res {
                    Disk::Unchanged => {}
                    d => {
                        if e.dirty() {
                            e.conflict = Some(d);
                        } else if let Disk::Changed(_) = d {
                            e.notice = Some((format!("{path} changed on disk: reloaded."), Instant::now()));
                            self.reload_editor();
                        } else {
                            e.conflict = Some(d);
                        }
                    }
                }
            }
            Msg::Prompt(res) => match res {
                Ok(p) => {
                    if self.settings.ask_preview || p.sections.iter().any(|s| !s.warnings.is_empty()) {
                        self.ask.phase = AskPhase::Preview(Box::new(p));
                    } else {
                        self.send_ask(&p);
                    }
                }
                Err(e) => self.ask.phase = AskPhase::Failed(e),
            },
            Msg::AskChunk(s) => self.ask.output.push_str(&s),
            Msg::AskDone(res) => {
                self.ask.phase = match res {
                    Ok(()) => AskPhase::Done,
                    Err(e) => AskPhase::Failed(e),
                };
            }
            Msg::Browse { token, res } => self.pj_handle(token, res),
            other => self.ag_handle(other),
        }
    }

    pub fn current_job_ids(&self) -> Vec<JobId> {
        Vec::new()
    }
}

pub fn short_ref(r: &str) -> String {
    if r.len() >= 40 && r.bytes().all(|b| b.is_ascii_hexdigit()) {
        crate::timefmt::short(r).to_string()
    } else {
        r.to_string()
    }
}

pub fn large_diff(p: &Prepared) -> bool {
    p.model.lines.len() > LARGE_DIFF_LINES
}

/// Where records live: exactly the dir the hook/CLI use (`ct_agent::gitx::find_repo`), so linked
/// worktrees share one store. Falls back to ct-core's common dir outside `find_repo`'s reach.
fn store_dir(root: &std::path::Path, common_dir: &std::path::Path) -> PathBuf {
    ct_agent::gitx::find_repo(root).map(|r| r.git_dir).unwrap_or_else(|| common_dir.to_path_buf())
}

/// A per-file diff result applies only to the comparison generation and file it was requested for.
fn file_diff_is_stale(result_gen: u64, current_gen: u64, selected: Option<&str>, path: &str) -> bool {
    result_gen != current_gen || selected != Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let o = std::process::Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }

    #[test]
    fn store_dir_is_the_hooks_dir_for_linked_worktrees() {
        let d = tempfile::tempdir().unwrap();
        let main = d.path().join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "i"]);
        let wt = d.path().join("wt");
        git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        let hook_dir = ct_agent::gitx::find_repo(&wt).unwrap().git_dir;
        let repo = Repo::open(&wt).unwrap();
        assert_eq!(store_dir(&repo.root, &repo.common_dir), hook_dir);
        assert_eq!(hook_dir, ct_agent::gitx::find_repo(&main).unwrap().git_dir);
    }

    #[test]
    fn stale_file_diff_is_discarded_after_a_new_comparison() {
        assert!(!file_diff_is_stale(3, 3, Some("a.rs"), "a.rs"));
        assert!(file_diff_is_stale(2, 3, Some("a.rs"), "a.rs"), "older comparison");
        assert!(file_diff_is_stale(3, 3, Some("b.rs"), "a.rs"), "other file selected");
        assert!(file_diff_is_stale(3, 3, None, "a.rs"));
        let mut j: Jobs<u32> = Jobs::new(|| {});
        let old = j.spawn(JobKind::FileDiff, |_| {});
        j.cancel(JobKind::FileDiff);
        assert!(!j.is_current(old), "cancelled FileDiff job reads as stale");
    }

    fn mb(_: &str, _: &str) -> ct_core::Result<String> {
        Ok("mbmbmbmb".into())
    }
    fn cmp(base: BaseMode, target: TargetSel, range: Option<(&str, &str)>, prev: Option<&str>, n: u8) -> Comparison {
        let i = CmpInput { base: &base, range, parent_n: n, target: &target, prev, head: "HEADSHA" };
        build_comparison(&i, &mb).unwrap()
    }
    fn commit(s: &str) -> TargetSel {
        TargetSel::Commit(s.into())
    }

    #[test]
    fn base_modes_map_to_comparisons() {
        assert_eq!(cmp(BaseMode::Parent, commit("c1"), None, None, 1), Comparison::parent_n_of("c1", 1).unwrap());
        assert_eq!(cmp(BaseMode::Parent, commit("c1"), None, None, 2), Comparison::parent_n_of("c1", 2).unwrap());
        assert_eq!(cmp(BaseMode::Ref("main".into()), commit("c1"), None, None, 1), Comparison::range("main", "c1").unwrap());
        assert_eq!(cmp(BaseMode::MergeBase("main".into()), commit("c1"), None, None, 1), Comparison::range("mbmbmbmb", "c1").unwrap());
        assert_eq!(cmp(BaseMode::WorkingTree, commit("c1"), None, None, 1), Comparison::commit_vs_worktree("c1").unwrap());
    }

    #[test]
    fn previous_selection_falls_back_to_parent_when_missing_or_same() {
        assert_eq!(cmp(BaseMode::Previous, commit("c2"), None, Some("c1"), 1), Comparison::range("c1", "c2").unwrap());
        assert_eq!(cmp(BaseMode::Previous, commit("c2"), None, None, 1), Comparison::parent_n_of("c2", 1).unwrap());
        assert_eq!(cmp(BaseMode::Previous, commit("c2"), None, Some("c2"), 1), Comparison::parent_n_of("c2", 1).unwrap());
    }

    #[test]
    fn range_overrides_everything_and_worktree_target_diffs_head() {
        assert_eq!(cmp(BaseMode::Parent, commit("c9"), Some(("a", "b")), None, 1), Comparison::range("a", "b").unwrap());
        assert_eq!(cmp(BaseMode::Ref("x".into()), TargetSel::Worktree, None, None, 1), Comparison::commit_vs_worktree("HEADSHA").unwrap());
    }

    #[test]
    fn merge_base_error_propagates() {
        let i = CmpInput { base: &BaseMode::MergeBase("zz".into()), range: None, parent_n: 1, target: &commit("c1"), prev: None, head: "H" };
        let e = build_comparison(&i, &|_, _| Err(ct_core::Error::Invalid("no merge base".into())));
        assert!(e.is_err());
    }

    #[test]
    fn base_labels() {
        assert_eq!(BaseMode::Parent.label(1, false), "parent");
        assert_eq!(BaseMode::Parent.label(2, true), "parent 2");
        assert_eq!(BaseMode::MergeBase("main".into()).label(1, false), "merge-base(main)");
        assert_eq!(BaseMode::WorkingTree.label(1, false), "working tree");
    }

    #[test]
    fn short_ref_only_shortens_full_shas() {
        assert_eq!(short_ref("main"), "main");
        assert_eq!(short_ref(&"a".repeat(40)), "aaaaaaa");
    }
}
