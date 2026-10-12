//! Integration tests for remote::discover: fake `tailscale` binaries on PATH (timeout, flood,
//! failure) and ssh_config `Include` handling on a temp dir. PATH is process-global, so every test
//! that touches it holds LOCK.

use ct_core::remote::discover::*;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

static LOCK: Mutex<()> = Mutex::new(());

fn fake_tailscale(dir: &Path, body: &str) {
    let p = dir.join("tailscale");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn with_path<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let old = std::env::var_os("PATH");
    // keep /bin and /usr/bin so `sleep`/`head` resolve inside the fake script
    std::env::set_var("PATH", format!("{}:/usr/bin:/bin", dir.display()));
    let r = f();
    match old {
        Some(o) => std::env::set_var("PATH", o),
        None => std::env::remove_var("PATH"),
    }
    r
}

#[test]
fn fake_tailscale_ok() {
    let d = tempfile::tempdir().unwrap();
    fake_tailscale(d.path(), r#"echo '{"BackendState":"Running","Self":{"HostName":"me","TailscaleIPs":["100.1.1.1"],"Online":true},"Peer":{"k":{"HostName":"b","TailscaleIPs":["100.1.1.2"],"Online":true}}}'"#);
    let p = with_path(d.path(), || tailscale_peers(Duration::from_secs(5))).unwrap();
    assert_eq!(p.len(), 2);
    assert!(p[0].is_self);
}

#[test]
fn hung_daemon_times_out_and_child_is_killed() {
    let d = tempfile::tempdir().unwrap();
    fake_tailscale(d.path(), "sleep 30");
    let t = Instant::now();
    let e = with_path(d.path(), || tailscale_peers(Duration::from_millis(300))).unwrap_err();
    assert!(t.elapsed() < Duration::from_secs(3), "took {:?}", t.elapsed());
    assert!(e.to_string().contains("did not answer"), "{e}");
}

#[test]
fn flood_of_output_is_rejected_quickly() {
    let d = tempfile::tempdir().unwrap();
    fake_tailscale(d.path(), "exec yes '{\"a\":1}'");
    let t = Instant::now();
    let e = with_path(d.path(), || tailscale_peers(Duration::from_secs(5))).unwrap_err();
    assert!(t.elapsed() < Duration::from_secs(3), "took {:?}", t.elapsed());
    assert!(e.to_string().contains("unexpectedly large"), "{e}");
}

#[test]
fn daemon_down_gives_hint() {
    let d = tempfile::tempdir().unwrap();
    fake_tailscale(d.path(), "echo 'failed to connect to local tailscaled; it doesn'\"'\"'t appear to be running' >&2; exit 1");
    let e = with_path(d.path(), || tailscale_peers(Duration::from_secs(5))).unwrap_err().to_string();
    assert!(e.contains("daemon is not running"), "{e}");
}

#[test]
fn missing_binary_gives_hint() {
    let d = tempfile::tempdir().unwrap();
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let old = std::env::var_os("PATH");
    std::env::set_var("PATH", d.path());
    let e = tailscale_peers(Duration::from_secs(2));
    if let Some(o) = old {
        std::env::set_var("PATH", o);
    }
    assert!(e.unwrap_err().to_string().contains("not found in PATH"));
}

#[test]
fn include_globs_one_level_and_first_value_wins() {
    let d = tempfile::tempdir().unwrap();
    let ssh = d.path();
    std::fs::create_dir(ssh.join("conf.d")).unwrap();
    std::fs::write(ssh.join("conf.d/a.conf"), "Host inc1\n  HostName a.example\n  Include nested.conf\nHost shared\n  User fromA\n").unwrap();
    std::fs::write(ssh.join("conf.d/b.conf"), "Host inc2\n  Port 2200\n").unwrap();
    std::fs::write(ssh.join("conf.d/skip.txt"), "Host skipped\n").unwrap();
    std::fs::write(ssh.join("nested.conf"), "Host nested\n").unwrap();
    std::fs::write(ssh.join("config"), "Host first\n  HostName f\nInclude conf.d/*.conf\nHost shared\n  User fromMain\n  Port 1\n").unwrap();
    let h = ssh_config_hosts(&ssh.join("config"));
    let names: Vec<_> = h.iter().map(|x| x.alias.as_str()).collect();
    assert_eq!(names, ["first", "inc1", "shared", "inc2"]);
    let shared = h.iter().find(|x| x.alias == "shared").unwrap();
    assert_eq!(shared.user.as_deref(), Some("fromA")); // include came first
    assert_eq!(shared.port, Some(1));
    assert_eq!(h.iter().find(|x| x.alias == "inc2").unwrap().port, Some(2200));
}

#[test]
fn include_file_count_and_size_caps() {
    let d = tempfile::tempdir().unwrap();
    let ssh = d.path();
    for i in 0..20 {
        std::fs::write(ssh.join(format!("f{i:02}.conf")), format!("Host g{i:02}\n")).unwrap();
    }
    std::fs::write(ssh.join("config"), "Include f*.conf\n").unwrap();
    // main + at most 7 includes => 7 hosts from includes
    assert_eq!(ssh_config_hosts(&ssh.join("config")).len(), 7);

    // oversized file: only the first 256 KiB are read
    let mut big = String::new();
    while big.len() < 600 * 1024 {
        big.push_str("Host x\n");
    }
    std::fs::write(ssh.join("big"), &big).unwrap();
    std::fs::write(ssh.join("tail"), "").unwrap();
    assert_eq!(ssh_config_hosts(&ssh.join("big")).len(), 1);
}

#[test]
fn missing_or_binary_config_is_empty() {
    let d = tempfile::tempdir().unwrap();
    assert!(ssh_config_hosts(&d.path().join("nope")).is_empty());
    std::fs::write(d.path().join("bin"), [0xffu8, 0xfe, 0, 1, 2, 0x80]).unwrap();
    assert!(ssh_config_hosts(&d.path().join("bin")).is_empty());
    assert!(ssh_config_hosts(d.path()).is_empty()); // a directory
}

/// Real tailscale, if installed and logged in: must parse. Prints only counts.
#[test]
fn real_tailscale_if_present() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    match tailscale_peers(Duration::from_secs(5)) {
        Ok(p) => {
            eprintln!("real tailscale: {} nodes, {} online", p.len(), p.iter().filter(|x| x.online).count());
            assert!(p.len() <= 200);
            assert!(p.iter().all(|x| !x.ips.is_empty() && !x.name.is_empty()));
        }
        Err(e) => eprintln!("real tailscale unavailable: {e}"),
    }
}

/// Real ~/.ssh/config must parse without panic; prints the alias count only.
#[test]
fn real_ssh_config_parses() {
    if let Some(h) = std::env::var_os("HOME") {
        let n = ssh_config_hosts(&Path::new(&h).join(".ssh/config")).len();
        eprintln!("real ~/.ssh/config: {n} concrete aliases");
    }
}
