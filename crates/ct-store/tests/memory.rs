//! Memory bound of log reads. Own test binary: it reads the process peak RSS (VmHWM), so nothing else
//! may run in this process. `CT_STORE_BIG_MB=200` reproduces the 200 MB acceptance measurement.
use ct_store::*;
use std::fs;

fn fat(path: &str, ts: i64) -> Record {
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
        post_blob: Some("1".repeat(40)),
        spans: (0..std::env::var("CT_STORE_SPANS").ok().and_then(|v| v.parse().ok()).unwrap_or(150u32)).map(|i| Span { new_start: i * 3 + 1, new_len: 2, fingerprint: u64::from(i).wrapping_mul(0x9E37_79B9_7F4A_7C15), occurrence: 0 }).collect(),
        reason: vec![],
        transcript: None,
    }
}

fn status_kb(key: &str) -> u64 {
    let s = fs::read_to_string("/proc/self/status").unwrap();
    s.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse().ok()).unwrap()
}

#[test]
fn querying_one_file_of_a_large_log_does_not_load_the_log() {
    let mb: u64 = std::env::var("CT_STORE_BIG_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(24);
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    let mut i = 0i64;
    while fs::metadata(s.log_path()).map_or(0, |m| m.len()) < mb << 20 {
        for _ in 0..200 {
            i += 1;
            s.append(&fat(&format!("src/dir{}/file{}.rs", i % 50, i % 400), i)).unwrap();
        }
    }
    s.append_note(fat("x", 1).id, Agent::Claude, "s1", "orphan").unwrap();
    drop(s);
    let log_kb = fs::metadata(d.path().join("codetrail/log.ct")).unwrap().len() / 1024;
    let _ = fs::remove_file(d.path().join("codetrail/index.ct")); // cold: force the streaming scan

    fs::write("/proc/self/clear_refs", "5").unwrap(); // reset VmHWM to current RSS
    let base_kb = status_kb("VmHWM:");
    let s = Store::open(d.path()).unwrap();
    let hits = s.for_path("src/dir1/file1.rs");
    let scan_peak = status_kb("VmHWM:") - base_kb;
    assert!(!hits.is_empty() && hits.iter().all(|r| r.path == "src/dir1/file1.rs"));
    assert_eq!(s.stats().records as i64, i);
    assert_eq!(s.stats().orphan_notes, 1);

    drop(s);
    fs::write("/proc/self/clear_refs", "5").unwrap();
    let base2_kb = status_kb("VmRSS:");
    // warm cache path (what every later open does)
    let s2 = Store::open(d.path()).unwrap();
    assert!(s2.stats().from_cache);
    assert_eq!(s2.for_path("src/dir1/file1.rs").len(), hits.len());
    let peak = (status_kb("VmHWM:") - base2_kb.min(status_kb("VmHWM:"))).max(scan_peak);
    eprintln!("log={} MB records={i} cold-scan peak +{} MB, with cache +{} MB", log_kb / 1024, scan_peak / 1024, peak / 1024);
    let limit_kb = (log_kb / 3).min(64 * 1024);
    assert!(peak < limit_kb, "peak RSS grew {peak} kB for a {log_kb} kB log (limit {limit_kb} kB)");
}
