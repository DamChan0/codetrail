//! Discovery helpers for the remote-connection UI: Tailscale peers and `~/.ssh/config` hosts.
//!
//! Everything here is bounded: the `tailscale` child gets a cleared environment, its own process
//! group, a wall-clock timeout and a stdout cap; the ssh config parser caps file count, file size,
//! line count and result size, so hostile or garbage input can never blow up CPU or memory.

use super::RemoteErrorKind;
use crate::{Error, Result};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Max peers returned by [`tailscale_peers`].
pub const MAX_PEERS: usize = 200;
/// Max bytes of `tailscale status --json` we accept.
const MAX_STATUS_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 16 * 1024;
/// ssh_config limits.
const MAX_CONFIG_FILES: usize = 8;
const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_CONFIG_LINES: usize = 20_000;
const MAX_HOSTS: usize = 2000;
const MAX_INCLUDE_ENTRIES: usize = 64;

fn remote_err(detail: impl Into<String>) -> Error {
    Error::Remote { kind: RemoteErrorKind::Other, detail: detail.into() }
}

// ---------------------------------------------------------------------------------------------
// Tailscale
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TailnetPeer {
    /// Short host name (`HostName`, or first DNS label).
    pub name: String,
    /// MagicDNS name without the trailing dot (may be empty if MagicDNS is off).
    pub dns_name: String,
    pub ips: Vec<String>,
    pub os: String,
    pub online: bool,
    pub is_self: bool,
}

impl TailnetPeer {
    /// Best address to hand to ssh: MagicDNS name, else first IP.
    pub fn address(&self) -> &str {
        if !self.dns_name.is_empty() {
            &self.dns_name
        } else {
            self.ips.first().map_or("", String::as_str)
        }
    }
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

fn peer_from(v: &Value, is_self: bool) -> Option<TailnetPeer> {
    if !v.is_object() {
        return None;
    }
    let ips: Vec<String> = v
        .get("TailscaleIPs")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).take(8).collect())
        .unwrap_or_default();
    if ips.is_empty() {
        return None;
    }
    let dns_name = str_of(v, "DNSName").trim_end_matches('.').to_string();
    let mut name = str_of(v, "HostName");
    if name.is_empty() {
        name = dns_name.split('.').next().unwrap_or("").to_string();
    }
    if name.is_empty() {
        name = ips[0].clone();
    }
    // The local node reports Online=true; a missing flag on Self still means "this machine".
    let online = v.get("Online").and_then(Value::as_bool).unwrap_or(is_self);
    Some(TailnetPeer { name, dns_name, ips, os: str_of(v, "OS"), online, is_self })
}

/// Pure parser for `tailscale status --json`. Unknown fields are ignored; a missing `Peer` map is
/// fine (only self is returned). Peers without any IP are skipped. Result: self first, then online
/// peers, then offline ones, each group sorted by name; capped at [`MAX_PEERS`].
pub fn parse_tailscale_status(json: &str) -> Result<Vec<TailnetPeer>> {
    let root: Value = serde_json::from_str(json).map_err(|e| remote_err(format!("unreadable `tailscale status --json` output: {e}")))?;
    if !root.is_object() {
        return Err(remote_err("unexpected `tailscale status --json` output (not an object)"));
    }
    if let Some(state) = root.get("BackendState").and_then(Value::as_str) {
        match state {
            "Running" => {}
            "NeedsLogin" | "NeedsMachineAuth" => {
                return Err(remote_err("Tailscale is not logged in: run `tailscale up` (or `sudo tailscale up`) and try again"))
            }
            "Stopped" => return Err(remote_err("Tailscale is stopped: run `tailscale up` and try again")),
            other => return Err(remote_err(format!("Tailscale is not running (state: {other}); start it with `tailscale up`"))),
        }
    }
    let mut peers: Vec<TailnetPeer> = Vec::new();
    if let Some(p) = root.get("Self").and_then(|s| peer_from(s, true)) {
        peers.push(p);
    }
    if let Some(map) = root.get("Peer").and_then(Value::as_object) {
        for v in map.values() {
            if let Some(p) = peer_from(v, false) {
                peers.push(p);
            }
        }
    }
    peers.sort_by(|a, b| {
        b.is_self
            .cmp(&a.is_self)
            .then(b.online.cmp(&a.online))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.dns_name.cmp(&b.dns_name))
    });
    peers.truncate(MAX_PEERS);
    Ok(peers)
}

struct Captured {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: Option<std::process::ExitStatus>,
    timed_out: bool,
    overflow: bool,
}

fn read_capped<R: Read + Send + 'static>(mut r: R, cap: usize) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut over = false;
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if buf.len() + n > cap {
                        over = true;
                        break; // dropping the pipe makes the child die of SIGPIPE
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
        }
        (buf, over)
    })
}

fn run_bounded(mut cmd: Command, timeout: Duration, max_stdout: usize) -> std::io::Result<Captured> {
    use std::os::unix::process::CommandExt;
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut child = cmd.spawn()?;
    let pid = child.id() as i32;
    let out = read_capped(child.stdout.take().expect("piped"), max_stdout);
    let err = read_capped(child.stderr.take().expect("piped"), MAX_STDERR_BYTES);
    let deadline = Instant::now() + timeout;
    let (mut status, mut timed_out, mut overflow) = (None, false, false);
    loop {
        if let Some(s) = child.try_wait()? {
            status = Some(s);
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if status.is_none() {
        // SAFETY: signal to the process group we created with process_group(0); the child is
        // still un-reaped here, so the pid cannot have been recycled.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = child.kill();
        status = child.wait().ok();
    }
    let (stdout, o) = out.join().unwrap_or_default();
    overflow |= o;
    let (stderr, _) = err.join().unwrap_or_default();
    Ok(Captured { stdout, stderr, status, timed_out, overflow })
}

/// Run `tailscale status --json` (5 s default budget supplied by the caller) and parse it.
/// Errors are `Error::Remote{kind: Other}` with an actionable hint.
pub fn tailscale_peers(timeout: Duration) -> Result<Vec<TailnetPeer>> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut cmd = Command::new("tailscale");
    cmd.args(["status", "--json"]).env_clear().env("PATH", &path);
    let cap = match run_bounded(cmd, timeout, MAX_STATUS_BYTES) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(remote_err("`tailscale` was not found in PATH: install Tailscale (https://tailscale.com/download) or enter the host manually"))
        }
        Err(e) => return Err(remote_err(format!("could not run `tailscale`: {e}"))),
    };
    if cap.timed_out {
        return Err(remote_err(format!(
            "`tailscale status` did not answer within {}s: the Tailscale daemon may be down (try `sudo systemctl start tailscaled`)",
            timeout.as_secs().max(1)
        )));
    }
    if cap.overflow {
        return Err(remote_err("`tailscale status --json` output is unexpectedly large; refusing to parse"));
    }
    let ok = cap.status.is_some_and(|s| s.success());
    if !ok {
        let msg = String::from_utf8_lossy(&cap.stderr);
        let msg = msg.trim();
        let low = msg.to_lowercase();
        let hint = if low.contains("connect") || low.contains("no such file") || low.contains("not running") || low.contains("failed to connect") {
            "the Tailscale daemon is not running: start it with `sudo systemctl start tailscaled`"
        } else if low.contains("permission") || low.contains("access denied") {
            "permission denied talking to tailscaled: run `sudo tailscale set --operator=$USER` once"
        } else if low.contains("login") || low.contains("logged out") {
            "Tailscale is not logged in: run `tailscale up`"
        } else {
            "`tailscale status` failed"
        };
        let first: String = msg.lines().next().unwrap_or("").chars().take(200).collect();
        return Err(remote_err(if first.is_empty() { hint.to_string() } else { format!("{hint} ({first})") }));
    }
    parse_tailscale_status(&String::from_utf8_lossy(&cap.stdout))
}

// ---------------------------------------------------------------------------------------------
// ~/.ssh/config
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SshHost {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
}

/// Split a config line into (lowercase keyword, rest). Keyword and value may be separated by
/// whitespace and/or a single `=`.
fn split_kw(line: &str) -> Option<(String, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(line.len());
    let kw = line[..end].to_ascii_lowercase();
    let mut rest = line[end..].trim_start();
    if let Some(r) = rest.strip_prefix('=') {
        rest = r.trim_start();
    }
    Some((kw, rest))
}

/// Split a value list honoring double quotes; an unquoted `#` starts a comment.
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut has = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_q = !in_q;
                has = true;
            }
            '#' if !in_q => break,
            c if c.is_whitespace() && !in_q => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
        if out.len() >= 64 {
            break;
        }
    }
    if has {
        out.push(cur);
    }
    out
}

fn is_concrete_alias(p: &str) -> bool {
    !p.is_empty() && !p.starts_with('!') && !p.contains(['*', '?']) && !p.contains(|c: char| c.is_control()) && p.len() <= 255 && !p.starts_with('-')
}

/// Parse ssh_config text (includes are NOT followed here; see [`ssh_config_hosts`]).
///
/// Only concrete `Host` aliases are returned: wildcard and negated patterns are skipped, and
/// `Host *` options are not applied to anything. Like OpenSSH, the first value obtained for a
/// keyword wins, also across repeated `Host` blocks naming the same alias. `Match` blocks are
/// ignored. Tolerates BOM, CRLF and `Keyword=value`.
pub fn parse_ssh_config(text: &str) -> Vec<SshHost> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut hosts: Vec<SshHost> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    for line in text.lines().take(MAX_CONFIG_LINES) {
        let Some((kw, rest)) = split_kw(line) else { continue };
        match kw.as_str() {
            "host" => {
                current.clear();
                for p in split_args(rest) {
                    if !is_concrete_alias(&p) {
                        continue;
                    }
                    let idx = match hosts.iter().position(|h| h.alias == p) {
                        Some(i) => i,
                        None if hosts.len() < MAX_HOSTS => {
                            hosts.push(SshHost { alias: p, ..SshHost::default() });
                            hosts.len() - 1
                        }
                        None => continue,
                    };
                    current.push(idx);
                }
            }
            "match" => current.clear(),
            "hostname" | "user" | "port" | "identityfile" if !current.is_empty() => {
                let Some(val) = split_args(rest).into_iter().next() else { continue };
                for &i in &current {
                    let h = &mut hosts[i];
                    match kw.as_str() {
                        "hostname" => {
                            if h.hostname.is_none() {
                                h.hostname = Some(val.clone());
                            }
                        }
                        "user" => {
                            if h.user.is_none() {
                                h.user = Some(val.clone());
                            }
                        }
                        "port" => {
                            if h.port.is_none() {
                                h.port = val.parse::<u16>().ok().filter(|p| *p != 0);
                                if h.port.is_none() {
                                    continue;
                                }
                            }
                        }
                        _ => {
                            if h.identity_file.is_none() && !val.eq_ignore_ascii_case("none") {
                                h.identity_file = Some(val.clone());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    hosts
}

/// Simple `*` / `?` glob on one path component.
fn glob_match(pat: &[u8], s: &[u8]) -> bool {
    let (mut p, mut i) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while i < s.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == s[i]) {
            p += 1;
            i += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = p;
            mark = i;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

fn read_capped_file(path: &Path) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut buf = Vec::new();
    f.take(MAX_CONFIG_BYTES).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Resolve one `Include` argument to files (sorted, globbed in the last component only).
fn resolve_include(arg: &str, base_dir: &Path) -> Vec<PathBuf> {
    let expanded = if let Some(rest) = arg.strip_prefix("~/") {
        match std::env::var_os("HOME") {
            Some(h) => PathBuf::from(h).join(rest),
            None => return Vec::new(),
        }
    } else {
        PathBuf::from(arg)
    };
    let full = if expanded.is_absolute() { expanded } else { base_dir.join(expanded) };
    let Some(name) = full.file_name().and_then(|n| n.to_str()).map(str::to_string) else { return Vec::new() };
    let dir = full.parent().map(Path::to_path_buf).unwrap_or_default();
    if dir.to_string_lossy().contains(['*', '?']) {
        return Vec::new(); // only the file name may be a glob
    }
    if !name.contains(['*', '?']) {
        return vec![full];
    }
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .take(MAX_INCLUDE_ENTRIES * 16)
        .filter(|e| e.file_name().to_str().is_some_and(|n| glob_match(name.as_bytes(), n.as_bytes())))
        .map(|e| e.path())
        .collect();
    v.sort();
    v.truncate(MAX_INCLUDE_ENTRIES);
    v
}

/// Read `path` (normally `~/.ssh/config`) and the files it `Include`s (one level deep, at most 8
/// files in total, 256 KiB each; relative includes resolve against the config's directory).
/// Included text is spliced in at the `Include` line so "first value wins" keeps OpenSSH order.
/// A missing/unreadable file yields an empty list.
pub fn ssh_config_hosts(path: &Path) -> Vec<SshHost> {
    let Some(main) = read_capped_file(path) else { return Vec::new() };
    let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut files_left = MAX_CONFIG_FILES - 1;
    let mut spliced = String::with_capacity(main.len());
    for line in main.lines().take(MAX_CONFIG_LINES) {
        match split_kw(line) {
            Some((kw, rest)) if kw == "include" => {
                for arg in split_args(rest) {
                    for f in resolve_include(&arg, &base) {
                        if files_left == 0 {
                            break;
                        }
                        let Some(mut text) = read_capped_file(&f) else { continue };
                        files_left -= 1;
                        // one level only: drop nested Include lines
                        text = text
                            .lines()
                            .filter(|l| !matches!(split_kw(l), Some((k, _)) if k == "include"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        spliced.push_str(&text);
                        spliced.push('\n');
                    }
                }
            }
            _ => {
                spliced.push_str(line);
                spliced.push('\n');
            }
        }
    }
    parse_ssh_config(&spliced)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(online: bool, with_ip: bool) -> String {
        format!(
            r#"{{"Version":"1.2","BackendState":"Running","Unknown":{{"x":1}},
"Self":{{"HostName":"me","DNSName":"me.tail1.ts.net.","OS":"linux","TailscaleIPs":["100.64.0.1","fd7a::1"],"Online":true}},
"Peer":{{
 "nodekey:a":{{"HostName":"Zed","DNSName":"zed.tail1.ts.net.","OS":"linux","TailscaleIPs":["100.64.0.2"],"Online":{online}}},
 "nodekey:b":{{"HostName":"alpha","DNSName":"alpha.tail1.ts.net.","OS":"macOS","TailscaleIPs":{ips},"Online":true,"Extra":[1,2]}},
 "nodekey:c":{{"HostName":"noip","DNSName":"noip.tail1.ts.net.","TailscaleIPs":[],"Online":true}}
}}}}"#,
            ips = if with_ip { r#"["100.64.0.3"]"# } else { "[]" }
        )
    }

    #[test]
    fn parses_self_online_offline_and_sorts() {
        let p = parse_tailscale_status(&fixture(false, true)).unwrap();
        let names: Vec<_> = p.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["me", "alpha", "Zed"]);
        assert!(p[0].is_self && p[0].online);
        assert_eq!(p[0].dns_name, "me.tail1.ts.net");
        assert!(!p[2].online && !p[2].is_self);
        assert_eq!(p[1].os, "macOS");
        assert_eq!(p[1].address(), "alpha.tail1.ts.net");
    }

    #[test]
    fn skips_peers_without_ip() {
        let p = parse_tailscale_status(&fixture(true, false)).unwrap();
        assert_eq!(p.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["me", "Zed"]);
    }

    #[test]
    fn no_peer_map_and_empty() {
        let p = parse_tailscale_status(r#"{"BackendState":"Running","Self":{"HostName":"solo","TailscaleIPs":["100.1.1.1"]}}"#).unwrap();
        assert_eq!(p.len(), 1);
        assert!(p[0].is_self && p[0].online);
        assert_eq!(p[0].address(), "100.1.1.1");
        assert!(parse_tailscale_status("{}").unwrap().is_empty());
        assert!(parse_tailscale_status(r#"{"Peer":null,"Self":null}"#).unwrap().is_empty());
    }

    #[test]
    fn hostname_falls_back_to_dns_label_then_ip() {
        let p = parse_tailscale_status(r#"{"Peer":{"a":{"DNSName":"box.t.ts.net.","TailscaleIPs":["100.2.2.2"]},"b":{"TailscaleIPs":["100.3.3.3"]}}}"#).unwrap();
        let mut n: Vec<_> = p.iter().map(|x| x.name.clone()).collect();
        n.sort();
        assert_eq!(n, ["100.3.3.3", "box"]);
        assert!(p.iter().all(|x| !x.online)); // Online missing on a peer => not online
    }

    #[test]
    fn logged_out_and_stopped_give_hints() {
        for (state, needle) in [("NeedsLogin", "not logged in"), ("Stopped", "stopped"), ("Starting", "not running")] {
            let e = parse_tailscale_status(&format!(r#"{{"BackendState":"{state}"}}"#)).unwrap_err();
            assert!(e.to_string().contains(needle), "{state}: {e}");
        }
    }

    #[test]
    fn malformed_is_error_not_panic() {
        for bad in ["", "not json", "[1,2]", "null", "{\"Peer\":"] {
            assert!(parse_tailscale_status(bad).is_err(), "{bad:?}");
        }
        // wrong types inside are tolerated
        let p = parse_tailscale_status(r#"{"Peer":{"a":1,"b":{"TailscaleIPs":"x"},"c":{"TailscaleIPs":[1,"100.9.9.9"],"Online":"yes"}}}"#).unwrap();
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn peers_capped_at_200() {
        let mut s = String::from(r#"{"Peer":{"#);
        for i in 0..500 {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(r#""k{i}":{{"HostName":"h{i}","TailscaleIPs":["100.0.{}.{}"],"Online":true}}"#, i / 256, i % 256));
        }
        s.push_str("}}");
        assert_eq!(parse_tailscale_status(&s).unwrap().len(), MAX_PEERS);
    }

    #[test]
    fn ssh_config_basic() {
        let cfg = "\u{feff}# comment\r\nHost dev build\r\n  HostName 10.0.0.5\r\n  User alice\r\n  Port 2222\r\n  IdentityFile ~/.ssh/id_a\r\n  IdentityFile ~/.ssh/id_b\r\n\r\nHost *\r\n  User nobody\r\n  HostName wildcard.example\r\n\r\nHost !bad ok? web*  prod\r\n  hostname=prod.example.com\r\n  USER \"deploy\"\r\n\r\nHost dev\r\n  User later-loses\r\n  Port 22\r\n\r\nMatch host foo\r\n  User matched\r\nHost last\r\n";
        let h = parse_ssh_config(cfg);
        let by = |a: &str| h.iter().find(|x| x.alias == a).cloned().unwrap();
        assert_eq!(h.iter().map(|x| x.alias.as_str()).collect::<Vec<_>>(), ["dev", "build", "prod", "last"]);
        let dev = by("dev");
        assert_eq!((dev.hostname.as_deref(), dev.user.as_deref(), dev.port, dev.identity_file.as_deref()), (Some("10.0.0.5"), Some("alice"), Some(2222), Some("~/.ssh/id_a")));
        assert_eq!(by("build").user.as_deref(), Some("alice"));
        let prod = by("prod");
        assert_eq!((prod.hostname.as_deref(), prod.user.as_deref()), (Some("prod.example.com"), Some("deploy")));
        assert_eq!(by("last"), SshHost { alias: "last".into(), ..Default::default() });
    }

    #[test]
    fn ssh_config_quotes_comments_and_bad_port() {
        let h = parse_ssh_config("Host \"my box\" b # trailing\n  HostName \"h o\" # c\n  Port 99999\n  Port 22\n  Port 23\nHost c\n  Port abc\n  IdentityFile none\n");
        assert_eq!(h[0].alias, "my box");
        assert_eq!(h[0].hostname.as_deref(), Some("h o"));
        assert_eq!(h[0].port, Some(22)); // invalid value does not claim the slot
        assert_eq!(h[1].alias, "b");
        assert_eq!(h[2].port, None);
        assert_eq!(h[2].identity_file, None);
    }

    #[test]
    fn ssh_config_caps_hosts() {
        let mut s = String::new();
        for i in 0..5000 {
            s.push_str(&format!("Host h{i}\n"));
        }
        assert_eq!(parse_ssh_config(&s).len(), MAX_HOSTS);
    }

    #[test]
    fn glob() {
        assert!(glob_match(b"*.conf", b"a.conf"));
        assert!(glob_match(b"a?c*", b"abcdef"));
        assert!(!glob_match(b"*.conf", b"a.conf.bak"));
        assert!(glob_match(b"*", b""));
    }

    #[test]
    fn fuzz_never_panics() {
        let mut x: u64 = 0x9e3779b97f4a7c15;
        let words = ["Host", "host", "Match", "Include", "HostName", "Port", "=", "\"", "#", "*", "!", " ", "\n", "\r\n", "\u{feff}", "é", "\t"];
        for round in 0..300 {
            let mut s = String::new();
            for _ in 0..(round % 200 + 1) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                if x % 3 == 0 {
                    s.push_str(words[(x >> 8) as usize % words.len()]);
                } else {
                    s.push(char::from_u32((x >> 16) as u32 % 0x250).unwrap_or('?'));
                }
            }
            let h = parse_ssh_config(&s);
            assert!(h.len() <= MAX_HOSTS);
            let _ = parse_tailscale_status(&s);
        }
    }
}
