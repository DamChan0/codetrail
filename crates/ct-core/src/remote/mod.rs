//! Remote repositories over `ssh` (read-only): `git` runs on the remote host, nothing else is
//! needed there. Every ssh child goes through the same limiter/timeout/kill/output-cap machinery as
//! local git (see `crate::git`).

pub mod discover;
pub(crate) mod cache;
pub(crate) mod exec;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::git::{run_remote_script, RunOpts};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    #[default]
    Ssh,
    /// Adds `-o ProxyCommand="tailscale nc %h %p"`.
    TailscaleNc,
    /// Plain ssh to a tailnet name/IP; Tailscale SSH authenticates by tailnet identity, so no `-i`
    /// and no ProxyCommand.
    TailscaleSsh,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSpec {
    /// Unique label shown in the UI.
    pub name: String,
    /// ssh alias, hostname, tailnet MagicDNS name or IP.
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    /// Passed with `-i` and `IdentitiesOnly=yes`.
    pub identity_file: Option<String>,
    pub transport: Transport,
    /// `false` → `StrictHostKeyChecking=yes` (default); `true` → `accept-new` (never `no`).
    pub trust_new_host: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteErrorKind {
    HostKeyUnknown,
    HostKeyChanged,
    AuthFailed,
    Unreachable,
    Timeout,
    NoGit,
    NotARepo,
    Other,
}

/// Actionable one-liner for an [`Error::Remote`].
pub fn describe(kind: &RemoteErrorKind, detail: &str) -> String {
    let hint = match kind {
        RemoteErrorKind::HostKeyUnknown => "host key not trusted yet: enable 'Trust new host key' or run `ssh <host>` once",
        RemoteErrorKind::HostKeyChanged => "host key CHANGED (possible man-in-the-middle): verify the host, then `ssh-keygen -R <host>`",
        RemoteErrorKind::AuthFailed => "authentication failed. Use Tailscale (no keys): set Transport to Tailscale SSH and run `tailscale set --ssh` on that machine, or add your key (authorized_keys, ssh-agent or an identity file); password prompts are disabled",
        RemoteErrorKind::Unreachable => "host unreachable: check the name/address, that sshd is running and the network/Tailscale connection",
        RemoteErrorKind::Timeout => "connection timed out",
        RemoteErrorKind::NoGit => "git was not found on the remote (non-interactive PATH)",
        RemoteErrorKind::NotARepo => "not a git repository (or no such directory) on the remote",
        RemoteErrorKind::Other => "",
    };
    match (hint.is_empty(), detail.is_empty()) {
        (true, true) => "remote command failed".to_string(),
        (true, false) => format!("remote: {detail}"),
        (false, true) => hint.to_string(),
        (false, false) => format!("{hint} [{detail}]"),
    }
}

fn plain_token(s: &str, extra: &str) -> bool {
    !s.is_empty() && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

impl RemoteSpec {
    /// Rejects anything that could be read as an ssh option or be interpreted by a shell. The host
    /// is also substituted into `ProxyCommand` (`%h`), which ssh runs through a shell, so it is
    /// limited to `[A-Za-z0-9._:-]` (ssh aliases, DNS names, IPv4/IPv6 literals).
    pub fn validate(&self) -> Result<()> {
        let bad = |m: String| Err(Error::Invalid(m));
        if self.name.trim().is_empty() || self.name.chars().any(char::is_control) {
            return bad("remote name must be non-empty and contain no control characters".into());
        }
        if self.host.starts_with('-') {
            return bad(format!("invalid host {:?}: must not start with '-'", self.host));
        }
        if !plain_token(&self.host, "._:-") {
            return bad(format!("invalid host {:?}: only letters, digits, '.', '_', '-' and ':' are allowed", self.host));
        }
        if let Some(u) = &self.user {
            if !plain_token(u, "._-") {
                return bad(format!("invalid user {u:?}: only letters, digits, '.', '_' and '-' are allowed"));
            }
        }
        if self.port == Some(0) {
            return bad("port must be 1-65535".into());
        }
        if let Some(f) = &self.identity_file {
            if f.trim().is_empty() || f.chars().any(char::is_control) || f.starts_with('-') {
                return bad(format!("invalid identity file {f:?}"));
            }
        }
        Ok(())
    }

    /// `user@host`, or just `host`.
    pub fn target(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteInfo {
    pub hostname: String,
    pub home: String,
    pub git_version: String,
    pub latency_ms: u64,
}

fn timeout_err(spec: &RemoteSpec, t: Duration) -> Error {
    Error::Remote { kind: RemoteErrorKind::Timeout, detail: format!("{} did not answer within {t:?}", spec.target()) }
}

fn map_timeout(e: Error, spec: &RemoteSpec) -> Error {
    match e {
        Error::Timeout(t) => timeout_err(spec, t),
        o => o,
    }
}

/// One ssh round trip: remote hostname, `$HOME` and git version.
pub fn test_connection(spec: &RemoteSpec, timeout: Duration) -> Result<RemoteInfo> {
    spec.validate()?;
    // ephemeral: a connection test must not leave a ControlMaster behind (cap on live masters)
    let o = RunOpts { timeout, max_output: 64 * 1024, remote: Some(Arc::new(spec.clone())), ephemeral_master: true, ..RunOpts::default() };
    // `printf '%s\n'` (not the bare `%s`) so $HOME and the git version are on separate lines
    let script = "(hostname 2>/dev/null || uname -n); printf '%s\\n' \"$HOME\"; git --version";
    let t0 = Instant::now();
    let (out, _) = run_remote_script(spec, script, "connection test", None, &o, &[]).map_err(|e| map_timeout(e, spec))?;
    let latency_ms = t0.elapsed().as_millis() as u64;
    let text = String::from_utf8_lossy(&out);
    // a login banner/motd must not confuse us: use the last three non-empty lines
    let lines: Vec<&str> = text.lines().map(str::trim_end).filter(|l| !l.is_empty()).collect();
    let [.., host, home, git] = lines.as_slice() else {
        return Err(Error::Remote { kind: RemoteErrorKind::Other, detail: format!("unexpected reply from {}", spec.target()) });
    };
    let Some(v) = git.strip_prefix("git version ") else {
        return Err(Error::Remote { kind: RemoteErrorKind::NoGit, detail: format!("unexpected `git --version` output: {git}") });
    };
    Ok(RemoteInfo { hostname: host.to_string(), home: home.to_string(), git_version: v.trim().to_string(), latency_ms })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_git_repo: bool,
}

/// Max entries returned by [`list_dir`].
pub const MAX_DIR_ENTRIES: usize = 500;

/// Sub-directories of `path` (hidden ones skipped, at most [`MAX_DIR_ENTRIES`]), each flagged when
/// it contains `.git`. A leading `~` expands to the remote `$HOME`. One ssh call.
pub fn list_dir(spec: &RemoteSpec, path: &str, cancel: Option<Arc<AtomicBool>>) -> Result<Vec<DirEntry>> {
    spec.validate()?;
    exec::check_no_nul(path)?;
    if path.trim().is_empty() {
        return Err(Error::Invalid("empty remote path".into()));
    }
    let o = RunOpts { timeout: Duration::from_secs(30), cancel, max_output: 256 * 1024, remote: Some(Arc::new(spec.clone())), ..RunOpts::default() };
    // `*/` (no dotglob) = visible directories only; symlinks to directories are followed
    let script = format!(
        "cd {} || exit 97; n=0; for d in */; do [ -d \"$d\" ] || continue; if [ -e \"${{d}}.git\" ]; then g=1; else g=0; fi; printf '%s\\t%s\\n' \"$g\" \"$d\"; n=$((n+1)); [ \"$n\" -ge {} ] && break; done; exit 0",
        exec::quote_path(path),
        MAX_DIR_ENTRIES
    );
    let (out, _) = run_remote_script(spec, &script, "list directory", None, &o, &[]).map_err(|e| match e {
        Error::Git { code: 97, stderr, .. } => Error::Remote { kind: RemoteErrorKind::NotARepo, detail: format!("cannot open directory {path}: {}", exec::short_detail(&stderr)) },
        o => map_timeout(o, spec),
    })?;
    let mut v: Vec<DirEntry> = String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| {
            let (g, name) = l.split_once('\t')?;
            let name = name.strip_suffix('/').unwrap_or(name);
            (!name.is_empty()).then(|| DirEntry { name: name.to_string(), is_git_repo: g == "1" })
        })
        .take(MAX_DIR_ENTRIES)
        .collect();
    v.sort_by_key(|e| e.name.to_lowercase());
    Ok(v)
}

/// `ssh -O exit` for this host's ControlMaster. Never blocks more than 2 s and never fails.
pub fn close_connection(spec: &RemoteSpec) {
    exec::close(spec);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RemoteSpec {
        RemoteSpec { name: "n".into(), host: "box.tail1234.ts.net".into(), user: Some("bob".into()), port: Some(22), identity_file: None, transport: Transport::Ssh, trust_new_host: false }
    }

    #[test]
    fn validate_accepts_normal() {
        assert!(spec().validate().is_ok());
        for h in ["localhost", "10.0.0.1", "fe80::1", "my_alias-2", "100.64.0.9"] {
            let mut s = spec();
            s.host = h.into();
            assert!(s.validate().is_ok(), "{h}");
        }
        let mut s = spec();
        s.identity_file = Some("/home/x/my key".into());
        assert!(s.validate().is_ok());
    }

    #[test]
    fn validate_rejects_dangerous() {
        for h in ["", "-oProxyCommand=x", "-h", "a b", "a\tb", "a\nb", "a\0b", "host;id", "$(id)", "a`b`", "h'x", "a/b", "a%h", "a|b", "ünï"] {
            let mut s = spec();
            s.host = h.into();
            assert!(s.validate().is_err(), "host {h:?} must be rejected");
        }
        for u in ["", "-l", "a b", "a@b", "a;b", "x\ny"] {
            let mut s = spec();
            s.user = Some(u.into());
            assert!(s.validate().is_err(), "user {u:?} must be rejected");
        }
        let mut s = spec();
        s.port = Some(0);
        assert!(s.validate().is_err());
        let mut s = spec();
        s.name = "  ".into();
        assert!(s.validate().is_err());
        let mut s = spec();
        s.identity_file = Some("a\nb".into());
        assert!(s.validate().is_err());
        s.identity_file = Some("-oFoo".into());
        assert!(s.validate().is_err());
    }

    #[test]
    fn target_forms() {
        assert_eq!(spec().target(), "bob@box.tail1234.ts.net");
        let mut s = spec();
        s.user = None;
        assert_eq!(s.target(), "box.tail1234.ts.net");
    }

    #[test]
    fn describe_is_actionable() {
        let m = describe(&RemoteErrorKind::HostKeyUnknown, "Host key verification failed.");
        assert!(m.contains("Trust new host key") && m.contains("ssh <host>") && m.contains("Host key verification failed."), "{m}");
        assert_eq!(describe(&RemoteErrorKind::Other, ""), "remote command failed");
        assert_eq!(describe(&RemoteErrorKind::Other, "boom"), "remote: boom");
        assert_eq!(describe(&RemoteErrorKind::Timeout, ""), "connection timed out");
    }

    #[test]
    fn serde_round_trip() {
        let s = spec();
        let j = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<RemoteSpec>(&j).unwrap(), s);
        assert!(serde_json::to_string(&Transport::TailscaleNc).unwrap().contains("tailscale_nc"));
        assert_eq!(serde_json::to_string(&Transport::TailscaleSsh).unwrap(), "\"tailscale_ssh\"");
        let mut t = spec();
        t.transport = Transport::TailscaleSsh;
        assert_eq!(serde_json::from_str::<RemoteSpec>(&serde_json::to_string(&t).unwrap()).unwrap(), t);
    }

    #[test]
    fn old_configs_without_new_variant_still_load() {
        let j = r#"{"name":"n","host":"h","user":null,"port":null,"identity_file":null,"transport":"tailscale_nc","trust_new_host":false}"#;
        assert_eq!(serde_json::from_str::<RemoteSpec>(j).unwrap().transport, Transport::TailscaleNc);
        let j = j.replace("tailscale_nc", "ssh");
        assert_eq!(serde_json::from_str::<RemoteSpec>(&j).unwrap().transport, Transport::Ssh);
    }
}
