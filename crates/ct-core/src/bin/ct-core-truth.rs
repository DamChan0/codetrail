//! Ground-truth comparison of ct-core against the git / rg CLIs on a real repository.
//!
//! usage: ct-core-truth <repo> [--commits N] [--patterns a,b,c]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

use ct_core::*;

fn sh(dir: &Path, prog: &str, args: &[&str]) -> Vec<u8> {
    let out = Command::new(prog)
        .current_dir(dir)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .unwrap_or_else(|e| panic!("{prog}: {e}"));
    out.stdout
}

fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
    let mut a = vec!["-c", "core.quotepath=false"];
    a.extend_from_slice(args);
    sh(dir, "git", &a)
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

struct Report {
    ok: usize,
    bad: usize,
}

impl Report {
    fn check(&mut self, name: &str, pass: bool, detail: impl FnOnce() -> String) {
        if pass {
            self.ok += 1;
            println!("OK        {name}");
        } else {
            self.bad += 1;
            println!("MISMATCH  {name}: {}", detail());
        }
    }
}

/// `git diff --name-status -z` → (path → letter, with R/C old path in the key's tuple)
fn truth_name_status(dir: &Path, args: &[&str]) -> BTreeMap<String, (char, Option<String>)> {
    let mut a = vec!["diff", "--name-status", "-z", "-M"];
    a.extend_from_slice(args);
    let out = git(dir, &a);
    let mut m = BTreeMap::new();
    let mut it = out.split(|&b| b == 0).filter(|s| !s.is_empty());
    while let Some(st) = it.next() {
        let st = lossy(st);
        let c = st.chars().next().unwrap();
        if c == 'R' || c == 'C' {
            let old = lossy(it.next().unwrap());
            let new = lossy(it.next().unwrap());
            m.insert(new, (c, Some(old)));
        } else {
            m.insert(lossy(it.next().unwrap()), (c, None));
        }
    }
    m
}

fn truth_numstat(dir: &Path, args: &[&str]) -> BTreeMap<String, (String, String)> {
    let mut a = vec!["diff", "--numstat", "-z", "-M"];
    a.extend_from_slice(args);
    let out = git(dir, &a);
    let mut m = BTreeMap::new();
    let mut it = out.split(|&b| b == 0).filter(|s| !s.is_empty());
    while let Some(t) = it.next() {
        let t = lossy(t);
        let mut p = t.splitn(3, '\t');
        let (a, d, rest) = (p.next().unwrap().to_string(), p.next().unwrap().to_string(), p.next().unwrap_or(""));
        if rest.is_empty() {
            it.next();
            let new = lossy(it.next().unwrap());
            m.insert(new, (a, d));
        } else {
            m.insert(rest.to_string(), (a, d));
        }
    }
    m
}

fn ours_name_status(r: &Repo, c: &Comparison) -> BTreeMap<String, (char, Option<String>)> {
    r.diff(c, &DiffOpts::default())
        .unwrap()
        .files
        .into_iter()
        .map(|f| (f.path, (f.status.letter(), f.old_path)))
        .collect()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let repo_path = PathBuf::from(args.next().expect("usage: ct-core-truth <repo> [--commits N] [--patterns a,b]"));
    let mut sample = 150usize;
    let mut patterns: Vec<String> = vec!["TODO".into(), "the".into(), "fn ".into(), "use".into(), "import".into(), "한글".into()];
    while let Some(a) = args.next() {
        match a.as_str() {
            "--commits" => sample = args.next().unwrap().parse().unwrap(),
            "--patterns" => patterns = args.next().unwrap().split(',').map(String::from).collect(),
            o => panic!("unknown option {o}"),
        }
    }
    let r = Repo::open(&repo_path).unwrap();
    let dir = r.root.clone();
    let mut rep = Report { ok: 0, bad: 0 };
    println!("repo: {}", dir.display());

    // ── log ──
    let t0 = std::time::Instant::now();
    let ours = r.log(&LogQuery { limit: 10_000_000, ..Default::default() }).unwrap();
    let ours_ms = t0.elapsed().as_millis();
    let truth_raw = git(&dir, &["log", "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%s"]);
    let truth: Vec<(String, String, String, String, i64, String)> = lossy(&truth_raw)
        .lines()
        .map(|l| {
            let f: Vec<&str> = l.splitn(6, '\x1f').collect();
            (f[0].into(), f[1].into(), f[2].into(), f[3].into(), f[4].parse().unwrap(), f.get(5).unwrap_or(&"").to_string())
        })
        .collect();
    let ours_t: Vec<_> = ours
        .iter()
        .map(|c| (c.sha.clone(), c.parents.join(" "), c.author.clone(), c.email.clone(), c.time, c.subject.clone()))
        .collect();
    rep.check(&format!("log: {} commits identical (sha,parents,author,email,time,subject) [{} ms]", truth.len(), ours_ms), ours_t == truth, || {
        format!("ours {} vs git {}", ours_t.len(), truth.len())
    });
    // pagination equals slices of the full log
    if truth.len() > 60 {
        let p = r.log(&LogQuery { skip: 25, limit: 30, ..Default::default() }).unwrap();
        rep.check("log: skip/limit page == slice of full log", p.iter().map(|c| &c.sha).eq(truth[25..55].iter().map(|t| &t.0)), || "page differs".into());
    }

    // ── file lists per commit (sampled) ──
    let n = ours.len();
    let step = (n / sample.max(1)).max(1);
    let mut picked: Vec<&CommitMeta> = ours.iter().step_by(step).collect();
    picked.extend(ours.iter().filter(|c| c.parents.len() != 1).take(10)); // root + merges
    let (mut files_bad, mut hunks_ok, mut hunks_bad) = (0, 0, 0);
    let mut first_err = String::new();
    for c in &picked {
        let cmp = Comparison::parent_of(&c.sha).unwrap();
        let old = c.parents.first().cloned().unwrap_or_else(|| EMPTY_TREE.to_string());
        let want = truth_name_status(&dir, &[&old, &c.sha]);
        let got = ours_name_status(&r, &cmp);
        if want != got {
            files_bad += 1;
            if first_err.is_empty() {
                first_err = format!("{}: ours {} files vs git {}", &c.sha[..8], got.len(), want.len());
            }
        }
        // +/- counts vs numstat, and hunk lines vs counts
        let ns = truth_numstat(&dir, &[&old, &c.sha]);
        let set = r.diff(&cmp, &DiffOpts::default()).unwrap();
        for f in set.files.iter().take(6) {
            let (wa, wd) = ns.get(&f.path).cloned().unwrap_or_default();
            let counts_ok = if f.binary { wa == "-" && wd == "-" } else { wa == f.add.to_string() && wd == f.del.to_string() };
            if f.binary || f.kind == FileKind::Submodule {
                if counts_ok { hunks_ok += 1 } else { hunks_bad += 1 }
                continue;
            }
            let fd = r.file_diff(&cmp, &f.path, &DiffOpts::default()).unwrap();
            let add = fd.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == LineKind::Add).count();
            let del = fd.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == LineKind::Del).count();
            if counts_ok && add == f.add as usize && del == f.del as usize {
                hunks_ok += 1;
            } else {
                hunks_bad += 1;
                if first_err.is_empty() {
                    first_err = format!("{} {}: hunk +{add}/-{del} vs numstat {wa}/{wd}", &c.sha[..8], f.path);
                }
            }
        }
    }
    rep.check(&format!("diff: file list (path,status,old_path) vs `git diff --name-status` on {} commits (incl. root/merge)", picked.len()), files_bad == 0, || format!("{files_bad} bad, e.g. {first_err}"));
    rep.check(&format!("file_diff: +/- line counts vs `git diff --numstat` on {} files", hunks_ok + hunks_bad), hunks_bad == 0, || format!("{hunks_bad} bad, e.g. {first_err}"));

    // patch text equality for a few commits: reconstruct unified diff body from hunks and compare with git
    let mut patch_ok = 0;
    let mut patch_bad = 0;
    for c in picked.iter().take(25) {
        let cmp = Comparison::parent_of(&c.sha).unwrap();
        let old = c.parents.first().cloned().unwrap_or_else(|| EMPTY_TREE.to_string());
        let set = r.diff(&cmp, &DiffOpts::default()).unwrap();
        for f in set.files.iter().filter(|f| f.kind == FileKind::Text && !f.binary).take(3) {
            let fd = r.file_diff(&cmp, &f.path, &DiffOpts::default()).unwrap();
            let mut got = String::new();
            for h in &fd.hunks {
                got.push_str(&format!("@@ -{},{} +{},{} @@\n", h.old_start, h.old_len, h.new_start, h.new_len));
                for l in &h.lines {
                    got.push(match l.kind { LineKind::Ctx => ' ', LineKind::Add => '+', LineKind::Del => '-' });
                    got.push_str(&l.text);
                    got.push('\n');
                    if l.no_newline_at_eof {
                        got.push_str("\\ No newline at end of file\n");
                    }
                }
            }
            let raw = git(&dir, &["diff", "-M", "--no-ext-diff", "--no-color", "-U3", &old, &c.sha, "--", &f.path, f.old_path.as_deref().unwrap_or(&f.path)]);
            let want: String = lossy(&raw)
                .lines()
                .skip_while(|l| !l.starts_with("@@ "))
                .map(|l| {
                    // normalise git's "@@ -a +b @@ section" (omitted ",1" lengths, trailing section)
                    if let Some(rest) = l.strip_prefix("@@ ") {
                        let end = rest.find(" @@").unwrap();
                        let (mut o, mut nw) = rest[..end].split_once(" +").map(|(a, b)| (a[1..].to_string(), b.to_string())).unwrap();
                        if !o.contains(',') { o.push_str(",1"); }
                        if !nw.contains(',') { nw.push_str(",1"); }
                        format!("@@ -{o} +{nw} @@\n")
                    } else {
                        format!("{l}\n")
                    }
                })
                .collect();
            if got == want { patch_ok += 1 } else { patch_bad += 1; if first_err.is_empty() { first_err = format!("{} {}", &c.sha[..8], f.path); } }
        }
    }
    rep.check(&format!("file_diff: reconstructed hunk text byte-identical to `git diff -U3` on {} files", patch_ok + patch_bad), patch_bad == 0, || format!("{patch_bad} differ, e.g. {first_err}"));

    // ── Comparison variants vs git diff --name-status ──
    if n >= 12 {
        let a = &ours[n.min(40) - 1].sha;
        let b = &ours[0].sha;
        let mb_ours = r.merge_base(a, b).unwrap();
        let mb_git = lossy(&git(&dir, &["merge-base", a, b])).trim().to_string();
        rep.check("merge_base == `git merge-base`", mb_ours == mb_git, || format!("{mb_ours} vs {mb_git}"));
        let range = ours_name_status(&r, &Comparison::range(a, b).unwrap());
        rep.check("range a..b file list == `git diff a b`", range == truth_name_status(&dir, &[a, b]), || "differs".into());
        let mbc = ours_name_status(&r, &Comparison::merge_base(a, b, &r).unwrap());
        rep.check("merge-base(a,b) vs b file list == `git diff <mb> b`", mbc == truth_name_status(&dir, &[&mb_git, b]), || "differs".into());
        let wt = ours_name_status(&r, &Comparison::commit_vs_worktree(b).unwrap());
        rep.check("commit vs worktree == `git diff <c>`", wt == truth_name_status(&dir, &[b]), || "differs".into());
        let iw = ours_name_status(&r, &Comparison::index_vs_worktree());
        rep.check("index vs worktree == `git diff`", iw == truth_name_status(&dir, &[]), || "differs".into());
        let hi = ours_name_status(&r, &Comparison::commit_vs_index("HEAD").unwrap());
        rep.check("HEAD vs index == `git diff --cached`", hi == truth_name_status(&dir, &["--cached"]), || "differs".into());
    }

    // ── list_files ──
    let mut lf: Vec<String> = r.list_files().unwrap();
    lf.sort();
    let want_raw = git(&dir, &["ls-files", "-z", "--cached", "--others", "--exclude-standard"]);
    let mut want: Vec<String> = want_raw.split(|&b| b == 0).filter(|s| !s.is_empty()).map(lossy).collect();
    let del: BTreeSet<String> = git(&dir, &["ls-files", "-z", "--deleted"]).split(|&b| b == 0).filter(|s| !s.is_empty()).map(lossy).collect();
    want.retain(|p| !del.contains(p));
    want.sort();
    want.dedup();
    rep.check(&format!("list_files: {} paths == ls-files (cached+others, minus deleted)", want.len()), lf == want, || format!("{} vs {}", lf.len(), want.len()));

    // ── blame vs --line-porcelain ──
    let mut blame_ok = 0;
    let mut blame_bad = 0;
    let mut cand: Vec<&String> = lf.iter().filter(|p| !p.ends_with(".lock") && !p.ends_with(".png") && !p.ends_with(".wav")).collect();
    cand.sort_by_key(|p| std::fs::metadata(dir.join(p)).map(|m| std::cmp::Reverse(m.len())).unwrap_or(std::cmp::Reverse(0)));
    let head = r.head().unwrap();
    for p in cand.iter().take(6) {
        let tracked = !git(&dir, &["ls-tree", "--name-only", "-r", &head, "--", p]).is_empty();
        if !tracked {
            continue;
        }
        let want_raw = git(&dir, &["blame", "--line-porcelain", &head, "--", p]);
        let mut want: Vec<(u32, String, u32, String, i64, String, String)> = Vec::new();
        let mut cur: Option<(String, u32, u32)> = None;
        let (mut au, mut at, mut su, mut fnm) = (String::new(), 0i64, String::new(), String::new());
        for l in lossy(&want_raw).split('\n') {
            if let Some(text) = l.strip_prefix('\t') {
                if let Some((sha, o, f)) = cur.take() {
                    want.push((f, sha, o, au.clone(), at, su.clone(), format!("{fnm}\u{0}{text}")));
                }
            } else if let Some(v) = l.strip_prefix("author ") {
                au = v.into();
            } else if let Some(v) = l.strip_prefix("author-time ") {
                at = v.parse().unwrap();
            } else if let Some(v) = l.strip_prefix("summary ") {
                su = v.into();
            } else if let Some(v) = l.strip_prefix("filename ") {
                fnm = v.into();
            } else {
                let f: Vec<&str> = l.split(' ').collect();
                if f.len() >= 3 && f[0].len() == 40 && f[0].bytes().all(|c| c.is_ascii_hexdigit()) {
                    cur = Some((f[0].into(), f[1].parse().unwrap(), f[2].parse().unwrap()));
                }
            }
        }
        let got: Vec<_> = r
            .blame(&Treeish::Commit(head.clone()), p, None)
            .unwrap()
            .into_iter()
            .map(|b| (b.line_no, b.sha, b.orig_line, b.author, b.time, b.summary, format!("{}\u{0}{}", b.orig_path, b.text)))
            .collect();
        if got == want { blame_ok += 1 } else { blame_bad += 1; println!("  blame differs for {p}: {} vs {} lines", got.len(), want.len()); }
    }
    rep.check(&format!("blame: {} largest tracked files == `git blame --line-porcelain` (sha,orig_line,author,time,summary,filename,text)", blame_ok + blame_bad), blame_bad == 0 && blame_ok > 0, || format!("{blame_bad} differ"));

    // ── file_history ──
    if let Some(p) = cand.iter().find(|p| !git(&dir, &["log", "--format=%H", "-n1", "--", p]).is_empty()) {
        let h = r.file_history(p, None, 1000).unwrap();
        let want = lossy(&git(&dir, &["log", "--follow", "--format=%H", "-n", "1000", "--", p]));
        rep.check(&format!("file_history({p}) == `git log --follow` ({} commits)", h.len()), h.iter().map(|c| c.sha.as_str()).eq(want.lines()), || "differs".into());
    }

    // ── search vs rg ──
    for pat in &patterns {
        for (label, regex, cs) in [("literal", false, true), ("literal -i", false, false), ("regex", true, true)] {
            let pattern = if regex { format!("{}\\w", regex_escape(pat)) } else { pat.clone() };
            let mut rg: Vec<&str> = vec!["-n", "--no-heading", "--color=never", "--no-config"];
            if !regex { rg.push("-F"); }
            if !cs { rg.push("-i"); }
            rg.push("-e");
            rg.push(&pattern);
            let out = sh(&dir, "rg", &rg);
            let want: BTreeSet<(String, u32, String)> = out
                .split(|&b| b == b'\n')
                .filter(|l| !l.is_empty())
                .map(|l| {
                    let s = lossy(l);
                    let mut p = s.splitn(3, ':');
                    (p.next().unwrap().to_string(), p.next().unwrap().parse().unwrap(), p.next().unwrap_or("").trim_end_matches('\r').to_string())
                })
                .collect();
            let (tx, rx) = mpsc::sync_channel(1024);
            let root = dir.clone();
            let q = SearchQuery { pattern: pattern.clone(), regex, case_sensitive: cs, globs: vec![] };
            let h = std::thread::spawn(move || search_content(&root, &q, Arc::new(AtomicBool::new(false)), tx));
            let got: BTreeSet<(String, u32, String)> = rx.iter().map(|x| (x.path, x.line, x.text)).collect();
            let st = h.join().unwrap().unwrap();
            let pass = got == want;
            rep.check(&format!("search {label} {pat:?}: {} hits == rg ({} ms)", want.len(), st.elapsed_ms), pass, || {
                let miss: Vec<_> = want.difference(&got).take(2).collect();
                let extra: Vec<_> = got.difference(&want).take(2).collect();
                format!("ours {} vs rg {}; missing {miss:?} extra {extra:?}", got.len(), want.len())
            });
        }
    }

    println!("\n{} checks passed, {} mismatches", rep.ok, rep.bad);
    if rep.bad > 0 {
        std::process::exit(1);
    }
}

fn regex_escape(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            o.push('\\');
        }
        o.push(c);
    }
    o
}
