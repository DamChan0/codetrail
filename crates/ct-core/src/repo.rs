use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use crate::diffparse::{parse_patch, parse_raw_numstat, to_stats, RawEntry};
use crate::git::{self, is_hex40, RunOpts};
use crate::*;

const LOG_FORMAT: &str = "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%D%x1f%s";

/// Resolved side of a comparison.
enum Side {
    Tree(Oid),
    Index,
    Worktree,
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn check_rev(rev: &str) -> Result<()> {
    if rev.trim().is_empty() || rev.starts_with('-') || rev.contains('\0') || rev.contains('\n') {
        return Err(Error::Invalid(format!("invalid revision {rev:?}")));
    }
    Ok(())
}

impl Repo {
    // ───────────────────────────── open / config ─────────────────────────────

    /// Opens the repository containing `path` (a directory inside the working tree, or a file).
    /// Works for plain repos, shallow clones, linked worktrees and submodule checkouts.
    /// Bare repositories are rejected.
    pub fn open(path: &Path) -> Result<Repo> {
        let dir = if path.is_file() { path.parent().unwrap_or(path) } else { path };
        let opts = RunOpts::default();
        let out = git::run(
            dir,
            &["rev-parse", "--path-format=absolute", "--show-toplevel", "--git-dir", "--git-common-dir"],
            None,
            &opts,
        )
        .map_err(|e| match e {
            Error::Git { stderr, .. } if stderr.contains("not a git repository") => {
                Error::NotARepo(path.display().to_string())
            }
            Error::Git { stderr, .. } if stderr.contains("work tree") => {
                Error::Invalid(format!("{}: bare repositories are not supported", path.display()))
            }
            Error::Spawn(s) if !dir.exists() => Error::NotARepo(format!("{} ({s})", path.display())),
            other => other,
        })?;
        let text = lossy(&out);
        let mut l = text.lines();
        let (root, git_dir, common) = match (l.next(), l.next(), l.next()) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return Err(Error::NotARepo(path.display().to_string())),
        };
        Ok(Repo {
            root: PathBuf::from(root),
            git_dir: PathBuf::from(git_dir),
            common_dir: PathBuf::from(common),
            opts,
        })
    }

    /// Timeout applied to every git subprocess of this handle (default 30 s).
    pub fn with_timeout(mut self, t: Duration) -> Repo {
        self.opts.timeout = t;
        self
    }

    /// Cap on buffered git stdout (and on a single streamed line); exceeding it → `Error::TooLarge`.
    pub fn with_max_output(mut self, bytes: usize) -> Repo {
        self.opts.max_output = bytes;
        self
    }

    /// Setting the flag kills in-flight git subprocesses of this handle (→ `Error::Cancelled`).
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Repo {
        self.opts.cancel = Some(cancel);
        self
    }

    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        git::run(&self.root, args, None, &self.opts)
    }

    fn run_s(&self, args: &[String]) -> Result<Vec<u8>> {
        let v: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&v)
    }

    // ─────────────────────────────── basic queries ───────────────────────────

    pub fn head(&self) -> Result<Oid> {
        self.resolve("HEAD").map_err(|e| match e {
            Error::Git { .. } => Error::Invalid("repository has no commits (HEAD is unborn)".into()),
            o => o,
        })
    }

    /// Resolves any commit-ish to its full commit sha.
    pub fn resolve(&self, rev: &str) -> Result<Oid> {
        check_rev(rev)?;
        let spec = format!("{rev}^{{commit}}");
        let out = self.run(&["rev-parse", "--verify", "--quiet", "--end-of-options", &spec]).map_err(|e| match e {
            Error::Git { stderr, .. } if stderr.is_empty() => Error::Invalid(format!("unknown revision {rev:?}")),
            o => o,
        })?;
        Ok(lossy(&out).trim().to_string())
    }

    pub fn merge_base(&self, a: &str, b: &str) -> Result<Oid> {
        check_rev(a)?;
        check_rev(b)?;
        let out = self.run(&["merge-base", "--end-of-options", a, b]).map_err(|e| match e {
            Error::Git { stderr, .. } if stderr.is_empty() => {
                Error::Invalid(format!("no merge base between {a:?} and {b:?}"))
            }
            o => o,
        })?;
        Ok(lossy(&out).trim().to_string())
    }

    pub fn refs(&self) -> Result<Vec<RefInfo>> {
        let out = self.run(&[
            "for-each-ref",
            "--format=%(refname)%1f%(refname:short)%1f%(objectname)%1f%(*objectname)%1f%(HEAD)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ])?;
        let mut v = Vec::new();
        for line in lossy(&out).lines() {
            let f: Vec<&str> = line.split('\x1f').collect();
            if f.len() < 5 {
                continue;
            }
            let (full, short, obj, peeled, head) = (f[0], f[1], f[2], f[3], f[4]);
            let kind = if full.starts_with("refs/heads/") {
                RefKind::Branch
            } else if full.starts_with("refs/remotes/") {
                if full.ends_with("/HEAD") {
                    continue;
                }
                RefKind::Remote
            } else {
                RefKind::Tag
            };
            v.push(RefInfo {
                name: short.to_string(),
                full_name: full.to_string(),
                kind,
                oid: if peeled.is_empty() { obj.to_string() } else { peeled.to_string() },
                is_head: head == "*",
            });
        }
        Ok(v)
    }

    /// Tracked files plus untracked non-ignored files that exist on disk.
    pub fn list_files(&self) -> Result<Vec<String>> {
        let out = self.run(&["ls-files", "-z", "--cached", "--others", "--exclude-standard", "--deduplicate"])?;
        let del = self.run(&["ls-files", "-z", "--deleted"])?;
        let deleted: std::collections::HashSet<&[u8]> =
            del.split(|&b| b == 0).filter(|s| !s.is_empty()).collect();
        Ok(out
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty() && !deleted.contains(s))
            .map(lossy)
            .collect())
    }

    // ───────────────────────────────── log ───────────────────────────────────

    pub fn log(&self, q: &LogQuery) -> Result<Vec<CommitMeta>> {
        if q.limit == 0 {
            return Ok(Vec::new());
        }
        let mut args: Vec<String> =
            vec!["--literal-pathspecs".into(), "log".into(), LOG_FORMAT.into(), "--no-show-signature".into()];
        if q.skip > 0 {
            args.push(format!("--skip={}", q.skip));
        }
        args.push(format!("--max-count={}", q.limit));
        if q.first_parent {
            args.push("--first-parent".into());
        }
        if q.author.is_some() || q.message.is_some() {
            args.push("--fixed-strings".into());
            args.push("--regexp-ignore-case".into());
        }
        if let Some(a) = &q.author {
            args.push(format!("--author={a}"));
        }
        if let Some(m) = &q.message {
            args.push(format!("--grep={m}"));
        }
        if q.all_refs {
            args.push("--all".into());
            args.push("--end-of-options".into());
        } else {
            let rev = q.rev.as_deref().unwrap_or("HEAD");
            check_rev(rev)?;
            args.push("--end-of-options".into());
            args.push(rev.to_string());
        }
        if let Some(p) = &q.path {
            args.push("--".into());
            args.push(p.clone());
        }
        let v: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut out = Vec::with_capacity(q.limit.min(1024));
        let res = git::run_lines(&self.root, &v, &self.opts, |line| {
            if let Some(c) = parse_commit_line(line) {
                out.push(c);
            }
            out.len() < q.limit
        });
        match res {
            Ok(()) => Ok(out),
            Err(Error::Git { .. }) if q.rev.is_none() && !q.all_refs && self.run(&["rev-parse", "--verify", "-q", "HEAD"]).is_err() => {
                Ok(Vec::new()) // empty repository
            }
            Err(Error::Git { .. }) if q.all_refs && self.run(&["rev-parse", "--verify", "-q", "HEAD"]).is_err() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// History of a file (following renames) or of a line range (`git log -L`).
    pub fn file_history(&self, path: &str, line_range: Option<(u32, u32)>, limit: usize) -> Result<Vec<CommitMeta>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut args: Vec<String> =
            vec!["--literal-pathspecs".into(), "log".into(), LOG_FORMAT.into(), format!("--max-count={limit}")];
        match line_range {
            Some((s, e)) => {
                if s == 0 || e < s {
                    return Err(Error::Invalid(format!("invalid line range {s}-{e}")));
                }
                args.push("-s".into());
                args.push(format!("-L{s},{e}:{path}"));
            }
            None => {
                args.push("--follow".into());
                args.push("--end-of-options".into());
                args.push("HEAD".into());
                args.push("--".into());
                args.push(path.to_string());
            }
        }
        let v: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut out = Vec::new();
        git::run_lines(&self.root, &v, &self.opts, |line| {
            if let Some(c) = parse_commit_line(line) {
                out.push(c);
            }
            out.len() < limit
        })?;
        Ok(out)
    }

    /// Like `file_history(path, Some(range), limit)` but also reports, per commit, the path and
    /// the line range the tracked lines occupy in that commit (taken from the `+c,d` side of the
    /// `git log -L` hunk headers; several hunks are merged into their union).
    pub fn line_history(&self, path: &str, range: (u32, u32), limit: usize) -> Result<Vec<LineRev>> {
        let (s, e) = range;
        if limit == 0 {
            return Ok(Vec::new());
        }
        if s == 0 || e < s {
            return Err(Error::Invalid(format!("invalid line range {s}-{e}")));
        }
        let args: Vec<String> = vec![
            "--literal-pathspecs".into(),
            "log".into(),
            LOG_FORMAT.into(),
            "--no-color".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            format!("--max-count={limit}"),
            format!("-L{s},{e}:{path}"),
        ];
        let v: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut out: Vec<LineRev> = Vec::new();
        git::run_lines(&self.root, &v, &self.opts, |line| {
            if let Some(c) = parse_commit_line(line) {
                if out.len() >= limit {
                    return false;
                }
                out.push(LineRev { commit: c, path: path.to_string(), lines: None });
            } else if let Some(cur) = out.last_mut() {
                let l = String::from_utf8_lossy(line);
                if let Some(p) = l.strip_prefix("+++ b/") {
                    cur.path = p.trim_end_matches(['\t', '\r']).to_string();
                } else if l.starts_with("@@ ") {
                    if let Some((a, b)) = parse_new_range(&l) {
                        cur.lines = Some(match cur.lines {
                            Some((x, y)) => (x.min(a), y.max(b)),
                            None => (a, b),
                        });
                    }
                }
            }
            true
        })?;
        Ok(out)
    }

    // ───────────────────────────────── diff ──────────────────────────────────

    /// Parents of a commit, in order.
    fn parents_of(&self, commit: &str) -> Result<Vec<Oid>> {
        check_rev(commit)?;
        let spec = if is_hex40(commit) { commit.to_string() } else { self.resolve(commit)? };
        let out = self.run(&["rev-list", "--parents", "-n1", &spec])?;
        let text = lossy(&out);
        let line = text.lines().next().unwrap_or("");
        Ok(line.split(' ').skip(1).map(str::to_string).collect())
    }

    fn side(&self, t: &Treeish) -> Result<Side> {
        Ok(match t {
            Treeish::Index => Side::Index,
            Treeish::Worktree => Side::Worktree,
            Treeish::Commit(c) => {
                check_rev(c)?;
                Side::Tree(if is_hex40(c) { c.clone() } else { self.resolve(c)? })
            }
            Treeish::Parent { commit, n } => {
                if *n == 0 {
                    return Err(Error::Invalid("Parent.n is 1-based; got 0".into()));
                }
                let parents = self.parents_of(commit)?;
                match parents.get(*n as usize - 1) {
                    Some(p) => Side::Tree(p.clone()),
                    None if parents.is_empty() && *n == 1 => Side::Tree(EMPTY_TREE.to_string()),
                    None => {
                        return Err(Error::Invalid(format!(
                            "commit {commit} has {} parent(s), no parent #{n}",
                            parents.len()
                        )))
                    }
                }
            }
        })
    }

    /// `git diff` argument vectors for a comparison: (flags before the output-format flags,
    /// revisions + `--`). Pathspecs are appended after the returned tail.
    fn diff_cmd(&self, c: &Comparison, o: &DiffOpts) -> Result<(Vec<String>, Vec<String>)> {
        Comparison::new(c.old.clone(), c.new.clone())?;
        let old = self.side(&c.old)?;
        let new = self.side(&c.new)?;
        let mut head: Vec<String> = vec![
            "--literal-pathspecs".into(),
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--no-color".into(),
        ];
        if o.detect_renames {
            head.push(if o.detect_copies { "-C".into() } else { "-M".into() });
        } else {
            head.push("--no-renames".into());
        }
        if o.ignore_whitespace {
            head.push("-w".into());
        }
        // git expresses "tree vs index/worktree" and "index vs worktree" directly; the mirrored
        // forms are produced with -R.
        let (cached, reverse, revs): (bool, bool, Vec<Oid>) = match (old, new) {
            (Side::Tree(a), Side::Tree(b)) => (false, false, vec![a, b]),
            (Side::Tree(a), Side::Index) => (true, false, vec![a]),
            (Side::Tree(a), Side::Worktree) => (false, false, vec![a]),
            (Side::Index, Side::Worktree) => (false, false, vec![]),
            (Side::Index, Side::Tree(b)) => (true, true, vec![b]),
            (Side::Worktree, Side::Tree(b)) => (false, true, vec![b]),
            (Side::Worktree, Side::Index) => (false, true, vec![]),
            (Side::Index, Side::Index) | (Side::Worktree, Side::Worktree) => {
                return Err(Error::Invalid("meaningless comparison".into()))
            }
        };
        if cached {
            head.push("--cached".into());
        }
        if reverse {
            head.push("-R".into());
        }
        let mut tail = revs;
        tail.push("--".into());
        Ok((head, tail))
    }

    /// File list (status, rename info, +/- counts, binary) for a comparison.
    pub fn diff(&self, c: &Comparison, o: &DiffOpts) -> Result<DiffSet> {
        let (mut args, tail) = self.diff_cmd(c, o)?;
        args.extend(["--raw", "--numstat", "-z", "--no-abbrev"].map(String::from));
        args.extend(tail);
        let out = self.run_s(&args)?;
        let (raws, nums) = parse_raw_numstat(&out)?;
        let mut files = to_stats(raws, nums, o.ignore_whitespace)?;
        if o.ignore_whitespace {
            // --raw compares blobs, so whitespace-only edits would still be listed.
            files.retain(|f| {
                !(f.status == Status::Modified && f.add == 0 && f.del == 0 && !f.binary && f.old_mode == f.new_mode)
            });
        }
        Ok(DiffSet { files })
    }

    /// Hunks of one file of a comparison. `path` is the new-side path (old-side for deletions).
    pub fn file_diff(&self, c: &Comparison, path: &str, o: &DiffOpts) -> Result<DiffFile> {
        // Pass 1 (cheap, no content): locate the entry so renames/copies get both pathspecs.
        let (mut args, tail) = self.diff_cmd(c, o)?;
        args.extend(["--raw", "-z", "--no-abbrev"].map(String::from));
        args.extend(tail);
        let out = self.run_s(&args)?;
        let (raws, _) = parse_raw_numstat(&out)?;
        let e: RawEntry = raws
            .into_iter()
            .find(|e| e.path == path)
            .ok_or_else(|| Error::Invalid(format!("{path:?} is not part of this comparison")))?;
        // Pass 2: the patch for just that file (+ its rename source).
        let (mut args, tail) = self.diff_cmd(c, o)?;
        args.push(format!("-U{}", o.context));
        args.extend(tail);
        args.push(e.path.clone());
        if let Some(op) = &e.old_path {
            args.push(op.clone());
        }
        let out = self.run_s(&args)?;
        let parsed = parse_patch(&out);
        let kind = crate::diffparse::kind_of(&e, parsed.binary);
        Ok(DiffFile {
            path: e.path,
            old_path: e.old_path,
            old_oid: e.old_oid,
            new_oid: e.new_oid,
            old_mode: e.old_mode,
            new_mode: e.new_mode,
            kind,
            status: e.status,
            hunks: parsed.hunks,
        })
    }

    // ──────────────────────────────── content ────────────────────────────────

    /// Blob content of `path` at `rev` (any commit-ish). `rev == ":"` reads from the index.
    pub fn show_file(&self, rev: &str, path: &str) -> Result<Vec<u8>> {
        if rev != ":" {
            check_rev(rev)?;
        }
        let spec = if rev == ":" { format!(":{path}") } else { format!("{rev}:{path}") };
        self.run(&["cat-file", "blob", &spec])
    }

    /// Reads a working-tree file (rejects absolute paths and `..`).
    pub fn read_worktree_file(&self, path: &str) -> Result<Vec<u8>> {
        let p = Path::new(path);
        if p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir | Component::Prefix(_))) {
            return Err(Error::Invalid(format!("path {path:?} escapes the repository")));
        }
        Ok(std::fs::read(self.root.join(p))?)
    }

    /// `git blame --porcelain`. `at`: Commit/Parent → that revision, Worktree → file on disk
    /// (uncommitted lines get the all-zero sha), Index → staged content.
    pub fn blame(&self, at: &Treeish, path: &str, range: Option<(u32, u32)>) -> Result<Vec<BlameLine>> {
        let mut args: Vec<String> = vec!["--literal-pathspecs".into(), "blame".into(), "--porcelain".into()];
        if let Some((s, e)) = range {
            if s == 0 || e < s {
                return Err(Error::Invalid(format!("invalid line range {s}-{e}")));
            }
            args.push("-L".into());
            args.push(format!("{s},{e}"));
        }
        let mut stdin = None;
        match self.side(at)? {
            Side::Tree(oid) => args.push(oid), // always 40-hex (see `side`), cannot be an option
            Side::Worktree => {}
            Side::Index => {
                stdin = Some(self.show_file(":", path)?);
                args.push("--contents".into());
                args.push("-".into());
            }
        }
        args.push("--".into());
        args.push(path.to_string());
        let v: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = git::run(&self.root, &v, stdin, &self.opts)?;
        Ok(parse_blame(&out))
    }
}

#[derive(Clone, Default)]
struct BlameMeta {
    author: String,
    time: i64,
    summary: String,
    filename: String,
}

fn parse_blame(buf: &[u8]) -> Vec<BlameLine> {
    let mut metas: HashMap<String, BlameMeta> = HashMap::new();
    let mut out = Vec::new();
    let mut cur: Option<(String, u32, u32)> = None; // sha, orig_line, final_line
    let mut lines = buf.split(|&b| b == b'\n').peekable();
    while let Some(l) = lines.next() {
        if lines.peek().is_none() && l.is_empty() {
            break;
        }
        if let Some(text) = l.strip_prefix(b"\t") {
            if let Some((sha, orig, fin)) = cur.take() {
                let m = metas.get(&sha).cloned().unwrap_or_default();
                out.push(BlameLine {
                    line_no: fin,
                    sha,
                    author: m.author,
                    time: m.time,
                    summary: m.summary,
                    orig_line: orig,
                    orig_path: m.filename,
                    text: lossy(text),
                });
            }
            continue;
        }
        let s = lossy(l);
        let mut parts = s.split(' ');
        let first = parts.next().unwrap_or("");
        if first.len() == 40 && first.bytes().all(|c| c.is_ascii_hexdigit()) {
            let orig = parts.next().and_then(|x| x.parse().ok());
            let fin = parts.next().and_then(|x| x.parse().ok());
            if let (Some(o), Some(f)) = (orig, fin) {
                metas.entry(first.to_string()).or_default();
                cur = Some((first.to_string(), o, f));
                continue;
            }
        }
        if let Some((sha, ..)) = &cur {
            let m = metas.get_mut(sha).expect("inserted at header");
            if let Some(v) = s.strip_prefix("author ") {
                m.author = v.to_string();
            } else if let Some(v) = s.strip_prefix("author-time ") {
                m.time = v.parse().unwrap_or(0);
            } else if let Some(v) = s.strip_prefix("summary ") {
                m.summary = v.to_string();
            } else if let Some(v) = s.strip_prefix("filename ") {
                m.filename = unquote_c(v);
            }
        }
    }
    out
}

/// `@@ -a,b +c,d @@` -> inclusive new range (`c..c+d-1`); a zero-length range yields `None`.
fn parse_new_range(h: &str) -> Option<(u32, u32)> {
    let plus = h.split_whitespace().find(|t| t.starts_with('+'))?;
    let mut it = plus[1..].splitn(2, ',');
    let start: u32 = it.next()?.parse().ok()?;
    let len: u32 = it.next().map_or(Some(1), |n| n.parse().ok())?;
    (len > 0 && start > 0).then(|| (start, start + len - 1))
}

fn parse_commit_line(line: &[u8]) -> Option<CommitMeta> {
    let s = String::from_utf8_lossy(line);
    let mut f = s.splitn(7, '\x1f');
    let sha = f.next()?;
    if sha.len() < 40 || !sha.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let parents = f.next()?;
    let author = f.next()?;
    let email = f.next()?;
    let time = f.next()?.parse::<i64>().ok()?;
    let deco = f.next()?;
    let subject = f.next().unwrap_or("");
    Some(CommitMeta {
        sha: sha.to_string(),
        parents: parents.split(' ').filter(|p| !p.is_empty()).map(str::to_string).collect(),
        author: author.to_string(),
        email: email.to_string(),
        time,
        subject: subject.to_string(),
        refs: deco.split(", ").filter(|d| !d.is_empty()).map(str::to_string).collect(),
    })
}

/// Decodes git's C-style quoting (`"a\tb"`, octal escapes) used for unusual paths in porcelain output.
fn unquote_c(s: &str) -> String {
    let inner = match s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        Some(i) => i,
        None => return s.to_string(),
    };
    let b = inner.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 >= b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        match b[i] {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'v' => out.push(11),
            b'0'..=b'7' => {
                let mut v = 0u32;
                let mut n = 0;
                while n < 3 && i < b.len() && (b'0'..=b'7').contains(&b[i]) {
                    v = v * 8 + (b[i] - b'0') as u32;
                    i += 1;
                    n += 1;
                }
                out.push(v as u8);
                continue;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
