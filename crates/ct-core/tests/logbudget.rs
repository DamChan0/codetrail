//! Streamed git output is bounded in total, and commit metadata is bounded per field and per call.
mod common;

use common::T;
use ct_core::*;

fn vm_hwm_kb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    s.lines().find(|l| l.starts_with("VmHWM")).and_then(|l| l.split_whitespace().nth(1)).and_then(|n| n.parse().ok()).unwrap()
}

/// 200 commits whose subject is one 1 MiB line (200 MiB of `git log` output).
fn fat_repo() -> T {
    let t = T::new();
    let subject = "S".repeat(1024 * 1024);
    t.write("msg.txt", subject.as_bytes());
    for i in 0..200 {
        t.git(&["commit", "-q", "--allow-empty", "-F", "msg.txt", &format!("--author=A{i} {} <a@x>", "N".repeat(8000))]);
    }
    std::fs::remove_file(t.path().join("msg.txt")).unwrap();
    t
}

#[test]
fn fat_commit_messages_stay_bounded() {
    let t = fat_repo();
    let r = t.repo();
    let before = vm_hwm_kb();
    let log = r.log(&LogQuery { limit: 200, ..Default::default() }).unwrap();
    assert_eq!(log.len(), 200);
    for c in &log {
        // 4 KiB + the ellipsis
        assert!(c.subject.len() <= 4096 + 3, "{}", c.subject.len());
        assert!(c.subject.ends_with('…'));
        assert!(c.author.len() <= 4096 + 3, "{}", c.author.len());
    }
    let held: usize = log.iter().map(|c| c.subject.len() + c.author.len() + c.email.len()).sum();
    assert!(held < 3 * 1024 * 1024, "{held} bytes of metadata retained");
    // transient memory is one line at a time, not the sum of lines
    let grown_mb = (vm_hwm_kb().saturating_sub(before)) / 1024;
    assert!(grown_mb < 64, "peak RSS grew {grown_mb} MiB");

    let hist = r.file_history("nope", None, 200).unwrap();
    assert!(hist.is_empty());
    // cumulative cap on the streamed output: 50 MiB < 200 MiB of log text
    let e = t.repo().with_max_total_output(50 * 1024 * 1024).log(&LogQuery { limit: 200, ..Default::default() }).unwrap_err();
    assert!(matches!(e, Error::TooLarge { limit, .. } if limit == 50 * 1024 * 1024), "{e:?}");
    // the handle stays usable
    assert_eq!(t.repo().log(&LogQuery { limit: 3, ..Default::default() }).unwrap().len(), 3);
}

#[test]
fn log_limit_is_clamped() {
    let t = T::new();
    t.write("a", b"1");
    t.commit_all("one");
    // a absurd limit neither allocates up front nor passes through to git
    let r = t.repo();
    let v = r.log(&LogQuery { limit: usize::MAX, ..Default::default() }).unwrap();
    assert_eq!(v.len(), 1);
    assert_eq!(r.file_history("a", None, usize::MAX).unwrap().len(), 1);
    assert_eq!(ct_core::MAX_LOG_LIMIT, 20_000);
}
