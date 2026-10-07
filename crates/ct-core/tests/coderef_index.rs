use ct_core::*;

fn cr(path: &str, s: u32, e: u32, at: RefAt) -> CodeRef {
    CodeRef { path: path.into(), start: s, end: e, at }
}

#[test]
fn coderef_parse_forms() {
    assert_eq!(CodeRef::parse("src/a.rs:10-24@abc1234").unwrap(), cr("src/a.rs", 10, 24, RefAt::Commit("abc1234".into())));
    assert_eq!(CodeRef::parse("src/a.rs:10-24@worktree").unwrap(), cr("src/a.rs", 10, 24, RefAt::Worktree));
    assert_eq!(CodeRef::parse("src/a.rs:10").unwrap(), cr("src/a.rs", 10, 10, RefAt::Worktree));
    assert_eq!(CodeRef::parse("src/a.rs:10@ABCDEF1").unwrap(), cr("src/a.rs", 10, 10, RefAt::Commit("abcdef1".into())));
    assert_eq!(CodeRef::parse("  a.rs:3-3@worktree ").unwrap().end, 3);
    // names containing ':' and '@'
    assert_eq!(CodeRef::parse("we ird@dir/a:b.rs:7-8@abc1234").unwrap().path, "we ird@dir/a:b.rs");
    assert_eq!(CodeRef::parse("a@b.rs:3").unwrap(), cr("a@b.rs", 3, 3, RefAt::Worktree));
    assert_eq!(CodeRef::parse("한글 폴더/파일.rs:1-2@worktree").unwrap().path, "한글 폴더/파일.rs");
    let full = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(CodeRef::parse(&format!("x:1@{full}")).unwrap().at, RefAt::Commit(full.into()));
    assert_eq!("x.rs:2-3@abcd".parse::<CodeRef>().unwrap().start, 2);
}

#[test]
fn coderef_roundtrip() {
    for r in [
        cr("src/a.rs", 10, 24, RefAt::Commit("abc1234".into())),
        cr("a b/한글.rs", 5, 5, RefAt::Worktree),
        cr("a@b:c.rs", 1, 9, RefAt::Worktree),
    ] {
        let s = r.to_string();
        assert_eq!(CodeRef::parse(&s).unwrap(), r, "{s}");
    }
    assert_eq!(cr("p", 4, 4, RefAt::Worktree).to_string(), "p:4@worktree");
    assert_eq!(cr("p", 4, 9, RefAt::Commit("abcd".into())).to_string(), "p:4-9@abcd");
}

#[test]
fn coderef_rejects_garbage() {
    for bad in [
        "", "   ", "garbage", "a.rs", "a.rs:", ":10", "a.rs:0", "a.rs:0-3", "a.rs:5-3", "a.rs:x", "a.rs:1-", "a.rs:-3",
        "a.rs:1-2-3", "a.rs:1@", "a.rs:1@xyz", "a.rs:1@abc", "a.rs:1@worktre", "a.rs:99999999999", "a.rs:1.5", "a.rs:1@abcd@worktree",
        "a.rs:1@0123456789abcdef0123456789abcdef012345678",
    ] {
        match CodeRef::parse(bad) {
            Err(Error::Invalid(m)) => assert!(m.contains("invalid code ref"), "{bad}: {m}"),
            other => panic!("{bad:?} should be rejected, got {other:?}"),
        }
    }
}

fn idx(paths: &[&str]) -> FileIndex {
    FileIndex::build(paths.iter().map(|s| s.to_string()).collect())
}

#[test]
fn fileindex_ranks_and_reports_indices() {
    let i = idx(&["src/main.rs", "src/lib.rs", "docs/maintenance.md", "README.md", "src/한글/파일.rs", "tests/main_test.rs"]);
    let r = i.query("main", 10);
    assert_eq!(r[0].0, "src/main.rs");
    assert!(r.iter().any(|x| x.0 == "docs/maintenance.md"));
    assert!(!r.iter().any(|x| x.0 == "src/lib.rs"));
    // indices are char positions pointing at the matched chars
    let (p, _, ix) = &r[0];
    let chars: Vec<char> = p.chars().collect();
    let got: String = ix.iter().map(|&k| chars[k as usize]).collect();
    assert_eq!(got.to_lowercase(), "main");
    // sorted descending by score
    assert!(r.windows(2).all(|w| w[0].1 >= w[1].1));
    // fuzzy subsequence and multi-atom
    assert!(i.query("srlib", 5).iter().any(|x| x.0 == "src/lib.rs"));
    assert_eq!(i.query("tests main", 5)[0].0, "tests/main_test.rs");
    // smart case: uppercase → case sensitive
    assert_eq!(i.query("README", 5)[0].0, "README.md");
    assert!(i.query("Main", 5).is_empty());
    assert_eq!(i.query("readme", 5)[0].0, "README.md");
    // Korean: indices are char (not byte) offsets
    let k = i.query("파일", 5);
    assert_eq!(k[0].0, "src/한글/파일.rs");
    assert_eq!(k[0].2, vec![7, 8]);
    assert!(i.query("zzzzqq", 5).is_empty());
}

#[test]
fn fileindex_limit_empty_query_and_empty_index() {
    let i = idx(&["a", "b", "c"]);
    assert_eq!(i.query("", 2).len(), 2);
    assert_eq!(i.query("  ", 10).len(), 3);
    assert!(i.query("a", 0).is_empty());
    let e = FileIndex::build(vec![]);
    assert!(e.query("x", 5).is_empty());
    assert!(e.is_empty());
    // more matches than limit → best `limit` returned, still sorted
    let many: Vec<String> = (0..500).map(|n| format!("dir{n}/file{n}.rs")).collect();
    let i = FileIndex::build(many);
    let r = i.query("file", 7);
    assert_eq!(r.len(), 7);
    assert!(r.windows(2).all(|w| w[0].1 >= w[1].1));
}
