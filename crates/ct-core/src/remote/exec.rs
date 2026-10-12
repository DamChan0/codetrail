//! ssh command construction, shell quoting and stderr classification. Pure functions except
//! [`control_dir`] / [`ssh_command`] (which touch the filesystem) and [`close`].

use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::{RemoteErrorKind, RemoteSpec, Transport};
use crate::{Error, Result};

/// Env var that overrides the ssh binary (tests point it at a fake that runs the command locally).
pub const SSH_BIN_ENV: &str = "CT_SSH_BIN";

pub fn ssh_bin() -> String {
    std::env::var(SSH_BIN_ENV).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "ssh".to_string())
}

/// POSIX single-quote escaping: the result is one shell word that evaluates to exactly `s`.
/// Newlines, `$()`, backticks and quotes are inert inside single quotes. Callers must reject NUL.
pub fn shquote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('\'');
    for c in s.chars() {
        if c == '\'' {
            o.push_str("'\\''");
        } else {
            o.push(c);
        }
    }
    o.push('\'');
    o
}

/// Quote a remote path; a leading `~` / `~/` expands to the remote `$HOME`.
pub fn quote_path(p: &str) -> String {
    if p == "~" {
        "\"$HOME\"".to_string()
    } else if let Some(rest) = p.strip_prefix("~/") {
        if rest.is_empty() {
            "\"$HOME\"".to_string()
        } else {
            format!("\"$HOME\"/{}", shquote(rest))
        }
    } else {
        shquote(p)
    }
}

pub fn check_no_nul(s: &str) -> Result<()> {
    if s.contains('\0') {
        return Err(Error::Invalid("argument contains a NUL byte".into()));
    }
    Ok(())
}

/// Script run on the remote for a git invocation (before the `sh -c` wrapper).
pub fn git_script(root: &str, args: &[&str]) -> Result<String> {
    check_no_nul(root)?;
    if root.is_empty() {
        return Err(Error::Invalid("empty remote path".into()));
    }
    let mut s = format!(
        "cd {} && exec env LC_ALL=C GIT_OPTIONAL_LOCKS=0 GIT_TERMINAL_PROMPT=0 GIT_PAGER=cat git -c core.quotepath=false -c color.ui=never -c core.pager=cat",
        quote_path(root)
    );
    for a in args {
        check_no_nul(a)?;
        s.push(' ');
        s.push_str(&shquote(a));
    }
    Ok(s)
}

/// The single command string handed to ssh: always run through `sh -c` so the user's login shell
/// (fish, csh, ...) never interprets our script.
pub fn wrap_sh(script: &str) -> String {
    format!("sh -c {}", shquote(script))
}

/// Application-level identity of a connection: every field that changes *how* we connect or
/// authenticate (not `name`, which is a label). ssh's own `%C` hash omits identity file, transport
/// and host-key policy, so two different policies would share one ControlMaster.
pub fn policy_hash(spec: &RemoteSpec) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(policy_string(spec).as_bytes()))
}

/// Canonical, unambiguous (NUL-separated) form of the connection policy; also the namespace of the
/// records cache.
pub fn policy_string(spec: &RemoteSpec) -> String {
    format!(
        "{}\0{}\0{}\0{}\0{}\0{}",
        spec.host,
        spec.user.as_deref().unwrap_or(""),
        spec.port.map_or(String::new(), |p| p.to_string()),
        spec.identity_file.as_deref().unwrap_or(""),
        match spec.transport {
            Transport::Ssh => "ssh",
            Transport::TailscaleNc => "tailscale_nc",
            Transport::TailscaleSsh => "tailscale_ssh",
        },
        spec.trust_new_host
    )
}

/// `ControlPath=<dir>/<16 hex policy hash>` (no `%C`; `%` in the directory is doubled for ssh).
fn control_path_arg(dir: &Path, spec: &RemoteSpec) -> String {
    format!("ControlPath={}/{}", dir.to_string_lossy().replace('%', "%%"), policy_hash(spec))
}

/// Full argument vector for `ssh` (without the program name); the master persists 120 s.
#[cfg(test)]
pub fn ssh_args(spec: &RemoteSpec, control_dir: &Path, remote_cmd: &str) -> Vec<String> {
    ssh_args_with(spec, control_dir, remote_cmd, true)
}

/// [`ssh_args`]; `persist == false` → `ControlPersist=no` (the master exits with its last client).
pub fn ssh_args_with(spec: &RemoteSpec, control_dir: &Path, remote_cmd: &str, persist: bool) -> Vec<String> {
    let mut a: Vec<String> = vec!["-T".into()];
    let mut o = |s: String| {
        a.push("-o".into());
        a.push(s);
    };
    o("BatchMode=yes".into());
    o("ConnectTimeout=10".into());
    o("ServerAliveInterval=10".into());
    o("ServerAliveCountMax=3".into());
    o("ControlMaster=auto".into());
    o(if persist { "ControlPersist=120".into() } else { "ControlPersist=no".into() });
    o(control_path_arg(control_dir, spec));
    // never forward anything, whatever ~/.ssh/config says
    o("ForwardAgent=no".into());
    o("ForwardX11=no".into());
    o("ClearAllForwardings=yes".into());
    if let Some(p) = spec.port {
        a.push("-p".into());
        a.push(p.to_string());
    }
    // Tailscale SSH authenticates by tailnet identity: never offer a key
    if let Some(f) = spec.identity_file.as_ref().filter(|_| spec.transport != Transport::TailscaleSsh) {
        a.push("-i".into());
        a.push(f.clone());
        a.push("-o".into());
        a.push("IdentitiesOnly=yes".into());
    }
    if spec.transport == Transport::TailscaleNc {
        a.push("-o".into());
        a.push("ProxyCommand=tailscale nc %h %p".into());
    }
    a.push("-o".into());
    a.push(format!("StrictHostKeyChecking={}", if spec.trust_new_host { "accept-new" } else { "yes" }));
    a.push("--".into());
    a.push(spec.target());
    a.push(remote_cmd.to_string());
    a
}

/// Max bytes of the ControlPath directory: `<dir>/` + 16-hex policy hash + ssh's 17-byte temp
/// suffix must fit sockaddr_un.sun_path (108 incl. NUL).
const MAX_CTL_DIR: usize = 49;

/// Creates `p` (0700) atomically if missing and verifies it is a real directory (never a symlink)
/// owned by us; loosened permissions on our own directory are repaired.
pub(super) fn ensure_private_dir(p: &Path) -> std::io::Result<()> {
    if let Some(parent) = p.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(p) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let check = || -> std::io::Result<std::fs::Metadata> {
        let m = std::fs::symlink_metadata(p)?;
        // SAFETY: geteuid has no preconditions
        if m.file_type().is_symlink() || !m.is_dir() || m.uid() != unsafe { libc::geteuid() } {
            return Err(std::io::Error::other(format!("{} is not a real directory owned by you (symlink or foreign owner)", p.display())));
        }
        Ok(m)
    };
    if check()?.mode() & 0o077 != 0 {
        std::fs::set_permissions(p, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        if check()?.mode() & 0o077 != 0 {
            return Err(std::io::Error::other(format!("cannot restrict {} to mode 0700", p.display())));
        }
    }
    Ok(())
}

fn control_dir_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(x) = std::env::var_os("XDG_RUNTIME_DIR").filter(|x| !x.is_empty()) {
        v.push(PathBuf::from(x).join("codetrail-ssh"));
    }
    if let Some(h) = std::env::var_os("HOME").filter(|x| !x.is_empty()) {
        v.push(PathBuf::from(h).join(".cache/codetrail/ssh"));
    }
    // last resort; only used when ensure_private_dir proves it is a real directory of ours
    // SAFETY: getuid has no preconditions
    v.push(PathBuf::from(format!("/tmp/ct-ssh-{}", unsafe { libc::getuid() })));
    v
}

/// First candidate that is (or can be made into) a safe private directory.
fn pick_dir(cands: &[PathBuf]) -> Result<PathBuf> {
    let mut last = None;
    for c in cands {
        if c.as_os_str().len() > MAX_CTL_DIR {
            continue;
        }
        match ensure_private_dir(c) {
            Ok(()) => return Ok(c.clone()),
            Err(e) => last = Some(format!("{}: {e}", c.display())),
        }
    }
    Err(Error::Remote {
        kind: RemoteErrorKind::Other,
        detail: format!("no safe directory for ssh control sockets ({})", last.unwrap_or_else(|| "no usable path".into())),
    })
}

/// Directory for ControlMaster sockets: validated once, then cached for the process (so connect
/// and close always agree).
pub fn control_dir() -> Result<PathBuf> {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    if let Some(d) = DIR.get() {
        return Ok(d.clone());
    }
    let d = pick_dir(&control_dir_candidates())?;
    Ok(DIR.get_or_init(|| d).clone())
}

/// At most this many ControlMasters are kept alive by one process.
pub const MAX_MASTERS: usize = 8;

/// LRU list of the policies we have started a persistent master for (front = least recent).
struct Registry(Vec<(String, RemoteSpec)>);

impl Registry {
    /// Marks `spec` used; returns the least recently used spec to shut down when over the cap.
    fn touch(&mut self, spec: &RemoteSpec) -> Option<RemoteSpec> {
        let key = policy_hash(spec);
        if let Some(i) = self.0.iter().position(|(k, _)| *k == key) {
            let e = self.0.remove(i);
            self.0.push(e);
            return None;
        }
        let evicted = (self.0.len() >= MAX_MASTERS).then(|| self.0.remove(0).1);
        self.0.push((key, spec.clone()));
        evicted
    }

    fn forget(&mut self, spec: &RemoteSpec) {
        let key = policy_hash(spec);
        self.0.retain(|(k, _)| *k != key);
    }
}

static REGISTRY: parking_lot::Mutex<Registry> = parking_lot::Mutex::new(Registry(Vec::new()));

/// `ssh` for running `remote_cmd` on the remote (stdio is set up by the caller). `persist`: leave
/// the ControlMaster up for 120 s (counted against [`MAX_MASTERS`]; the LRU master is stopped).
pub fn ssh_command(spec: &RemoteSpec, remote_cmd: &str, persist: bool) -> Result<Command> {
    spec.validate()?;
    let dir = control_dir()?;
    if persist {
        let evicted = REGISTRY.lock().touch(spec);
        if let Some(old) = evicted {
            // `stop`: no new sessions, running ones finish; the master then exits
            control_op(&old, "stop");
        }
    }
    let mut c = Command::new(ssh_bin());
    c.args(ssh_args_with(spec, &dir, remote_cmd, persist));
    Ok(c)
}

/// `ssh -O exit` for the ControlMaster of `spec`; bounded to 2 s, never errors.
pub fn close(spec: &RemoteSpec) {
    REGISTRY.lock().forget(spec);
    control_op(spec, "exit");
}

/// `ssh -O <op>` against the validated, cached control directory; bounded to 2 s.
fn control_op(spec: &RemoteSpec, op: &str) {
    if spec.validate().is_err() {
        return;
    }
    let Ok(dir) = control_dir() else { return };
    let mut c = Command::new(ssh_bin());
    c.args(["-O", op, "-o"]).arg(control_path_arg(&dir, spec));
    if let Some(p) = spec.port {
        c.args(["-p", &p.to_string()]);
    }
    c.args(["--", &spec.target()]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
    let Ok(mut child) = c.spawn() else { return };
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if Instant::now() >= end => {
                // SAFETY: the child is our unreaped process-group leader
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Maps ssh/git/shell stderr to a typed kind; `None` when nothing recognisable is in it.
pub fn classify_stderr(s: &str) -> Option<RemoteErrorKind> {
    use RemoteErrorKind::*;
    let l = s.to_ascii_lowercase();
    let has = |p: &str| l.contains(p);
    if has("remote host identification has changed") || has("possible dns spoofing") || has("offending") && has("known_hosts") {
        Some(HostKeyChanged)
    } else if has("host key verification failed") {
        Some(HostKeyUnknown)
    } else if has("permission denied (") || has("too many authentication failures") || has("no supported authentication methods") {
        Some(AuthFailed)
    } else if has("connection timed out") || has("operation timed out") || has("timed out during banner exchange") {
        Some(Timeout)
    } else if has("could not resolve hostname")
        || has("connection refused")
        || has("no route to host")
        || has("network is unreachable")
        || has("connection closed by")
        || has("connection reset by")
        || has("kex_exchange_identification")
        || has("name or service not known")
        || has("proxycommand")
    {
        Some(Unreachable)
    } else if has("git: command not found") || has("git: not found") || has("command not found: git") || has("'git' is not recognized") {
        Some(NoGit)
    } else if has("not a git repository") || (has("cd:") && (has("no such file or directory") || has("can't cd") || has("not a directory"))) {
        Some(NotARepo)
    } else {
        None
    }
}

/// First informative lines of ssh stderr for the error detail (never the whole 64 KiB).
pub fn short_detail(stderr: &str) -> String {
    let mut out = String::new();
    for l in stderr.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if l.starts_with("Warning: Permanently added") {
            continue;
        }
        if !out.is_empty() {
            out.push_str(" | ");
        }
        out.push_str(l);
        if out.len() > 400 {
            out.truncate(400);
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RemoteSpec {
        RemoteSpec {
            name: "n".into(),
            host: "box.example".into(),
            user: None,
            port: None,
            identity_file: None,
            transport: Transport::Ssh,
            trust_new_host: false,
        }
    }

    fn pos(a: &[String], s: &str) -> usize {
        a.iter().position(|x| x == s).unwrap_or_else(|| panic!("{s} missing in {a:?}"))
    }

    #[test]
    fn quote_basic() {
        assert_eq!(shquote("abc"), "'abc'");
        assert_eq!(shquote(""), "''");
        assert_eq!(shquote("a b"), "'a b'");
        assert_eq!(shquote("it's"), "'it'\\''s'");
        assert_eq!(shquote("$(rm -rf /)`x`\"y\"\\"), "'$(rm -rf /)`x`\"y\"\\'");
        assert_eq!(shquote("a\nb"), "'a\nb'");
    }

    #[test]
    fn quote_round_trips_through_sh() {
        for s in ["plain", "with space", "it's", "$(echo pwned)", "`echo pwned`", "a\nb", "''", "\"", "\\", "*?[a]", "-x", "é한글", "; echo pwned", "a'b'c'"] {
            let out = Command::new("sh").arg("-c").arg(format!("printf %s {}", shquote(s))).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), s, "round trip of {s:?}");
        }
    }

    #[test]
    fn tilde_paths() {
        assert_eq!(quote_path("~"), "\"$HOME\"");
        assert_eq!(quote_path("~/"), "\"$HOME\"");
        assert_eq!(quote_path("~/a b/it's"), "\"$HOME\"/'a b/it'\\''s'");
        assert_eq!(quote_path("/srv/x"), "'/srv/x'");
        assert_eq!(quote_path("~user/x"), "'~user/x'");
    }

    #[test]
    fn git_script_shape() {
        let s = git_script("/srv/my repo", &["log", "--format=%H", "a b", "x'y", "$(id)"]).unwrap();
        assert!(s.starts_with("cd '/srv/my repo' && exec env LC_ALL=C GIT_OPTIONAL_LOCKS=0 GIT_TERMINAL_PROMPT=0 GIT_PAGER=cat git -c core.quotepath=false -c color.ui=never -c core.pager=cat "), "{s}");
        assert!(s.ends_with(" 'log' '--format=%H' 'a b' 'x'\\''y' '$(id)'"), "{s}");
        assert!(git_script("", &[]).is_err());
        assert!(git_script("/x", &["a\0b"]).is_err());
        assert!(git_script("/x\0", &[]).is_err());
    }

    #[test]
    fn wrap_executes_script_verbatim() {
        let script = git_script("/", &["--version"]).unwrap();
        // the wrapped form must survive one more level of quoting
        let out = Command::new("sh").arg("-c").arg(wrap_sh(&format!("printf %s {}", shquote("it's $(x)\n")))).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "it's $(x)\n");
        assert!(wrap_sh(&script).starts_with("sh -c '"));
    }

    #[test]
    fn args_default() {
        let a = ssh_args(&spec(), Path::new("/run/user/1/codetrail-ssh"), "CMD");
        assert_eq!(a[0], "-T");
        for o in [
            "BatchMode=yes",
            "ConnectTimeout=10",
            "ServerAliveInterval=10",
            "ServerAliveCountMax=3",
            "ControlMaster=auto",
            "ControlPersist=120",
            "StrictHostKeyChecking=yes",
            "ForwardAgent=no",
            "ForwardX11=no",
        ] {
            assert_eq!(a[pos(&a, o) - 1], "-o", "{o}");
        }
        let cp = format!("ControlPath=/run/user/1/codetrail-ssh/{}", policy_hash(&spec()));
        assert_eq!(a[pos(&a, &cp) - 1], "-o");
        assert!(!a.iter().any(|x| x.contains("%C")), "ssh's %C hash must not be used");
        let n = a.len();
        assert_eq!(&a[n - 3..], ["--", "box.example", "CMD"]);
        assert!(!a.iter().any(|x| x == "-p" || x == "-i" || x.starts_with("ProxyCommand") || x.contains("IdentitiesOnly")));
        assert!(!a.iter().any(|x| x.contains("StrictHostKeyChecking=no") || x == "-A" || x == "-X" || x == "-t" || x == "-tt"));
    }

    #[test]
    fn args_every_combo() {
        for mask in 0..32u32 {
            let mut s = spec();
            if mask & 1 != 0 {
                s.user = Some("bob".into());
            }
            if mask & 2 != 0 {
                s.port = Some(2222);
            }
            if mask & 4 != 0 {
                s.identity_file = Some("/home/me/.ssh/id with space".into());
            }
            if mask & 8 != 0 {
                s.transport = Transport::TailscaleNc;
            }
            if mask & 16 != 0 {
                s.trust_new_host = true;
            }
            let a = ssh_args(&s, Path::new("/d"), "echo 'hi there'");
            let has = |x: &str| a.iter().any(|y| y == x);
            assert_eq!(has("-p"), mask & 2 != 0);
            if mask & 2 != 0 {
                assert_eq!(a[pos(&a, "-p") + 1], "2222");
            }
            assert_eq!(has("-i"), mask & 4 != 0);
            if mask & 4 != 0 {
                assert_eq!(a[pos(&a, "-i") + 1], "/home/me/.ssh/id with space");
                assert!(has("IdentitiesOnly=yes"));
            } else {
                assert!(!has("IdentitiesOnly=yes"));
            }
            assert_eq!(has("ProxyCommand=tailscale nc %h %p"), mask & 8 != 0);
            assert_eq!(has("StrictHostKeyChecking=accept-new"), mask & 16 != 0);
            assert_eq!(has("StrictHostKeyChecking=yes"), mask & 16 == 0);
            assert!(!has("StrictHostKeyChecking=no"));
            let n = a.len();
            assert_eq!(a[n - 3], "--");
            assert_eq!(a[n - 2], if mask & 1 != 0 { "bob@box.example" } else { "box.example" });
            assert_eq!(a[n - 1], "echo 'hi there'");
        }
    }

    #[test]
    fn control_path_percent_is_escaped() {
        let a = ssh_args(&spec(), Path::new("/we%ird"), "x");
        assert!(a.iter().any(|x| *x == format!("ControlPath=/we%%ird/{}", policy_hash(&spec()))));
    }

    fn control_path_of(s: &RemoteSpec) -> String {
        let a = ssh_args(s, Path::new("/d"), "x");
        a.into_iter().find(|x| x.starts_with("ControlPath=")).unwrap()
    }

    /// Regression (review #1): ssh's `%C` ignores identity/transport/host-key policy, so specs that
    /// differ in any connection field must never share a ControlPath.
    #[test]
    fn every_connection_field_changes_the_control_path() {
        let base = spec();
        let mut seen = std::collections::HashSet::new();
        assert!(seen.insert(control_path_of(&base)));
        let variants: Vec<Box<dyn Fn(&mut RemoteSpec)>> = vec![
            Box::new(|s| s.host = "other.example".into()),
            Box::new(|s| s.user = Some("bob".into())),
            Box::new(|s| s.port = Some(2222)),
            Box::new(|s| s.identity_file = Some("/k/a".into())),
            Box::new(|s| s.identity_file = Some("/k/b".into())),
            Box::new(|s| s.transport = Transport::TailscaleNc),
            Box::new(|s| s.transport = Transport::TailscaleSsh),
            Box::new(|s| s.trust_new_host = true),
        ];
        for v in &variants {
            let mut s = base.clone();
            v(&mut s);
            assert!(seen.insert(control_path_of(&s)), "{s:?} shares a ControlPath");
        }
        // the label is not part of the policy
        let mut renamed = base.clone();
        renamed.name = "other label".into();
        assert_eq!(control_path_of(&renamed), control_path_of(&base));
        // field boundaries cannot be forged: user "a" + host "b" vs host "ab"
        let mut x = base.clone();
        x.user = Some("a".into());
        x.host = "b".into();
        let mut y = base.clone();
        y.host = "ab".into();
        assert_ne!(control_path_of(&x), control_path_of(&y));
    }

    #[test]
    fn control_path_fits_a_unix_socket() {
        let dir = "d".repeat(MAX_CTL_DIR);
        let cp = control_path_of_dir(&spec(), Path::new(&dir));
        // ssh appends a 17-byte ".XXXXXXXXXXXXXXXX" temp suffix while binding; sun_path is 108 incl. NUL
        assert!(cp.len() + 17 < 100, "{} bytes", cp.len());
    }

    fn control_path_of_dir(s: &RemoteSpec, d: &Path) -> String {
        control_path_arg(d, s).strip_prefix("ControlPath=").unwrap().to_string()
    }

    #[test]
    fn ephemeral_master_does_not_persist() {
        let a = ssh_args_with(&spec(), Path::new("/d"), "x", false);
        assert!(a.iter().any(|x| x == "ControlPersist=no") && !a.iter().any(|x| x == "ControlPersist=120"));
    }

    /// Regression (review #5): symlinks and foreign owners are rejected, never followed.
    #[test]
    fn control_dir_rejects_symlinks_and_picks_the_next_safe_candidate() {
        let t = tempfile::tempdir().unwrap();
        let victim = t.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::set_permissions(&victim, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let planted = t.path().join("ct-ssh-planted");
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        let e = ensure_private_dir(&planted).unwrap_err();
        assert!(e.to_string().contains("symlink"), "{e}");
        // the victim directory was not chmod'ed through the link
        assert_eq!(std::fs::metadata(&victim).unwrap().mode() & 0o777, 0o755);
        // a dangling symlink is rejected too
        let dangling = t.path().join("dangling");
        std::os::unix::fs::symlink(t.path().join("nowhere"), &dangling).unwrap();
        assert!(ensure_private_dir(&dangling).is_err());
        // only unsafe candidates → typed error, no panic, no fallback to /tmp guessing
        let err = pick_dir(&[planted.clone(), dangling.clone()]).unwrap_err();
        assert!(matches!(&err, Error::Remote { kind: RemoteErrorKind::Other, detail } if detail.contains("no safe directory")), "{err:?}");
        // a later safe candidate is used
        let good = t.path().join("good/ssh");
        assert_eq!(pick_dir(&[planted, good.clone()]).unwrap(), good);
        assert_eq!(std::fs::symlink_metadata(&good).unwrap().mode() & 0o777, 0o700);
        // loosened permissions on our own dir are repaired
        std::fs::set_permissions(&good, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        ensure_private_dir(&good).unwrap();
        assert_eq!(std::fs::metadata(&good).unwrap().mode() & 0o777, 0o700);
    }

    /// Regression (review #9): at most MAX_MASTERS live masters; the LRU one is evicted.
    #[test]
    fn master_registry_is_capped_lru() {
        let mk = |i: usize| RemoteSpec { host: format!("h{i}"), ..spec() };
        let mut r = Registry(Vec::new());
        for i in 0..MAX_MASTERS {
            assert!(r.touch(&mk(i)).is_none());
        }
        assert!(r.touch(&mk(0)).is_none(), "re-touching an existing master evicts nothing");
        let ev = r.touch(&mk(100)).expect("over the cap");
        assert_eq!(ev.host, "h1", "h0 was just used, h1 is the least recently used");
        assert_eq!(r.0.len(), MAX_MASTERS);
        r.forget(&mk(2));
        assert_eq!(r.0.len(), MAX_MASTERS - 1);
        assert!(r.touch(&mk(101)).is_none());
    }

    #[test]
    fn classify_real_openssh_messages() {
        use RemoteErrorKind::*;
        let cases: &[(&str, Option<RemoteErrorKind>)] = &[
            ("Host key verification failed.\r\n", Some(HostKeyUnknown)),
            (
                "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n@@@@@@@@\nOffending ECDSA key in /home/u/.ssh/known_hosts:3\nHost key for h has changed and you have requested strict checking.\nHost key verification failed.\n",
                Some(HostKeyChanged),
            ),
            ("u@h: Permission denied (publickey,password).\n", Some(AuthFailed)),
            ("u@h: Permission denied (publickey).", Some(AuthFailed)),
            ("ssh: Could not resolve hostname nope.invalid: Name or service not known\n", Some(Unreachable)),
            ("ssh: connect to host 10.0.0.9 port 22: Connection timed out\n", Some(Timeout)),
            ("ssh: connect to host 127.0.0.1 port 1: Connection refused\n", Some(Unreachable)),
            ("ssh: connect to host h port 22: No route to host\n", Some(Unreachable)),
            ("ssh: connect to host h port 22: Network is unreachable\n", Some(Unreachable)),
            ("Connection closed by 10.0.0.1 port 22\n", Some(Unreachable)),
            ("kex_exchange_identification: read: Connection reset by peer\n", Some(Unreachable)),
            ("sh: 1: git: not found\n", Some(NoGit)),
            ("bash: line 1: git: command not found\n", Some(NoGit)),
            ("fatal: not a git repository (or any of the parent directories): .git\n", Some(NotARepo)),
            ("sh: 1: cd: can't cd to /nope\n", Some(NotARepo)),
            ("bash: line 0: cd: /nope: No such file or directory\n", Some(NotARepo)),
            ("fatal: bad revision 'zzz'\n", None),
            ("error: open(\"x\"): Permission denied\n", None),
            ("", None),
        ];
        for (s, want) in cases {
            assert_eq!(classify_stderr(s), *want, "for {s:?}");
        }
    }

    #[test]
    fn detail_is_short() {
        let big = "x".repeat(10_000);
        assert!(short_detail(&format!("Warning: Permanently added 'h' (ED25519) to the list of known hosts.\n{big}")).len() <= 410);
        assert_eq!(short_detail("a\n\nb\n"), "a | b");
    }

    #[test]
    fn tailscale_ssh_is_plain_ssh_without_key_or_proxy() {
        let mut s = RemoteSpec { name: "n".into(), host: "box.tail1.ts.net".into(), user: Some("me".into()), port: None, identity_file: Some("/k/id".into()), transport: Transport::TailscaleSsh, trust_new_host: false };
        let a = ssh_args(&s, Path::new("/d"), "true");
        assert!(!a.iter().any(|x| x == "-i" || x == "IdentitiesOnly=yes" || x.starts_with("ProxyCommand")), "{a:?}");
        assert!(a.iter().any(|x| x == "BatchMode=yes") && a.iter().any(|x| x == "StrictHostKeyChecking=yes"));
        assert_eq!(a[a.len() - 2], "me@box.tail1.ts.net");
        // the same spec over plain ssh keeps the key
        s.transport = Transport::Ssh;
        assert!(ssh_args(&s, Path::new("/d"), "true").iter().any(|x| x == "-i"));
    }
}
