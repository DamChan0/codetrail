//! Bounded child processes: every command the app spawns itself goes through here, so it is
//! always reaped, never outlives its timeout, and dies when its job is cancelled.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Captured stdout is capped; a runaway producer cannot grow our memory.
const MAX_STDOUT: u64 = 32 * 1024 * 1024;

pub struct Out {
    pub ok: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Runs `cmd` to completion. Killed (and reaped) on timeout or when `cancel` is raised.
pub fn run_bounded(mut cmd: Command, timeout: Duration, cancel: Option<&AtomicBool>) -> Result<Out, String> {
    use std::os::unix::process::CommandExt;
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut so = child.stdout.take().expect("piped");
    let mut se = child.stderr.take().expect("piped");
    let out_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = (&mut so).take(MAX_STDOUT).read_to_end(&mut v);
        // Drain the rest so the child never blocks on a full pipe.
        let _ = std::io::copy(&mut so, &mut std::io::sink());
        v
    });
    let err_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = (&mut se).take(64 * 1024).read_to_end(&mut v);
        let _ = std::io::copy(&mut se, &mut std::io::sink());
        String::from_utf8_lossy(&v).into_owned()
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Ok(s),
            Ok(None) => {}
            Err(e) => break Err(e.to_string()),
        }
        if start.elapsed() >= timeout || cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            let why = if start.elapsed() >= timeout { "timed out" } else { "cancelled" };
            kill_group(&mut child);
            let _ = out_t.join();
            let _ = err_t.join();
            return Err(format!("git {why}"));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    status.map(|s| Out { ok: s.success(), stdout, stderr })
}

/// SIGKILL the child's whole process group (it is its own group leader), then reap it.
fn kill_group(child: &mut std::process::Child) {
    let pgid = child.id();
    let _ = Command::new("kill").args(["-KILL", "--", &format!("-{pgid}")]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    let _ = child.kill();
    let _ = child.wait();
}

/// Fire-and-forget helper (xdg-open, notify-send): a detached thread waits for it with a timeout.
pub fn spawn_reaped(name: &str, cmd: Command, timeout: Duration) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(move || {
        let _ = run_bounded(cmd, timeout, None);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_output_and_exit_status() {
        let mut c = Command::new("sh");
        c.args(["-c", "echo hi; echo err >&2; exit 3"]);
        let o = run_bounded(c, Duration::from_secs(5), None).unwrap();
        assert!(!o.ok);
        assert_eq!(o.stdout, b"hi\n");
        assert_eq!(o.stderr.trim(), "err");
    }

    #[test]
    fn timeout_kills_the_whole_process_group_and_returns_promptly() {
        let marker = format!("ct-sleep-{}", std::process::id());
        let mut c = Command::new("bash");
        // The inner sleep is a grandchild; killing only the shell would leave it running.
        c.args(["-c", &format!("sleep 60 & exec -a {marker} sleep 61")]);
        let t = Instant::now();
        let e = run_bounded(c, Duration::from_millis(300), None).err().unwrap();
        assert!(e.contains("timed out"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(200));
        let alive = std::process::Command::new("pgrep").args(["-f", &marker]).output().unwrap();
        assert!(alive.stdout.is_empty(), "grandchild survived");
    }

    #[test]
    fn cancel_flag_kills_the_child() {
        let cancel = AtomicBool::new(true);
        let mut c = Command::new("sleep");
        c.arg("30");
        let t = Instant::now();
        let e = run_bounded(c, Duration::from_secs(30), Some(&cancel)).err().unwrap();
        assert!(e.contains("cancelled"));
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn huge_stdout_is_capped_not_buffered_without_bound() {
        let mut c = Command::new("sh");
        c.args(["-c", "head -c 40000000 /dev/zero"]);
        let o = run_bounded(c, Duration::from_secs(20), None).unwrap();
        assert_eq!(o.stdout.len() as u64, MAX_STDOUT);
    }
}
