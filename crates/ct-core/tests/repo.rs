mod common;

use common::{canon, git_in, T};
use ct_core::*;

fn d() -> DiffOpts {
    DiffOpts::default()
}

fn find<'a>(s: &'a DiffSet, path: &str) -> &'a FileStat {
    s.files.iter().find(|f| f.path == path).unwrap_or_else(|| panic!("{path} not in {:?}", s.files))
}

#[test]
fn empty_repo() {
    let t = T::new();
    t.write("untracked.txt", b"x\n");
    let r = t.repo();
    assert!(r.head().is_err());
    assert!(r.log(&LogQuery::default()).unwrap().is_empty());
    assert!(r.log(&LogQuery { all_refs: true, ..Default::default() }).unwrap().is_empty());
    assert!(r.refs().unwrap().is_empty());
    assert_eq!(r.list_files().unwrap(), vec!["untracked.txt"]);
    let s = r.diff(&Comparison::index_vs_worktree(), &d()).unwrap();
    assert!(s.files.is_empty());
}

#[test]
fn open_errors_and_subdir() {
    let tmp = tempfile::tempdir().unwrap();
    match Repo::open(tmp.path()) {
        Err(Error::NotARepo(_)) => {}
        other => panic!("expected NotARepo, got {other:?}"),
    }
    assert!(matches!(Repo::open(&tmp.path().join("nope")), Err(Error::NotARepo(_))));
    let t = T::new();
    t.write("a/b/c.txt", b"1\n");
    t.commit_all("one");
    let r = Repo::open(&t.path().join("a/b")).unwrap();
    assert_eq!(canon(&r.root), canon(t.path()));
    assert!(r.git_dir.ends_with(".git"));
    let r2 = Repo::open(&t.path().join("a/b/c.txt")).unwrap();
    assert_eq!(r2.root, r.root);
}

#[test]
fn bare_repo_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    git_in(tmp.path(), &["init", "-q", "--bare", "."]);
    assert!(matches!(Repo::open(tmp.path()), Err(Error::Invalid(_))));
}

#[test]
fn root_commit_is_diffed_against_empty_tree() {
    let t = T::new();
    t.write("a.txt", b"one\ntwo\n");
    t.write("dir/b.txt", b"x\n");
    let c = t.commit_all("root");
    let r = t.repo();
    assert_eq!(r.head().unwrap(), c);
    let cmp = Comparison::parent_of(&c).unwrap();
    let s = r.diff(&cmp, &d()).unwrap();
    assert_eq!(s.files.len(), 2);
    let a = find(&s, "a.txt");
    assert_eq!((a.status, a.add, a.del), (Status::Added, 2, 0));
    let f = r.file_diff(&cmp, "a.txt", &d()).unwrap();
    assert_eq!(f.status, Status::Added);
    assert_eq!(f.old_oid, None);
    assert!(f.new_oid.is_some());
    assert_eq!(f.hunks.len(), 1);
    assert_eq!((f.hunks[0].old_start, f.hunks[0].old_len, f.hunks[0].new_start, f.hunks[0].new_len), (0, 0, 1, 2));
    assert_eq!(f.hunks[0].lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["one", "two"]);
    assert!(f.hunks[0].lines.iter().all(|l| l.kind == LineKind::Add && l.old_no.is_none()));
    assert_eq!(f.hunks[0].lines[1].new_no, Some(2));
    // Parent n=2 of a root commit does not exist.
    assert!(r.diff(&Comparison::parent_n_of(&c, 2).unwrap(), &d()).is_err());
}

#[test]
fn merge_commit_first_parent_default_and_parent_n() {
    let t = T::new();
    t.write("base.txt", b"base\n");
    t.commit_all("base");
    t.git(&["checkout", "-q", "-b", "side"]);
    t.write("side.txt", b"side\n");
    let side = t.commit_all("side");
    t.git(&["checkout", "-q", "main"]);
    t.write("main.txt", b"main\n");
    let main = t.commit_all("main");
    t.git(&["merge", "-q", "--no-ff", "side", "-m", "merge"]);
    let m = t.git(&["rev-parse", "HEAD"]);
    let r = t.repo();
    let log = r.log(&LogQuery { limit: 1, ..Default::default() }).unwrap();
    assert_eq!(log[0].parents, vec![main.clone(), side.clone()]);

    let p1 = r.diff(&Comparison::parent_of(&m).unwrap(), &d()).unwrap();
    assert_eq!(p1.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["side.txt"]);
    let p2 = r.diff(&Comparison::parent_n_of(&m, 2).unwrap(), &d()).unwrap();
    assert_eq!(p2.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["main.txt"]);
    assert!(r.diff(&Comparison::parent_n_of(&m, 3).unwrap(), &d()).is_err());
    assert!(Comparison::parent_n_of(&m, 0).is_err());
    let f = r.file_diff(&Comparison::parent_of(&m).unwrap(), "side.txt", &d()).unwrap();
    assert_eq!(f.hunks.len(), 1);
    // first-parent log
    let fp = r.log(&LogQuery { first_parent: true, ..Default::default() }).unwrap();
    assert!(fp.iter().all(|c| c.sha != side));
}

#[test]
fn rename_with_edit_and_without_detection() {
    let t = T::new();
    let body: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    t.write("old name.txt", body.as_bytes());
    t.commit_all("a");
    std::fs::create_dir_all(t.path().join("new")).unwrap();
    t.git(&["mv", "old name.txt", "new/목적지 파일.txt"]);
    t.write("new/목적지 파일.txt", format!("{body}extra\n").as_bytes());
    let c = t.commit_all("rename");
    let r = t.repo();
    let cmp = Comparison::parent_of(&c).unwrap();
    let s = r.diff(&cmp, &d()).unwrap();
    assert_eq!(s.files.len(), 1);
    let f = &s.files[0];
    assert_eq!((f.status, f.path.as_str(), f.old_path.as_deref()), (Status::Renamed, "new/목적지 파일.txt", Some("old name.txt")));
    assert!(f.similarity.unwrap() > 50);
    assert_eq!((f.add, f.del), (1, 0));
    let fd = r.file_diff(&cmp, "new/목적지 파일.txt", &d()).unwrap();
    assert_eq!(fd.status, Status::Renamed);
    assert_eq!(fd.old_path.as_deref(), Some("old name.txt"));
    assert_eq!(fd.hunks.len(), 1);
    assert_eq!(fd.hunks[0].lines.iter().filter(|l| l.kind == LineKind::Add).count(), 1);

    let no = DiffOpts { detect_renames: false, ..d() };
    let s = r.diff(&cmp, &no).unwrap();
    let mut st: Vec<_> = s.files.iter().map(|f| f.status).collect();
    st.sort_by_key(|s| s.letter());
    assert_eq!(st, vec![Status::Added, Status::Deleted]);
    let del = r.file_diff(&cmp, "old name.txt", &no).unwrap();
    assert_eq!(del.status, Status::Deleted);
    assert_eq!(del.new_oid, None);
    assert!(del.hunks[0].lines.iter().all(|l| l.kind == LineKind::Del));
}

#[test]
fn binary_and_symlink_and_mode() {
    let t = T::new();
    t.write("keep.txt", b"k\n");
    t.commit_all("base");
    t.write("img.bin", &[0u8, 1, 2, 3, 255, 0, 0]);
    std::os::unix::fs::symlink("keep.txt", t.path().join("link")).unwrap();
    t.write("run.sh", b"#!/bin/sh\n");
    let c1 = t.commit_all("add");
    let r = t.repo();
    let cmp = Comparison::parent_of(&c1).unwrap();
    let s = r.diff(&cmp, &d()).unwrap();
    let bin = find(&s, "img.bin");
    assert!(bin.binary);
    assert_eq!(bin.kind, FileKind::Binary);
    let fd = r.file_diff(&cmp, "img.bin", &d()).unwrap();
    assert_eq!(fd.kind, FileKind::Binary);
    assert!(fd.hunks.is_empty());
    let lk = find(&s, "link");
    assert_eq!(lk.kind, FileKind::Symlink);
    assert_eq!(lk.new_mode, Some(0o120000));
    let fl = r.file_diff(&cmp, "link", &d()).unwrap();
    assert_eq!(fl.kind, FileKind::Symlink);
    assert_eq!(fl.hunks[0].lines[0].text, "keep.txt");
    assert!(fl.hunks[0].lines[0].no_newline_at_eof, "symlink target blob has no trailing newline");

    // mode change only
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(t.path().join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let c2 = t.commit_all("chmod");
    let cmp2 = Comparison::parent_of(&c2).unwrap();
    let s2 = r.diff(&cmp2, &d()).unwrap();
    let rs = find(&s2, "run.sh");
    assert_eq!((rs.old_mode, rs.new_mode), (Some(0o100644), Some(0o100755)));
    assert_eq!((rs.add, rs.del), (0, 0));
    let f2 = r.file_diff(&cmp2, "run.sh", &d()).unwrap();
    assert!(f2.hunks.is_empty());
    assert_eq!(f2.kind, FileKind::Text);

    // type change file -> symlink
    t.rm("keep.txt");
    std::os::unix::fs::symlink("link", t.path().join("keep.txt")).unwrap();
    let c3 = t.commit_all("typechange");
    let s3 = r.diff(&Comparison::parent_of(&c3).unwrap(), &d()).unwrap();
    assert_eq!(find(&s3, "keep.txt").status, Status::TypeChanged);
}

#[test]
fn no_newline_at_eof_flags() {
    let t = T::new();
    t.write("f.txt", b"a\nb");
    t.commit_all("no nl");
    t.write("f.txt", b"a\nb\n");
    let c2 = t.commit_all("add nl");
    t.write("f.txt", b"a\nb\nc");
    let c3 = t.commit_all("append no nl");
    let r = t.repo();
    let f = r.file_diff(&Comparison::parent_of(&c2).unwrap(), "f.txt", &d()).unwrap();
    let ls = &f.hunks[0].lines;
    // -b (no newline)  +b
    assert_eq!(ls.len(), 3);
    assert_eq!((ls[1].kind, ls[1].text.as_str(), ls[1].no_newline_at_eof), (LineKind::Del, "b", true));
    assert_eq!((ls[2].kind, ls[2].text.as_str(), ls[2].no_newline_at_eof), (LineKind::Add, "b", false));
    let f = r.file_diff(&Comparison::parent_of(&c3).unwrap(), "f.txt", &d()).unwrap();
    let ls = &f.hunks[0].lines;
    assert_eq!(ls.last().unwrap().text, "c");
    assert!(ls.last().unwrap().no_newline_at_eof);
    assert!(!ls[0].no_newline_at_eof);
}

#[test]
fn crlf_preserved_and_whitespace_option() {
    let t = T::new();
    t.write("w.txt", b"a\r\nb\r\n");
    t.commit_all("crlf");
    t.write("w.txt", b"a\r\nb  \r\n");
    let c = t.commit_all("ws");
    let r = t.repo();
    let cmp = Comparison::parent_of(&c).unwrap();
    let f = r.file_diff(&cmp, "w.txt", &d()).unwrap();
    assert!(f.hunks[0].lines.iter().any(|l| l.kind == LineKind::Add && l.text == "b  \r"));
    let s = r.diff(&cmp, &DiffOpts { ignore_whitespace: true, ..d() }).unwrap();
    assert!(s.files.is_empty());
}

#[test]
fn hunk_line_numbers_and_context_option() {
    let t = T::new();
    let a: String = (1..=30).map(|i| format!("l{i}\n")).collect();
    t.write("n.txt", a.as_bytes());
    t.commit_all("a");
    let b = a.replace("l10\n", "L10\n").replace("l25\n", "L25\nextra\n");
    t.write("n.txt", b.as_bytes());
    let c = t.commit_all("b");
    let r = t.repo();
    let cmp = Comparison::parent_of(&c).unwrap();
    let f = r.file_diff(&cmp, "n.txt", &d()).unwrap();
    assert_eq!(f.hunks.len(), 2);
    let h = &f.hunks[0];
    assert_eq!((h.old_start, h.old_len, h.new_start, h.new_len), (7, 7, 7, 7));
    let del = h.lines.iter().find(|l| l.kind == LineKind::Del).unwrap();
    assert_eq!((del.old_no, del.new_no, del.text.as_str()), (Some(10), None, "l10"));
    let add = h.lines.iter().find(|l| l.kind == LineKind::Add).unwrap();
    assert_eq!((add.old_no, add.new_no, add.text.as_str()), (None, Some(10), "L10"));
    let h2 = &f.hunks[1];
    assert_eq!(h2.lines.iter().filter(|l| l.kind == LineKind::Add).count(), 2);
    // more context merges the hunks
    let f = r.file_diff(&cmp, "n.txt", &DiffOpts { context: 20, ..d() }).unwrap();
    assert_eq!(f.hunks.len(), 1);
    let f0 = r.file_diff(&cmp, "n.txt", &DiffOpts { context: 0, ..d() }).unwrap();
    assert_eq!(f0.hunks.len(), 2);
    assert!(f0.hunks.iter().all(|h| h.lines.iter().all(|l| l.kind != LineKind::Ctx)));
    assert!(r.file_diff(&cmp, "absent.txt", &d()).is_err());
}

#[test]
fn unicode_space_and_special_paths() {
    let t = T::new();
    let names = ["한글 파일.txt", "sp ace/내 폴더/a b.rs", "tab\tname.txt", "glob[1]*.txt", "-dash.txt", "quote\"d.txt"];
    for n in names {
        t.write(n, format!("hello {n}\n").as_bytes());
    }
    let c = t.commit_all("many names");
    let r = t.repo();
    let cmp = Comparison::parent_of(&c).unwrap();
    let s = r.diff(&cmp, &d()).unwrap();
    for n in names {
        assert_eq!(find(&s, n).status, Status::Added, "{n}");
        let f = r.file_diff(&cmp, n, &d()).unwrap();
        assert_eq!(f.hunks[0].lines[0].text, format!("hello {n}"), "{n}");
        assert_eq!(r.show_file(&c, n).unwrap(), format!("hello {n}\n").into_bytes(), "{n}");
        let b = r.blame(&Treeish::Commit(c.clone()), n, None).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].orig_path, n);
    }
    let mut files = r.list_files().unwrap();
    files.sort();
    let mut want: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    want.sort();
    assert_eq!(files, want);
    let log = r.log(&LogQuery { path: Some("glob[1]*.txt".into()), ..Default::default() }).unwrap();
    assert_eq!(log.len(), 1);
    // literal pathspec: a glob must not match other files
    let log = r.log(&LogQuery { path: Some("*.txt".into()), ..Default::default() }).unwrap();
    assert!(log.is_empty());
}

#[test]
fn shallow_clone_and_linked_worktree_and_submodule() {
    // shallow
    let src = T::new();
    for i in 0..3 {
        src.write("f.txt", format!("v{i}\n").as_bytes());
        src.commit_all(&format!("c{i}"));
    }
    let dst = tempfile::tempdir().unwrap();
    let clone = dst.path().join("clone");
    git_in(dst.path(), &["clone", "-q", "--depth", "1", &format!("file://{}", src.path().display()), "clone"]);
    let r = Repo::open(&clone).unwrap();
    let log = r.log(&LogQuery::default()).unwrap();
    assert_eq!(log.len(), 1);
    assert!(log[0].parents.is_empty(), "shallow boundary has no visible parents");
    let s = r.diff(&Comparison::parent_of(&log[0].sha).unwrap(), &d()).unwrap();
    assert_eq!(s.files.len(), 1);
    assert_eq!(s.files[0].status, Status::Added);
    // origin/main shows up as remote ref
    assert!(r.refs().unwrap().iter().any(|x| x.kind == RefKind::Remote && x.name == "origin/main"));

    // linked worktree
    let wt = dst.path().join("wt");
    src.git(&["worktree", "add", "-q", "-b", "wtbranch", wt.to_str().unwrap()]);
    std::fs::write(wt.join("only-in-wt.txt"), "w\n").unwrap();
    let rw = Repo::open(&wt).unwrap();
    assert_eq!(canon(&rw.root), canon(&wt));
    assert_ne!(rw.git_dir, rw.common_dir);
    assert_eq!(canon(&rw.common_dir), canon(&src.path().join(".git")));
    assert_eq!(rw.log(&LogQuery::default()).unwrap().len(), 3);
    assert!(rw.list_files().unwrap().contains(&"only-in-wt.txt".to_string()));
    let rb = rw.refs().unwrap();
    assert!(rb.iter().any(|x| x.name == "wtbranch" && x.is_head));

    // submodule
    let sub = T::new();
    sub.write("s.txt", b"s1\n");
    let sub1 = sub.commit_all("sub1");
    let sup = T::new();
    sup.write("top.txt", b"t\n");
    sup.commit_all("top");
    sup.git(&["submodule", "add", "-q", sub.path().to_str().unwrap(), "libs/sub"]);
    let c = sup.commit_all("add submodule");
    let rs = sup.repo();
    let cmp = Comparison::parent_of(&c).unwrap();
    let s = rs.diff(&cmp, &d()).unwrap();
    let sm = find(&s, "libs/sub");
    assert_eq!(sm.kind, FileKind::Submodule);
    assert_eq!(sm.new_mode, Some(0o160000));
    let fd = rs.file_diff(&cmp, "libs/sub", &d()).unwrap();
    assert_eq!(fd.kind, FileKind::Submodule);
    assert_eq!(fd.new_oid.as_deref(), Some(sub1.as_str()));
    assert!(fd.hunks[0].lines.iter().any(|l| l.text.contains(&sub1)));
    assert!(rs.list_files().unwrap().contains(&"libs/sub".to_string()));
    // update the submodule pointer
    sub.write("s.txt", b"s2\n");
    sub.commit_all("sub2");
    sup.git(&["submodule", "update", "--remote", "-q"]);
    let c2 = sup.commit_all("bump");
    let s = rs.diff(&Comparison::parent_of(&c2).unwrap(), &d()).unwrap();
    assert_eq!(find(&s, "libs/sub").status, Status::Modified);
    // opening inside the checked-out submodule works
    let inner = Repo::open(&sup.path().join("libs/sub")).unwrap();
    assert_eq!(canon(&inner.root), canon(&sup.path().join("libs/sub")));
    assert_eq!(inner.log(&LogQuery::default()).unwrap().len(), 2);
}

#[test]
fn all_comparison_variants() {
    let t = T::new();
    t.write("a.txt", b"a1\n");
    t.write("b.txt", b"b1\n");
    let c1 = t.commit_all("c1");
    t.git(&["checkout", "-q", "-b", "feature"]);
    t.write("a.txt", b"a2\n");
    let c2 = t.commit_all("c2");
    t.git(&["checkout", "-q", "main"]);
    t.write("b.txt", b"b2\n");
    let c3 = t.commit_all("c3");
    // staged change + unstaged change + untracked
    t.write("a.txt", b"a1\nstaged\n");
    t.git(&["add", "a.txt"]);
    t.write("a.txt", b"a1\nstaged\nunstaged\n");
    t.write("untracked.txt", b"u\n");
    let r = t.repo();
    let paths = |c: &Comparison| -> Vec<(String, Status)> {
        let mut v: Vec<_> = r.diff(c, &d()).unwrap().files.into_iter().map(|f| (f.path, f.status)).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    };
    let m = |x: &str| (x.to_string(), Status::Modified);

    // range: c1 -> c2
    assert_eq!(paths(&Comparison::range(&c1, &c2).unwrap()), vec![m("a.txt")]);
    assert_eq!(paths(&Comparison::range(&c2, &c1).unwrap()), vec![m("a.txt")]);
    // range with refs (non-hex) is resolved
    assert_eq!(paths(&Comparison::range("main~1", "feature").unwrap()), vec![m("a.txt")]);
    // merge-base(feature, main) = c1 → vs main = c3 changes b.txt
    let mb = Comparison::merge_base("feature", "main", &r).unwrap();
    assert_eq!(mb.old, Treeish::Commit(c1.clone()));
    assert_eq!(paths(&mb), vec![m("b.txt")]);
    assert!(Comparison::merge_base("feature", "nonexistent", &r).is_err());
    // commit vs worktree (tracked only; untracked is not listed)
    assert_eq!(paths(&Comparison::commit_vs_worktree(&c3).unwrap()), vec![m("a.txt")]);
    // index vs worktree = unstaged
    let uw = Comparison::index_vs_worktree();
    assert_eq!(paths(&uw), vec![m("a.txt")]);
    let fd = r.file_diff(&uw, "a.txt", &d()).unwrap();
    assert_eq!(fd.hunks[0].lines.iter().filter(|l| l.kind == LineKind::Add).map(|l| l.text.as_str()).collect::<Vec<_>>(), ["unstaged"]);
    assert_eq!(fd.new_oid, None, "worktree side has no object id");
    assert!(fd.old_oid.is_some());
    // HEAD vs index = staged
    let hi = Comparison::commit_vs_index(&c3).unwrap();
    let fd = r.file_diff(&hi, "a.txt", &d()).unwrap();
    assert_eq!(fd.hunks[0].lines.iter().filter(|l| l.kind == LineKind::Add).map(|l| l.text.as_str()).collect::<Vec<_>>(), ["staged"]);
    // mirrored forms swap sides
    let wu = Comparison::new(Treeish::Worktree, Treeish::Index).unwrap();
    let fd = r.file_diff(&wu, "a.txt", &d()).unwrap();
    assert_eq!(fd.hunks[0].lines.iter().filter(|l| l.kind == LineKind::Del).map(|l| l.text.as_str()).collect::<Vec<_>>(), ["unstaged"]);
    let iw = Comparison::new(Treeish::Index, Treeish::Commit(c3.clone())).unwrap();
    let fd = r.file_diff(&iw, "a.txt", &d()).unwrap();
    assert_eq!(fd.hunks[0].lines.iter().filter(|l| l.kind == LineKind::Del).map(|l| l.text.as_str()).collect::<Vec<_>>(), ["staged"]);
    let wt_c = Comparison::new(Treeish::Worktree, Treeish::Commit(c3.clone())).unwrap();
    assert_eq!(paths(&wt_c), vec![m("a.txt")]);
    assert_eq!(paths(&Comparison::new(Treeish::Worktree, Treeish::Index).unwrap()), vec![m("a.txt")]);
    // parent_of
    assert_eq!(paths(&Comparison::parent_of(&c3).unwrap()), vec![m("b.txt")]);
    // invalid combinations
    assert!(Comparison::new(Treeish::Worktree, Treeish::Worktree).is_err());
    assert!(Comparison::new(Treeish::Index, Treeish::Index).is_err());
    assert!(r.diff(&Comparison { old: Treeish::Worktree, new: Treeish::Worktree }, &d()).is_err());
    assert!(Comparison::parent_of("").is_err());
    assert!(Comparison::range("--output=/tmp/x", "HEAD").is_err());
    assert!(r.diff(&Comparison::range("deadbeef", "HEAD").unwrap(), &d()).is_err());
}

#[test]
fn deleted_and_added_files_in_worktree_compare() {
    let t = T::new();
    t.write("keep.txt", b"k\n");
    t.write("gone.txt", b"g\n");
    let c = t.commit_all("base");
    t.rm("gone.txt");
    let r = t.repo();
    let s = r.diff(&Comparison::commit_vs_worktree(&c).unwrap(), &d()).unwrap();
    assert_eq!(s.files.len(), 1);
    assert_eq!((s.files[0].path.as_str(), s.files[0].status), ("gone.txt", Status::Deleted));
    assert!(!r.list_files().unwrap().contains(&"gone.txt".to_string()), "deleted-on-disk file is not listed");
}

#[test]
fn resolve_and_refs_and_merge_base() {
    let t = T::new();
    t.write("a", b"1\n");
    let c1 = t.commit_all("one");
    t.git(&["tag", "light"]);
    t.git(&["tag", "-a", "annot", "-m", "annotated"]);
    t.git(&["branch", "other"]);
    t.write("a", b"2\n");
    let c2 = t.commit_all("two");
    let r = t.repo();
    assert_eq!(r.resolve("HEAD").unwrap(), c2);
    assert_eq!(r.resolve("HEAD~1").unwrap(), c1);
    assert_eq!(r.resolve(&c1[..7]).unwrap(), c1);
    assert_eq!(r.resolve("annot").unwrap(), c1, "tags are peeled to commits");
    assert!(r.resolve("nope").is_err());
    assert!(r.resolve("-bad").is_err());
    assert_eq!(r.merge_base("other", "main").unwrap(), c1);
    let refs = r.refs().unwrap();
    let get = |n: &str| refs.iter().find(|x| x.name == n).unwrap_or_else(|| panic!("{n}")).clone();
    assert_eq!((get("main").kind, get("main").is_head, get("main").oid), (RefKind::Branch, true, c2.clone()));
    assert_eq!(get("other").oid, c1);
    assert!(!get("other").is_head);
    assert_eq!((get("light").kind, get("light").oid), (RefKind::Tag, c1.clone()));
    assert_eq!(get("annot").oid, c1, "annotated tag peeled");
    assert_eq!(get("main").full_name, "refs/heads/main");
}

#[test]
fn log_pagination_filters_and_decorations() {
    let t = T::new();
    for i in 0..25 {
        let (file, who) = if i % 5 == 0 { ("special.txt", "Alice") } else { ("common.txt", "Bob") };
        t.write(file, format!("v{i}\n").as_bytes());
        t.git(&["add", "-A"]);
        t.git(&["-c", &format!("user.name={who}"), "-c", "user.email=x@y", "commit", "-q", "--author", &format!("{who} <{}@e.com>", who.to_lowercase()), "-m", &format!("commit number {i} [{who}]")]);
    }
    t.git(&["tag", "v1"]);
    let r = t.repo();
    let all = r.log(&LogQuery { limit: 1000, ..Default::default() }).unwrap();
    assert_eq!(all.len(), 25);
    let page1 = r.log(&LogQuery { limit: 10, ..Default::default() }).unwrap();
    let page2 = r.log(&LogQuery { skip: 10, limit: 10, ..Default::default() }).unwrap();
    let page3 = r.log(&LogQuery { skip: 20, limit: 10, ..Default::default() }).unwrap();
    assert_eq!((page1.len(), page2.len(), page3.len()), (10, 10, 5));
    let joined: Vec<_> = page1.iter().chain(&page2).chain(&page3).map(|c| c.sha.clone()).collect();
    assert_eq!(joined, all.iter().map(|c| c.sha.clone()).collect::<Vec<_>>());
    assert_eq!(all[0].subject, "commit number 24 [Bob]");
    assert_eq!(all[0].author, "Bob");
    assert_eq!(all[0].email, "bob@e.com");
    assert!(all[0].time > 1_600_000_000);
    assert_eq!(all[0].parents, vec![all[1].sha.clone()]);
    assert!(all[24].parents.is_empty());
    assert!(all[0].refs.contains(&"HEAD -> main".to_string()) && all[0].refs.contains(&"tag: v1".to_string()), "{:?}", all[0].refs);
    assert!(all[1].refs.is_empty());
    assert!(r.log(&LogQuery { limit: 0, ..Default::default() }).unwrap().is_empty());
    assert!(r.log(&LogQuery { skip: 100, ..Default::default() }).unwrap().is_empty());
    // filters
    let by_path = r.log(&LogQuery { path: Some("special.txt".into()), limit: 100, ..Default::default() }).unwrap();
    assert_eq!(by_path.len(), 5);
    let by_author = r.log(&LogQuery { author: Some("alice".into()), limit: 100, ..Default::default() }).unwrap();
    assert_eq!(by_author.len(), 5);
    let by_email = r.log(&LogQuery { author: Some("@e.com".into()), limit: 100, ..Default::default() }).unwrap();
    assert_eq!(by_email.len(), 25);
    let by_msg = r.log(&LogQuery { message: Some("NUMBER 1".into()), limit: 100, ..Default::default() }).unwrap();
    assert_eq!(by_msg.len(), 11, "1, 10..19");
    let literal = r.log(&LogQuery { message: Some("[Bob]".into()), limit: 100, ..Default::default() }).unwrap();
    assert_eq!(literal.len(), 20, "message filter is literal, not regex");
    let both = r.log(&LogQuery { message: Some("number 1".into()), author: Some("Alice".into()), limit: 100, ..Default::default() }).unwrap();
    assert_eq!(both.len(), 2, "10 and 15");
    // rev
    let at = r.log(&LogQuery { rev: Some(all[5].sha.clone()), limit: 3, ..Default::default() }).unwrap();
    assert_eq!(at[0].sha, all[5].sha);
    assert!(r.log(&LogQuery { rev: Some("--all".into()), ..Default::default() }).is_err());
    assert!(r.log(&LogQuery { rev: Some("nonexistent".into()), ..Default::default() }).is_err());
    // all_refs
    t.git(&["branch", "side", &all[10].sha]);
    t.git(&["checkout", "-q", "-b", "wip", &all[10].sha]);
    t.write("w.txt", b"w\n");
    t.commit_all("only on wip");
    t.git(&["checkout", "-q", "main"]);
    let main_only = r.log(&LogQuery { limit: 1000, ..Default::default() }).unwrap();
    let every = r.log(&LogQuery { limit: 1000, all_refs: true, ..Default::default() }).unwrap();
    assert_eq!(main_only.len(), 25);
    assert_eq!(every.len(), 26);
}

#[test]
fn log_streams_and_stops_early_on_big_history() {
    let t = T::new();
    // 3000 commits via fast-import (fast)
    let mut s = String::new();
    for i in 0..3000 {
        s.push_str(&format!("commit refs/heads/main\ncommitter T <t@e.com> {} +0000\ndata 3\nc{}\n", 1_700_000_000 + i, i % 10));
        s.push_str(&format!("M 100644 inline f\ndata 2\n{}\n", i % 10));
    }
    let mut child = std::process::Command::new("git")
        .current_dir(t.path())
        .args(["fast-import", "--quiet", "--force"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success());
    t.git(&["checkout", "-q", "-f", "main"]);
    let r = t.repo();
    let start = std::time::Instant::now();
    let page = r.log(&LogQuery { limit: 7, ..Default::default() }).unwrap();
    assert_eq!(page.len(), 7);
    assert!(start.elapsed().as_secs() < 5);
    let all = r.log(&LogQuery { limit: 100_000, ..Default::default() }).unwrap();
    assert_eq!(all.len(), 3000);
}

#[test]
fn blame_matches_expectations_and_worktree_and_range() {
    let t = T::new();
    t.write("b.txt", b"one\ntwo\nthree\n");
    let c1 = t.commit_all("first");
    t.write("b.txt", b"one\nTWO\nthree\nfour\n");
    let c2 = t.commit_all("second");
    let r = t.repo();
    let b = r.blame(&Treeish::Commit(c2.clone()), "b.txt", None).unwrap();
    let shas: Vec<_> = b.iter().map(|l| l.sha.as_str()).collect();
    assert_eq!(shas, [c1.as_str(), c2.as_str(), c1.as_str(), c2.as_str()]);
    assert_eq!(b[1].text, "TWO");
    assert_eq!(b[1].summary, "second");
    assert_eq!(b[1].author, "Tester");
    assert_eq!((b[0].line_no, b[0].orig_line), (1, 1));
    assert_eq!((b[3].line_no, b[3].orig_line), (4, 4));
    assert!(b[0].time > 1_600_000_000);
    let part = r.blame(&Treeish::Commit(c2.clone()), "b.txt", Some((2, 3))).unwrap();
    assert_eq!(part.iter().map(|l| l.line_no).collect::<Vec<_>>(), [2, 3]);
    assert!(r.blame(&Treeish::Commit(c2.clone()), "b.txt", Some((3, 2))).is_err());
    assert!(r.blame(&Treeish::Commit(c2.clone()), "missing.txt", None).is_err());
    // Parent{c2,1} = c1
    let bp = r.blame(&Treeish::Parent { commit: c2.clone(), n: 1 }, "b.txt", None).unwrap();
    assert_eq!(bp.len(), 3);
    assert!(bp.iter().all(|l| l.sha == c1));
    // worktree: uncommitted line
    t.write("b.txt", b"one\nTWO\nthree\nfour\nfive\n");
    let bw = r.blame(&Treeish::Worktree, "b.txt", None).unwrap();
    assert_eq!(bw.len(), 5);
    assert_eq!(bw[4].sha, "0".repeat(40));
    assert_eq!(bw[4].text, "five");
    // index: staged content
    t.git(&["add", "b.txt"]);
    t.write("b.txt", b"changed\n");
    let bi = r.blame(&Treeish::Index, "b.txt", None).unwrap();
    assert_eq!(bi.len(), 5);
    assert_eq!(bi[0].sha, c1);
}

#[test]
fn blame_follows_rename_orig_path() {
    let t = T::new();
    let body: String = (1..=10).map(|i| format!("row {i}\n")).collect();
    t.write("before.txt", body.as_bytes());
    let c1 = t.commit_all("create");
    t.git(&["mv", "before.txt", "after.txt"]);
    t.commit_all("rename");
    let r = t.repo();
    let b = r.blame(&Treeish::Commit("HEAD".into()), "after.txt", None).unwrap();
    assert_eq!(b.len(), 10);
    assert!(b.iter().all(|l| l.sha == c1 && l.orig_path == "before.txt"));
}

#[test]
fn file_history_follow_and_line_range() {
    let t = T::new();
    t.write("h.txt", b"a\nb\nc\nd\n");
    let c1 = t.commit_all("create");
    t.write("h.txt", b"a\nB\nc\nd\n");
    let c2 = t.commit_all("touch line 2");
    t.write("h.txt", b"a\nB\nc\nD\n");
    let c3 = t.commit_all("touch line 4");
    t.git(&["mv", "h.txt", "moved.txt"]);
    let c4 = t.commit_all("rename");
    let r = t.repo();
    let h = r.file_history("moved.txt", None, 100).unwrap();
    assert_eq!(h.iter().map(|c| c.sha.as_str()).collect::<Vec<_>>(), [c4.as_str(), c3.as_str(), c2.as_str(), c1.as_str()]);
    let h = r.file_history("moved.txt", None, 2).unwrap();
    assert_eq!(h.len(), 2);
    let h = r.file_history("moved.txt", Some((2, 2)), 100).unwrap();
    let shas: Vec<_> = h.iter().map(|c| c.sha.as_str()).collect();
    assert_eq!(shas, [c2.as_str(), c1.as_str()]);
    assert!(r.file_history("moved.txt", Some((0, 2)), 10).is_err());
    assert!(r.file_history("moved.txt", None, 0).unwrap().is_empty());
}

#[test]
fn line_history_reports_path_and_line_range_per_commit() {
    let t = T::new();
    t.write("h.txt", b"a\nb\nc\nd\n");
    let c1 = t.commit_all("create");
    t.git(&["mv", "h.txt", "moved.txt"]);
    let _ = t.commit_all("rename");
    // two lines inserted above: the tracked line shifts from 2 to 4
    t.write("moved.txt", b"x\ny\na\nB\nc\nd\n");
    let c3 = t.commit_all("insert and touch");
    let r = t.repo();
    let h = r.line_history("moved.txt", (4, 4), 50).unwrap();
    let got: Vec<_> = h.iter().map(|x| (x.commit.sha.as_str(), x.path.as_str(), x.lines)).collect();
    assert_eq!(got, [(c3.as_str(), "moved.txt", Some((4, 4))), (c1.as_str(), "h.txt", Some((2, 2)))]);
    assert!(r.line_history("moved.txt", (0, 1), 5).is_err());
    assert!(r.line_history("moved.txt", (4, 4), 0).unwrap().is_empty());
    assert_eq!(r.line_history("moved.txt", (4, 4), 1).unwrap().len(), 1);
}

#[test]
fn show_file_and_worktree_read() {
    let t = T::new();
    t.write("s.txt", b"v1\n");
    let c1 = t.commit_all("one");
    t.write("s.txt", b"v2\n");
    t.git(&["add", "s.txt"]);
    t.write("s.txt", b"v3\n");
    let r = t.repo();
    assert_eq!(r.show_file(&c1, "s.txt").unwrap(), b"v1\n");
    assert_eq!(r.show_file("HEAD", "s.txt").unwrap(), b"v1\n");
    assert_eq!(r.show_file(":", "s.txt").unwrap(), b"v2\n");
    assert_eq!(r.read_worktree_file("s.txt").unwrap(), b"v3\n");
    assert!(r.read_worktree_file("../etc/passwd").is_err());
    assert!(r.read_worktree_file("/etc/passwd").is_err());
    assert!(r.show_file(&c1, "nope.txt").is_err());
    assert!(r.show_file("-x", "s.txt").is_err());
    t.write("bin.dat", &[0, 159, 146, 150]);
    let c2 = t.commit_all("bin");
    assert_eq!(r.show_file(&c2, "bin.dat").unwrap(), vec![0, 159, 146, 150]);
}

#[test]
fn list_files_includes_untracked_but_not_ignored() {
    let t = T::new();
    t.write(".gitignore", b"ignored.txt\nbuild/\n");
    t.write("tracked.txt", b"t\n");
    t.commit_all("base");
    t.write("new.txt", b"n\n");
    t.write("ignored.txt", b"i\n");
    t.write("build/out.o", b"o\n");
    let mut f = t.repo().list_files().unwrap();
    f.sort();
    assert_eq!(f, [".gitignore", "new.txt", "tracked.txt"]);
}

#[test]
fn cancel_flag_set_before_call_is_cancelled() {
    let t = T::new();
    t.write("a", b"1\n");
    t.commit_all("one");
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = t.repo().with_cancel(flag.clone());
    assert!(matches!(r.head(), Err(Error::Cancelled)));
    assert!(matches!(r.log(&LogQuery::default()), Err(Error::Cancelled)));
    flag.store(false, std::sync::atomic::Ordering::Relaxed);
    assert!(r.head().is_ok());
}
