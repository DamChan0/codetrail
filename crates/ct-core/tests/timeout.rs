//! Single test per binary: it rewrites PATH, which would race with other tests.
mod common;

use common::T;
use ct_core::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn alive(pid: &str) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat")).map(|s| s.contains(") Z")).unwrap_or(true)
}

#[test]
fn timeout_and_cancel_kill_the_child() {
    let t = T::new();
    t.write("a", b"1\n");
    t.commit_all("one");
    let repo = t.repo();

    let fake = tempfile::tempdir().unwrap();
    let pidfile = fake.path().join("pid");
    let script = format!("#!/bin/sh\necho $$ > {}\nexec sleep 30\n", pidfile.display());
    let bin = fake.path().join("git");
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let old = std::env::var("PATH").unwrap();
    std::env::set_var("PATH", format!("{}:{old}", fake.path().display()));

    // timeout
    let r = repo.clone().with_timeout(Duration::from_millis(300));
    let t0 = Instant::now();
    let e = r.head().unwrap_err();
    assert!(matches!(e, Error::Timeout(_)), "{e:?}");
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    let pid = std::fs::read_to_string(&pidfile).unwrap().trim().to_string();
    std::thread::sleep(Duration::from_millis(100));
    assert!(!alive(&pid), "child {pid} must be killed on timeout");

    // cancel mid-flight
    let flag = Arc::new(AtomicBool::new(false));
    let r = repo.clone().with_cancel(flag.clone());
    let f2 = flag.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        f2.store(true, Ordering::Relaxed);
    });
    let t0 = Instant::now();
    let e = r.log(&LogQuery::default()).unwrap_err();
    assert!(matches!(e, Error::Cancelled), "{e:?}");
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    let pid = std::fs::read_to_string(&pidfile).unwrap().trim().to_string();
    std::thread::sleep(Duration::from_millis(100));
    assert!(!alive(&pid), "child {pid} must be killed on cancel");
    std::env::set_var("PATH", old);
}
