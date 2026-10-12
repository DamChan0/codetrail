//! Thin adapter over `ct_core::remote`: everything the GUI asks of a remote host goes through the
//! [`RemoteBackend`] trait so the picker and settings state machines can be tested without ssh.
//! Also holds the pure helpers (form validation, connection-error detection, poll backoff).

use ct_core::remote::discover::{SshHost, TailnetPeer};
use ct_core::remote::{DirEntry, RemoteInfo, RemoteSpec, Transport};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

pub trait RemoteBackend: Send + Sync {
    fn test(&self, spec: &RemoteSpec, timeout: Duration) -> Result<RemoteInfo, String>;
    fn list_dir(&self, spec: &RemoteSpec, path: &str, cancel: Option<Arc<AtomicBool>>) -> Result<Vec<DirEntry>, String>;
    fn tailscale_peers(&self) -> Result<Vec<TailnetPeer>, String>;
    fn ssh_hosts(&self) -> Vec<SshHost>;
    fn close(&self, spec: &RemoteSpec);
}

pub struct RealRemote;

impl RemoteBackend for RealRemote {
    fn test(&self, spec: &RemoteSpec, timeout: Duration) -> Result<RemoteInfo, String> {
        ct_core::remote::test_connection(spec, timeout).map_err(|e| e.to_string())
    }
    fn list_dir(&self, spec: &RemoteSpec, path: &str, cancel: Option<Arc<AtomicBool>>) -> Result<Vec<DirEntry>, String> {
        ct_core::remote::list_dir(spec, path, cancel).map_err(|e| e.to_string())
    }
    fn tailscale_peers(&self) -> Result<Vec<TailnetPeer>, String> {
        ct_core::remote::discover::tailscale_peers(Duration::from_secs(5)).map_err(|e| e.to_string())
    }
    fn ssh_hosts(&self) -> Vec<SshHost> {
        let home = crate::projects::home_dir();
        ct_core::remote::discover::ssh_config_hosts(&home.join(".ssh/config"))
    }
    fn close(&self, spec: &RemoteSpec) {
        ct_core::remote::close_connection(spec);
    }
}

/// Text fields of the add/edit form; [`RemoteForm::build`] is the only way to a [`RemoteSpec`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteForm {
    pub name: String,
    pub host: String,
    pub user: String,
    pub port: String,
    pub identity_file: String,
    pub transport: Transport,
    pub trust_new_host: bool,
}

impl RemoteForm {
    pub fn from_spec(s: &RemoteSpec) -> Self {
        RemoteForm { name: s.name.clone(), host: s.host.clone(), user: s.user.clone().unwrap_or_default(), port: s.port.map(|p| p.to_string()).unwrap_or_default(), identity_file: s.identity_file.clone().unwrap_or_default(), transport: s.transport, trust_new_host: s.trust_new_host }
    }

    /// `others`: names already taken by *other* remotes (the one being edited is excluded).
    pub fn build(&self, others: &[&str]) -> Result<RemoteSpec, String> {
        let opt = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err("Give the remote a name.".into());
        }
        if others.contains(&name.as_str()) {
            return Err(format!("A remote named \"{name}\" already exists."));
        }
        if self.host.trim().is_empty() {
            return Err("Enter a host name, address or ssh alias.".into());
        }
        let port = match self.port.trim() {
            "" => None,
            p => Some(p.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(|| "Port must be a number from 1 to 65535.".to_string())?),
        };
        let spec = RemoteSpec { name, host: self.host.trim().to_string(), user: opt(&self.user), port, identity_file: opt(&self.identity_file).filter(|_| self.transport != Transport::TailscaleSsh), transport: self.transport, trust_new_host: self.trust_new_host };
        spec.validate().map_err(|e| e.to_string())?;
        Ok(spec)
    }
}

/// Remotes shown by the `remote-*` smoke scenes (they run against the fake ssh).
pub fn smoke_remotes() -> Vec<RemoteSpec> {
    let r = |name: &str, host: &str, user: &str| RemoteSpec { name: name.into(), host: host.into(), user: Some(user.into()), port: None, identity_file: None, transport: Transport::Ssh, trust_new_host: false };
    vec![RemoteSpec { transport: Transport::TailscaleSsh, ..r("dev-box", "fake-host", "dev") }, RemoteSpec { transport: Transport::TailscaleNc, ..r("build-server", "build.tail1234.ts.net", "ci") }, r("home-nas", "192.168.1.20", "me")]
}

/// Header chip text: `user@host:~/path`.
pub fn chip_label(spec: &RemoteSpec, path: &str) -> String {
    format!("{}:{}", spec.target(), path)
}

/// True when an error string means the link itself is down (as opposed to a git/path failure), so
/// the UI offers "Retry" instead of showing a per-request error.
pub fn is_connection_error(msg: &str) -> bool {
    ["host unreachable", "connection timed out", "authentication failed", "host key"].iter().any(|k| msg.contains(k))
}

/// Poll interval of the Current tab on a remote: 10 s, doubled per consecutive failure, at most 5 min.
pub fn remote_poll_interval(fails: u32) -> Duration {
    Duration::from_secs(10).saturating_mul(1u32 << fails.min(5)).min(Duration::from_secs(300))
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;

    /// Scripted backend: directory tree and call log, no ssh.
    #[derive(Default)]
    pub struct ScriptedRemote {
        pub dirs: Mutex<HashMap<String, Vec<(String, bool)>>>,
        pub calls: Mutex<Vec<String>>,
        pub fail: Mutex<Option<String>>,
    }

    impl ScriptedRemote {
        pub fn with(dirs: &[(&str, &[(&str, bool)])]) -> Arc<Self> {
            let s = ScriptedRemote::default();
            for (d, es) in dirs {
                s.dirs.lock().insert((*d).to_string(), es.iter().map(|(n, g)| ((*n).to_string(), *g)).collect());
            }
            Arc::new(s)
        }
    }

    impl RemoteBackend for ScriptedRemote {
        fn test(&self, spec: &RemoteSpec, _t: Duration) -> Result<RemoteInfo, String> {
            self.calls.lock().push(format!("test {}", spec.name));
            match self.fail.lock().clone() {
                Some(e) => Err(e),
                None => Ok(RemoteInfo { hostname: "scripted".into(), home: "/home/me".into(), git_version: "git version 2.43".into(), latency_ms: 12 }),
            }
        }
        fn list_dir(&self, _spec: &RemoteSpec, path: &str, _c: Option<Arc<AtomicBool>>) -> Result<Vec<DirEntry>, String> {
            self.calls.lock().push(format!("ls {path}"));
            if let Some(e) = self.fail.lock().clone() {
                return Err(e);
            }
            self.dirs.lock().get(path).map(|v| v.iter().map(|(n, g)| DirEntry { name: n.clone(), is_git_repo: *g }).collect()).ok_or_else(|| format!("no such directory {path}"))
        }
        fn tailscale_peers(&self) -> Result<Vec<TailnetPeer>, String> {
            Err("tailscale is not installed".into())
        }
        fn ssh_hosts(&self) -> Vec<SshHost> {
            Vec::new()
        }
        fn close(&self, spec: &RemoteSpec) {
            self.calls.lock().push(format!("close {}", spec.name));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> RemoteForm {
        RemoteForm { name: "dev".into(), host: "dev.tail1234.ts.net".into(), ..Default::default() }
    }

    #[test]
    fn form_builds_a_spec_and_round_trips_through_from_spec() {
        let mut f = form();
        f.user = " me ".into();
        f.port = "2222".into();
        f.transport = Transport::TailscaleNc;
        f.trust_new_host = true;
        let s = f.build(&[]).unwrap();
        assert_eq!((s.user.as_deref(), s.port, s.transport, s.trust_new_host), (Some("me"), Some(2222), Transport::TailscaleNc, true));
        assert_eq!(RemoteForm::from_spec(&s).build(&[]).unwrap(), s);
        assert_eq!(chip_label(&s, "~/code"), "me@dev.tail1234.ts.net:~/code");
    }

    #[test]
    fn tailscale_ssh_form_drops_a_stale_identity_file() {
        let f = RemoteForm { transport: Transport::TailscaleSsh, identity_file: "/k/id".into(), ..form() };
        let s = f.build(&[]).unwrap();
        assert_eq!((s.transport, s.identity_file), (Transport::TailscaleSsh, None));
        let k = RemoteForm { transport: Transport::Ssh, identity_file: "/k/id".into(), ..form() }.build(&[]).unwrap();
        assert_eq!(k.identity_file.as_deref(), Some("/k/id"));
    }

    #[test]
    fn form_rejects_bad_input_with_a_sentence() {
        let bad = |f: RemoteForm, others: &[&str]| f.build(others).unwrap_err();
        assert!(bad(RemoteForm { name: " ".into(), ..form() }, &[]).contains("name"));
        assert!(bad(form(), &["dev"]).contains("already exists"));
        assert!(bad(RemoteForm { host: "".into(), ..form() }, &[]).contains("host"));
        assert!(bad(RemoteForm { port: "0".into(), ..form() }, &[]).contains("Port"));
        assert!(bad(RemoteForm { port: "70000".into(), ..form() }, &[]).contains("Port"));
        assert!(bad(RemoteForm { port: "ssh".into(), ..form() }, &[]).contains("Port"));
        assert!(bad(RemoteForm { host: "-oProxyCommand=x".into(), ..form() }, &[]).contains("host"));
        assert!(bad(RemoteForm { host: "a b".into(), ..form() }, &[]).contains("host"));
        assert!(bad(RemoteForm { user: "a b".into(), ..form() }, &[]).contains("user"));
        assert!(RemoteForm { identity_file: "".into(), ..form() }.build(&[]).unwrap().identity_file.is_none(), "an empty identity file means none");
    }

    #[test]
    fn connection_errors_are_told_apart_from_git_errors() {
        for k in [ct_core::RemoteErrorKind::Unreachable, ct_core::RemoteErrorKind::Timeout, ct_core::RemoteErrorKind::AuthFailed, ct_core::RemoteErrorKind::HostKeyChanged, ct_core::RemoteErrorKind::HostKeyUnknown] {
            assert!(is_connection_error(&ct_core::remote::describe(&k, "x")), "{k:?}");
        }
        for k in [ct_core::RemoteErrorKind::NoGit, ct_core::RemoteErrorKind::NotARepo, ct_core::RemoteErrorKind::Other] {
            assert!(!is_connection_error(&ct_core::remote::describe(&k, "x")), "{k:?}");
        }
        assert!(!is_connection_error("fatal: bad revision"));
    }

    #[test]
    fn poll_interval_backs_off_exponentially_and_is_capped() {
        let v: Vec<u64> = (0..8).map(|n| remote_poll_interval(n).as_secs()).collect();
        assert_eq!(v, [10, 20, 40, 80, 160, 300, 300, 300]);
    }
}
