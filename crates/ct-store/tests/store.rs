use ct_store::*;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

fn rec(path: &str, ts: i64, post: Option<&str>, added: &[&str], start: u32) -> Record {
    let lines: Vec<String> = added.iter().map(|s| s.to_string()).collect();
    Record {
        id: new_id(ts),
        ts_ms: ts,
        agent: Agent::Claude,
        session: "s1".into(),
        kind: Kind::Edit,
        tool: "Edit".into(),
        tool_use_id: format!("toolu_{ts}"),
        head_at_edit: "0".repeat(40),
        path: path.into(),
        pre_blob: None,
        post_blob: post.map(String::from),
        spans: vec![Span {
            new_start: start,
            new_len: lines.len() as u32,
            fingerprint: fingerprint_lines(&lines),
            occurrence: 0,
        }],
        reason: vec![],
        transcript: None,
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

#[test]
fn roundtrip_note_and_export() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    let r = rec("a.rs", 1000, Some("b1"), &["x", "y"], 3);
    s.append(&r).unwrap();
    s.append_note(r.id, Agent::Claude, "s1", "because user asked").unwrap();
    s.append_note(r.id, Agent::Claude, "s1", "latest reason").unwrap();
    let got = s.for_path("a.rs");
    assert_eq!(got.len(), 1);
    assert_eq!(decode_reason(&got[0]), "latest reason");
    assert_eq!(s.find_id(&fmt_id(r.id)[..8]).unwrap().id, r.id);
    let mut out = Vec::new();
    s.export_json(&mut out).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["records"][0]["reason"], "latest reason");
    assert_eq!(v["notes"].as_array().unwrap().len(), 2);
    assert_eq!(s.for_range("a.rs", 4, 4, None).len(), 1);
    assert_eq!(s.for_range("a.rs", 9, 12, None).len(), 0);
}

#[test]
fn corruption_resync_skips_bad_frame() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    for i in 0..3 {
        s.append(&rec("a.rs", 1000 + i, None, &["l"], 1)).unwrap();
    }
    let log = s.log_path().to_path_buf();
    let mut b = fs::read(&log).unwrap();
    // flip a payload byte of the 2nd frame
    let first_len = u32::from_le_bytes(b[10..14].try_into().unwrap()) as usize;
    let second = 6 + 20 + first_len;
    b[second + 22] ^= 0xff;
    fs::write(&log, &b).unwrap();
    // plus garbage and a torn tail
    let mut f = fs::OpenOptions::new().append(true).open(&log).unwrap();
    f.write_all(b"garbage-garbage").unwrap();
    drop(f);
    let s = Store::open(d.path()).unwrap();
    assert_eq!(s.for_path("a.rs").len(), 2);
    assert_eq!(s.stats().skipped_frames, 2);
    // appending after garbage still works and is readable
    s.append(&rec("a.rs", 2000, None, &["m"], 1)).unwrap();
    assert_eq!(s.for_path("a.rs").len(), 3);
}

#[test]
fn truncated_tail_then_append_recovers() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    s.append(&rec("a.rs", 1, None, &["l"], 1)).unwrap();
    s.append(&rec("a.rs", 2, None, &["l"], 1)).unwrap();
    let log = s.log_path().to_path_buf();
    let len = fs::metadata(&log).unwrap().len();
    fs::OpenOptions::new().write(true).open(&log).unwrap().set_len(len - 5).unwrap();
    let s = Store::open(d.path()).unwrap();
    s.append(&rec("a.rs", 3, None, &["l"], 1)).unwrap();
    let st = s.stats();
    assert_eq!(st.records, 2);
    assert_eq!(st.skipped_frames, 1);
}

#[test]
fn concurrent_two_process_append() {
    if let Ok(dir) = std::env::var("CT_CHILD_DIR") {
        let tag = std::env::var("CT_CHILD_TAG").unwrap();
        let s = Store::open(Path::new(&dir)).unwrap();
        for i in 0..200 {
            s.append(&rec(&format!("{tag}.rs"), i, None, &["line"], 1)).unwrap();
        }
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let kids: Vec<_> = ["p1", "p2"]
        .iter()
        .map(|t| {
            Command::new(&exe)
                .args(["--exact", "concurrent_two_process_append", "--nocapture"])
                .env("CT_CHILD_DIR", d.path())
                .env("CT_CHILD_TAG", t)
                .spawn()
                .unwrap()
        })
        .collect();
    for mut k in kids {
        assert!(k.wait().unwrap().success());
    }
    let s = Store::open(d.path()).unwrap();
    assert_eq!(s.for_path("p1.rs").len(), 200);
    assert_eq!(s.for_path("p2.rs").len(), 200);
    assert_eq!(s.stats().skipped_frames, 0);
}

#[test]
fn version_mismatch_is_read_only() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    s.append(&rec("a.rs", 1, None, &["l"], 1)).unwrap();
    let log = s.log_path().to_path_buf();
    let mut b = fs::read(&log).unwrap();
    b[4] = 99;
    fs::write(&log, &b).unwrap();
    let s = Store::open(d.path()).unwrap();
    assert!(s.read_only_reason().unwrap().contains("version 99"));
    assert!(matches!(
        s.append(&rec("a.rs", 2, None, &["l"], 1)),
        Err(StoreError::ReadOnlyVersion { found: 99, .. })
    ));
    assert!(s.stats().read_only);
    assert_eq!(fs::read(&log).unwrap(), b, "file untouched");
}

#[test]
fn index_cache_hit_and_invalidation() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    s.append(&rec("a.rs", 1, None, &["l"], 1)).unwrap();
    assert!(!s.stats().from_cache);
    assert!(d.path().join("codetrail/index.ct").exists());
    let s2 = Store::open(d.path()).unwrap();
    assert!(s2.stats().from_cache);
    // append invalidates (len/mtime change)
    s2.append(&rec("a.rs", 2, None, &["l"], 1)).unwrap();
    let st = s2.stats();
    assert!(!st.from_cache);
    assert_eq!(st.records, 2);
    // corrupt cache -> discarded & regenerated
    fs::write(d.path().join("codetrail/index.ct"), b"junk").unwrap();
    let s3 = Store::open(d.path()).unwrap();
    assert!(!s3.stats().from_cache);
    assert_eq!(s3.stats().records, 2);
    assert!(Store::open(d.path()).unwrap().stats().from_cache);
}

// ------------------------------------------------ commit linking with real git

struct Repo {
    dir: tempfile::TempDir,
}
impl Repo {
    fn new() -> Repo {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        Repo { dir }
    }
    fn p(&self) -> &Path {
        self.dir.path()
    }
    fn write(&self, f: &str, c: &str) {
        fs::write(self.p().join(f), c).unwrap();
    }
    fn blob(&self, f: &str) -> String {
        git(self.p(), &["hash-object", f])
    }
    fn commit(&self, msg: &str) {
        git(self.p(), &["add", "-A"]);
        git(self.p(), &["commit", "-q", "-m", msg]);
    }
    fn blob_at(&self, rev: &str, f: &str) -> String {
        git(self.p(), &["rev-parse", &format!("{rev}:{f}")])
    }
}

const BASE: &str = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n";

#[test]
fn linking_high_medium_unknown_amend_rebase_cherry_pick() {
    let r = Repo::new();
    r.write("f.txt", BASE);
    r.commit("base");
    // "agent" edit: add two lines after line 2
    let edited = BASE.replacen("b\n", "b\nNEW1\nNEW2\n", 1);
    r.write("f.txt", &edited);
    let post = r.blob("f.txt");
    let store_dir = tempfile::tempdir().unwrap();
    let s = Store::open(store_dir.path()).unwrap();
    s.append(&rec("f.txt", 10, Some(&post), &["NEW1", "NEW2"], 3)).unwrap();

    // 1. plain commit -> blob equal -> High
    r.commit("feature");
    let c1 = r.blob_at("HEAD", "f.txt");
    let m = s.match_hunk("f.txt", &["NEW1".into(), "NEW2".into()], &[c1.clone()]);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].1, Confidence::High);
    assert_eq!(best_confidence(&m), Confidence::High);

    // 2. amend with an extra unrelated edit -> blob differs, text same -> Medium
    r.write("f.txt", &edited.replace("l\n", "l\nTAIL\n"));
    git(r.p(), &["add", "-A"]);
    git(r.p(), &["commit", "-q", "--amend", "--no-edit"]);
    let c2 = r.blob_at("HEAD", "f.txt");
    assert_ne!(c1, c2);
    let m = s.match_hunk("f.txt", &["NEW1".into(), "NEW2".into()], &[c2]);
    assert_eq!(m.iter().map(|x| x.1).collect::<Vec<_>>(), vec![Confidence::Medium]);

    // 3. rebase onto a diverged base that touched another line of the same file
    git(r.p(), &["branch", "feat"]);
    git(r.p(), &["checkout", "-q", "-b", "other", "main~1"]);
    r.write("f.txt", &BASE.replace("k\n", "K2\n"));
    r.commit("other");
    git(r.p(), &["checkout", "-q", "feat"]);
    git(r.p(), &["rebase", "-q", "other"]);
    let c3 = r.blob_at("HEAD", "f.txt");
    let m = s.match_hunk("f.txt", &["NEW1".into(), "NEW2".into()], &[c3]);
    assert_eq!(best_confidence(&m), Confidence::Medium);

    // 4. cherry-pick onto a third branch
    git(r.p(), &["checkout", "-q", "-b", "third", "main~1"]);
    r.write("f.txt", &BASE.replace("a\n", "A2\n"));
    r.commit("third");
    let pick = git(r.p(), &["rev-parse", "feat"]);
    git(r.p(), &["cherry-pick", &pick]);
    let c4 = r.blob_at("HEAD", "f.txt");
    let m = s.match_hunk("f.txt", &["NEW1".into(), "NEW2".into()], &[c4]);
    assert_eq!(best_confidence(&m), Confidence::Medium);

    // 5. human rewrote the lines -> Unknown (no match at all)
    let m = s.match_hunk("f.txt", &["NEW1 changed".into(), "NEW2".into()], &["deadbeef".into()]);
    assert!(m.is_empty());
    assert_eq!(best_confidence(&m), Confidence::Unknown);

    // 6. delete-only hunk: only blob equality links
    assert!(s.match_hunk("f.txt", &[], &["deadbeef".into()]).is_empty());
    // trailing whitespace is normalised
    let m = s.match_hunk("f.txt", &["NEW1  ".into(), "NEW2\t".into()], &["x".into()]);
    assert_eq!(best_confidence(&m), Confidence::Medium);
    // other path never matches
    assert!(s.match_hunk("g.txt", &["NEW1".into(), "NEW2".into()], &[]).is_empty());
}

#[test]
fn occurrence_requires_enough_copies() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    let mut r = rec("a.rs", 1, None, &["dup"], 1);
    r.spans.push(Span { new_start: 9, new_len: 1, fingerprint: fingerprint_lines(&["dup"]), occurrence: 1 });
    s.append(&r).unwrap();
    // one copy: span 0 matches => Medium (record matched via first span)
    assert_eq!(s.match_hunk("a.rs", &["dup".into()], &[]).len(), 1);
    // record whose only span is occurrence 1 needs two copies
    let mut r2 = rec("b.rs", 2, None, &["dup"], 1);
    r2.spans[0].occurrence = 1;
    s.append(&r2).unwrap();
    assert!(s.match_hunk("b.rs", &["dup".into()], &[]).is_empty());
    assert_eq!(s.match_hunk("b.rs", &["dup".into(), "x".into(), "dup".into()], &[]).len(), 1);
}
