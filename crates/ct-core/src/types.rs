//! Public data types of ct-core (PLAN §3.1 as amended by §9.1).

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use crate::{Error, Result};

/// 40-hex object id (full sha).
pub type Oid = String;

/// SHA-1 empty tree. Used as the "old side" for root commits.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

// ───────────────────────────── repo / refs / log ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefKind {
    Branch,
    Remote,
    Tag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefInfo {
    /// Short name (`main`, `origin/main`, `v1.0`).
    pub name: String,
    /// Full ref name (`refs/heads/main`).
    pub full_name: String,
    pub kind: RefKind,
    /// Commit the ref points to (annotated tags are peeled).
    pub oid: Oid,
    /// True for the currently checked-out branch.
    pub is_head: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMeta {
    pub sha: Oid,
    pub parents: Vec<Oid>,
    pub author: String,
    pub email: String,
    /// Author time, unix seconds.
    pub time: i64,
    pub subject: String,
    /// Decorations exactly as `git log %D` tokens: `HEAD -> main`, `tag: v1`, `origin/main`, `HEAD`.
    pub refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogQuery {
    /// Start revision (default `HEAD`). Ignored when `all_refs`.
    pub rev: Option<String>,
    pub all_refs: bool,
    pub skip: usize,
    pub limit: usize,
    /// Only commits touching this path (repo-relative).
    pub path: Option<String>,
    /// Literal, case-insensitive author substring (name or email).
    pub author: Option<String>,
    /// Literal, case-insensitive commit message substring.
    pub message: Option<String>,
    pub first_parent: bool,
}

impl Default for LogQuery {
    fn default() -> Self {
        LogQuery {
            rev: None,
            all_refs: false,
            skip: 0,
            limit: 200,
            path: None,
            author: None,
            message: None,
            first_parent: false,
        }
    }
}

// ─────────────────────────────── comparison ────────────────────────────────

/// One side of a comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Treeish {
    /// A commit (full or abbreviated sha, or any rev git understands; 40-hex is used verbatim).
    Commit(Oid),
    /// n-th parent (1-based) of `commit`. Root commit + n=1 means the empty tree.
    Parent { commit: Oid, n: u8 },
    /// The index (staging area).
    Index,
    /// The working tree (tracked files; untracked files are not part of diffs).
    Worktree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub old: Treeish,
    pub new: Treeish,
}

impl Comparison {
    /// Validated constructor. `Index/Index` and `Worktree/Worktree` are meaningless → Err.
    pub fn new(old: Treeish, new: Treeish) -> Result<Comparison> {
        match (&old, &new) {
            (Treeish::Worktree, Treeish::Worktree) => {
                return Err(Error::Invalid("cannot compare Worktree with Worktree".into()))
            }
            (Treeish::Index, Treeish::Index) => {
                return Err(Error::Invalid("cannot compare Index with Index".into()))
            }
            _ => {}
        }
        for t in [&old, &new] {
            if let Treeish::Parent { n, .. } = t {
                if *n == 0 {
                    return Err(Error::Invalid("Parent.n is 1-based; got 0".into()));
                }
            }
            if let Treeish::Commit(c) | Treeish::Parent { commit: c, .. } = t {
                if c.trim().is_empty() || c.starts_with('-') {
                    return Err(Error::Invalid(format!("invalid revision {c:?}")));
                }
            }
        }
        Ok(Comparison { old, new })
    }

    /// "What did this commit change?" — first parent (root commit → empty tree) vs commit.
    pub fn parent_of(commit: &str) -> Result<Comparison> {
        Self::parent_n_of(commit, 1)
    }

    /// Like [`parent_of`](Self::parent_of) for the n-th parent (merge commits).
    pub fn parent_n_of(commit: &str, n: u8) -> Result<Comparison> {
        Comparison::new(
            Treeish::Parent { commit: commit.to_string(), n },
            Treeish::Commit(commit.to_string()),
        )
    }

    /// `a..b` style two-point comparison (a is the old side).
    pub fn range(a: &str, b: &str) -> Result<Comparison> {
        Comparison::new(Treeish::Commit(a.to_string()), Treeish::Commit(b.to_string()))
    }

    /// `merge-base(a, b)` (old) vs `b` (new) — what `b` introduced relative to `a`.
    pub fn merge_base(a: &str, b: &str, repo: &crate::Repo) -> Result<Comparison> {
        let base = repo.merge_base(a, b)?;
        Comparison::new(Treeish::Commit(base), Treeish::Commit(b.to_string()))
    }

    /// commit (old) vs working tree (new).
    pub fn commit_vs_worktree(commit: &str) -> Result<Comparison> {
        Comparison::new(Treeish::Commit(commit.to_string()), Treeish::Worktree)
    }

    /// index (old) vs working tree (new) — unstaged changes.
    pub fn index_vs_worktree() -> Comparison {
        Comparison { old: Treeish::Index, new: Treeish::Worktree }
    }

    /// commit (old) vs index (new) — staged changes when commit = HEAD.
    pub fn commit_vs_index(commit: &str) -> Result<Comparison> {
        Comparison::new(Treeish::Commit(commit.to_string()), Treeish::Index)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffOpts {
    pub context: u32,
    pub ignore_whitespace: bool,
    pub detect_renames: bool,
    pub detect_copies: bool,
}

impl Default for DiffOpts {
    fn default() -> Self {
        DiffOpts { context: 3, ignore_whitespace: false, detect_renames: true, detect_copies: false }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}

impl Status {
    /// git's one-letter code.
    pub fn letter(self) -> char {
        match self {
            Status::Added => 'A',
            Status::Modified => 'M',
            Status::Deleted => 'D',
            Status::Renamed => 'R',
            Status::Copied => 'C',
            Status::TypeChanged => 'T',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Text,
    Binary,
    Symlink,
    Submodule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub path: String,
    pub old_path: Option<String>,
    pub status: Status,
    /// Rename/copy similarity percentage (R/C only).
    pub similarity: Option<u8>,
    pub add: u32,
    pub del: u32,
    pub binary: bool,
    pub kind: FileKind,
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffSet {
    pub files: Vec<FileStat>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Ctx,
    Add,
    Del,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    /// Line text without the trailing `\n` (a `\r` of CRLF files is preserved).
    pub text: String,
    /// True when git printed `\ No newline at end of file` for this line.
    pub no_newline_at_eof: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// Text after the second `@@` (git's function context), possibly empty.
    pub section: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    pub path: String,
    pub old_path: Option<String>,
    pub old_oid: Option<Oid>,
    pub new_oid: Option<Oid>,
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
    pub kind: FileKind,
    pub status: Status,
    /// Empty for binary files.
    pub hunks: Vec<Hunk>,
}

// ────────────────────────────────── blame ────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    /// 1-based line number in the blamed revision.
    pub line_no: u32,
    /// Commit sha; 40 zeros for uncommitted lines.
    pub sha: Oid,
    pub author: String,
    pub time: i64,
    pub summary: String,
    /// Line number in the originating commit.
    pub orig_line: u32,
    /// Path in the originating commit (differs from the file path after renames).
    pub orig_path: String,
    pub text: String,
}

// ───────────────────────────────── search ────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub pattern: String,
    pub regex: bool,
    pub case_sensitive: bool,
    /// ripgrep-style globs relative to the search root; `!glob` excludes.
    pub globs: Vec<String>,
}

impl Default for SearchQuery {
    fn default() -> Self {
        SearchQuery { pattern: String::new(), regex: false, case_sensitive: true, globs: Vec::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Root-relative path with `/` separators.
    pub path: String,
    /// 1-based line number.
    pub line: u32,
    /// 1-based **byte** column of the first match in `text`.
    pub col: u32,
    /// The matching line without its line terminator (lossy UTF-8).
    pub text: String,
    /// Half-open **byte** ranges of every match inside `text`.
    pub ranges: Vec<(u32, u32)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchStats {
    pub files_searched: u64,
    pub files_matched: u64,
    pub matches: u64,
    pub bytes_searched: u64,
    pub elapsed_ms: u64,
    pub first_hit_ms: Option<u64>,
    pub cancelled: bool,
    /// Aborted because the result channel stayed full (consumer stalled) or was dropped.
    pub aborted: bool,
}

// ───────────────────────────────── code ref ──────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefAt {
    /// Hex commit id, 4–40 chars, lower-cased.
    Commit(String),
    Worktree,
}

/// `path:10-24@abc1234`, `path:10@worktree`, `path:10` (= worktree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeRef {
    pub path: String,
    /// 1-based inclusive.
    pub start: u32,
    pub end: u32,
    pub at: RefAt,
}

impl CodeRef {
    pub fn parse(s: &str) -> Result<CodeRef> {
        let bad = |why: &str| Error::Invalid(format!("invalid code ref {s:?}: {why}"));
        let s = s.trim();
        if s.is_empty() {
            return Err(bad("empty"));
        }
        // `@suffix` is only taken as the revision part when it is a valid one; otherwise the
        // `@` belongs to the path (e.g. `a@b.rs:3`).
        let (head, at) = match s.rfind('@') {
            Some(i) => match parse_at(&s[i + 1..]) {
                Some(at) => (&s[..i], at),
                None if s[i + 1..].contains(':') || s[i + 1..].contains('/') => (s, RefAt::Worktree),
                None => return Err(bad("revision after '@' must be 4-40 hex chars or 'worktree'")),
            },
            None => (s, RefAt::Worktree),
        };
        let colon = head.rfind(':').ok_or_else(|| bad("missing ':<line>'"))?;
        let (path, lines) = (&head[..colon], &head[colon + 1..]);
        if path.is_empty() {
            return Err(bad("empty path"));
        }
        let lines = lines.strip_prefix('L').unwrap_or(lines);
        let (a, b) = match lines.split_once('-') {
            Some((a, b)) => (a, b.strip_prefix('L').unwrap_or(b)),
            None => (lines, lines),
        };
        let num = |t: &str| -> Result<u32> {
            if t.is_empty() || !t.bytes().all(|c| c.is_ascii_digit()) {
                return Err(bad("line numbers must be positive integers"));
            }
            t.parse::<u32>().map_err(|_| bad("line number out of range"))
        };
        let (start, end) = (num(a)?, num(b)?);
        if start == 0 || end == 0 {
            return Err(bad("lines are 1-based"));
        }
        if start > end {
            return Err(bad("start line is after end line"));
        }
        Ok(CodeRef { path: path.to_string(), start, end, at })
    }
}

fn parse_at(t: &str) -> Option<RefAt> {
    if t.eq_ignore_ascii_case("worktree") {
        return Some(RefAt::Worktree);
    }
    if (4..=40).contains(&t.len()) && t.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Some(RefAt::Commit(t.to_ascii_lowercase()));
    }
    None
}

impl fmt::Display for CodeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "{}:{}", self.path, self.start)?;
        } else {
            write!(f, "{}:{}-{}", self.path, self.start, self.end)?;
        }
        match &self.at {
            RefAt::Commit(c) => write!(f, "@{c}"),
            RefAt::Worktree => write!(f, "@worktree"),
        }
    }
}

impl FromStr for CodeRef {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        CodeRef::parse(s)
    }
}

// ───────────────────────────────── Repo ──────────────────────────────────────

/// Handle to a git repository (working tree). Cheap to clone.
#[derive(Debug, Clone)]
pub struct Repo {
    /// Working tree root.
    pub root: PathBuf,
    /// Per-worktree git dir (`.git`, or `.git/worktrees/<name>` for linked worktrees).
    pub git_dir: PathBuf,
    /// Shared git dir (same as `git_dir` except in linked worktrees).
    pub common_dir: PathBuf,
    pub(crate) opts: crate::git::RunOpts,
}
