//! Resource limits of the git runner. One test per binary: it rewrites PATH and counts children.
mod common;

use common::T;
use ct_core::*;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

fn children_of_me() -> Vec<u32> {
    let me = std::process::id();
    let mut v = Vec::new();
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        let rest = &stat[stat.rfind(')').unwrap() + 2..];
        let mut f = rest.split(' ');
        let state = f.next().unwrap_or("");
        let ppid: u32 = f.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        if ppid == me && state != "Z" {
            v.push(pid);
        }
    }
    v
}

fn vm_hwm_kb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    s.lines().find(|l| l.starts_with("VmHWM")).and_then(|l| l.split_whitespace().nth(1)).and_then(|n| n.parse().ok()).unwrap()
}

fn shim(dir: &std::path::Path, body: &str) {
    let bin = dir.join("git");
    std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn git_children_are_bounded_killed_as_a_group_and_output_is_capped() {
    let t = T::new();
    t.write("a", b"1\n");
    t.commit_all("one");
    let repo = t.repo(); // opened with the real git, before PATH is rewritten
    let fake = tempfile::tempdir().unwrap();
    let old = std::env::var("PATH").unwrap();
    std::env::set_var("PATH", format!("{}:{old}", fake.path().display()));

    // 1. 50 threads → never more than 4 live git children
    shim(fake.path(), "exec sleep 0.2");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let peak = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut peak = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                peak = peak.max(children_of_me().len());
                std::thread::sleep(Duration::from_millis(2));
            }
            peak
        })
    };
    let t0 = Instant::now();
    let hs: Vec<_> = (0..50)
        .map(|_| {
            let r = repo.clone().with_timeout(Duration::from_secs(30));
            std::thread::spawn(move || {
                let _ = r.head();
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    let wall = t0.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let peak = peak.join().unwrap();
    eprintln!("MEASURED peak_git_children={peak} wall_50_calls={wall:?} slots_in_use_after={}", git_slots().in_use());
    assert!(peak >= 2 && peak <= MAX_GIT_PROCS, "peak {peak}");
    assert_eq!(git_slots().in_use(), 0);
    assert!(children_of_me().is_empty(), "leftover children {:?}", children_of_me());

    // 2. timeout kills the whole group (grandchild included) and frees the slot
    let pidfile = fake.path().join("gc");
    shim(fake.path(), &format!("sleep 60 &\necho $! > {}\nwait", pidfile.display()));
    let t0 = Instant::now();
    let e = repo.clone().with_timeout(Duration::from_millis(300)).head().unwrap_err();
    assert!(matches!(e, Error::Timeout(_)), "{e:?}");
    eprintln!("MEASURED timeout_kill_latency={:?}", t0.elapsed());
    std::thread::sleep(Duration::from_millis(100));
    let gc: u32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    let gone = !std::path::Path::new(&format!("/proc/{gc}")).exists()
        || std::fs::read_to_string(format!("/proc/{gc}/stat")).map(|s| s.contains(") Z")).unwrap_or(true);
    assert!(gone, "grandchild {gc} survived the timeout");
    assert_eq!(git_slots().in_use(), 0);
    assert!(children_of_me().is_empty(), "{:?}", children_of_me());

    // 3. saturated pool + short timeout → Timeout, not a hang
    shim(fake.path(), "exec sleep 2");
    let hold: Vec<_> = (0..MAX_GIT_PROCS)
        .map(|_| {
            let r = repo.clone().with_timeout(Duration::from_secs(5));
            std::thread::spawn(move || {
                let _ = r.head();
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    let t0 = Instant::now();
    let e = repo.clone().with_timeout(Duration::from_millis(200)).head().unwrap_err();
    assert!(matches!(e, Error::Timeout(_)), "{e:?}");
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
    for h in hold {
        h.join().unwrap();
    }

    // 4. endless output → TooLarge with bounded memory
    shim(fake.path(), "exec yes aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let before = vm_hwm_kb();
    let t0 = Instant::now();
    let e = repo.clone().with_max_output(8 << 20).head().unwrap_err();
    let grew = vm_hwm_kb().saturating_sub(before);
    assert!(matches!(e, Error::TooLarge { .. }), "{e:?}");
    eprintln!("MEASURED too_large_after={:?} peak_rss_growth_kb={grew} (cap 8 MiB)", t0.elapsed());
    assert!(grew < 40 * 1024, "grew {grew} KB");
    // a single endless line is capped too
    shim(fake.path(), "exec head -c 100000000 /dev/zero");
    let e = repo.clone().with_max_output(1 << 20).log(&LogQuery::default()).unwrap_err();
    assert!(matches!(e, Error::TooLarge { .. }), "{e:?}");
    assert_eq!(git_slots().in_use(), 0);
    assert!(children_of_me().is_empty(), "{:?}", children_of_me());
    std::env::set_var("PATH", old);
}
