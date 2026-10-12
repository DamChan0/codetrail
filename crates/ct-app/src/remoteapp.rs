//! Remote repositories: state, async jobs and the project-picker state machine. Every request goes
//! through the [`RemoteBackend`] trait (ct-core's ssh layer in production), so each ssh child is
//! bounded by ct-core's git slot limiter, timeout, output cap and process-group kill. Nothing here
//! polls: probes run on a button press, the folder browser on navigation (older requests are
//! cancelled by the job system), and the Current tab on its own backed-off timer (`wtapp`).

use crate::app::{App, Loadable, Msg};
use crate::jobs::JobKind;
use crate::remote_backend::{RealRemote, RemoteBackend, RemoteForm};
use ct_core::remote::discover::{SshHost, TailnetPeer};
use ct_core::remote::{DirEntry, RemoteInfo, RemoteSpec};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

const TEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Ok(RemoteInfo),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestTarget {
    Named(String),
    Form,
}

pub struct FormState {
    /// Name of the remote being edited (`None` = new).
    pub orig: Option<String>,
    pub f: RemoteForm,
    pub error: Option<String>,
    pub test: Option<Result<RemoteInfo, String>>,
}

pub struct ImportState {
    pub hosts: Vec<(SshHost, bool)>,
}

/// One directory of a remote host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteListing {
    pub path: String,
    pub entries: Vec<DirEntry>,
    /// The directory itself is a git working tree (`Open here` is offered only then).
    pub is_git: bool,
}

/// Remote half of the folder browser.
pub struct RemotePicker {
    pub spec: RemoteSpec,
    pub cwd: String,
    pub input: String,
    pub listing: Loadable<RemoteListing>,
    pub token: u64,
    pub error: Option<String>,
    pub auto: crate::autocomplete::Auto,
}

#[derive(Default)]
pub struct RemoteUi {
    pub backend: Option<Arc<dyn RemoteBackend>>,
    pub status: HashMap<String, Status>,
    pub testing: Option<TestTarget>,
    pub test_token: u64,
    pub form: Option<FormState>,
    pub import: Option<ImportState>,
    pub peers: Option<Loadable<Vec<TailnetPeer>>>,
    pub picker: Option<RemotePicker>,
    /// Set when the link to the open remote project failed; the banner offers Retry.
    pub conn_lost: Option<String>,
    /// Consecutive failed Current-tab polls (drives the backoff).
    pub poll_fails: u32,
}

impl RemoteUi {
    pub fn backend(&self) -> Arc<dyn RemoteBackend> {
        self.backend.clone().unwrap_or_else(|| Arc::new(RealRemote))
    }
}

/// `(parent, name)` of a remote path; `None` at `~`, `/` and `~user`.
pub fn parent_of(path: &str) -> Option<(String, String)> {
    let p = if path.len() > 1 { path.trim_end_matches('/') } else { path };
    if p == "~" || p == "/" || p.is_empty() {
        return None;
    }
    let (dir, name) = p.rsplit_once('/')?;
    Some((if dir.is_empty() { "/".to_string() } else { dir.to_string() }, name.to_string()))
}

pub fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Every ancestor as `(label, path)`, root (`~` or `/`) first.
pub fn remote_breadcrumbs(path: &str) -> Vec<(String, String)> {
    let p = path.trim();
    let (root, rest) = if p == "~" || p.starts_with("~/") { ("~", p.trim_start_matches('~')) } else { ("/", p) };
    let mut out = vec![(root.to_string(), root.to_string())];
    let mut acc = if root == "~" { "~".to_string() } else { String::new() };
    for part in rest.split('/').filter(|s| !s.is_empty()) {
        acc = format!("{acc}/{part}");
        out.push((part.to_string(), acc.clone()));
    }
    out
}

/// One listing plus, when `known_git` is unknown, one more `list_dir` of the parent to learn
/// whether `path` itself is a git working tree.
pub fn browse_remote(be: &dyn RemoteBackend, spec: &RemoteSpec, path: &str, known_git: Option<bool>, cancel: Arc<AtomicBool>) -> Result<RemoteListing, String> {
    let entries = be.list_dir(spec, path, Some(cancel.clone()))?;
    let is_git = match known_git {
        Some(g) => g,
        None => match parent_of(path) {
            Some((parent, name)) if !cancel.load(std::sync::atomic::Ordering::Relaxed) => be.list_dir(spec, &parent, Some(cancel)).map(|v| v.iter().any(|e| e.name == name && e.is_git_repo)).unwrap_or(false),
            _ => false,
        },
    };
    Ok(RemoteListing { path: path.to_string(), entries, is_git })
}

impl App {
    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    // ------------------------------------------------------------ connection test

    pub fn rs_test(&mut self, target: TestTarget, spec: RemoteSpec) {
        self.rs.test_token += 1;
        let token = self.rs.test_token;
        self.rs.testing = Some(target);
        let be = self.rs.backend();
        self.jobs.spawn(JobKind::RemoteTest, move |c| {
            c.finish(Msg::RemoteTest { token, res: be.test(&spec, TEST_TIMEOUT) });
        });
    }

    pub fn rs_test_done(&mut self, token: u64, res: Result<RemoteInfo, String>) {
        if token != self.rs.test_token {
            return;
        }
        match self.rs.testing.take() {
            Some(TestTarget::Named(n)) => {
                self.rs.status.insert(n, res.map_or_else(Status::Failed, Status::Ok));
            }
            Some(TestTarget::Form) => {
                if let Some(f) = &mut self.rs.form {
                    f.test = Some(res);
                }
            }
            None => {}
        }
    }

    // ------------------------------------------------------------ settings model

    pub fn rs_form_new(&mut self) {
        self.rs.form = Some(FormState { orig: None, f: RemoteForm::default(), error: None, test: None });
    }

    pub fn rs_form_edit(&mut self, name: &str) {
        if let Some(s) = self.settings.remotes.iter().find(|r| r.name == name) {
            self.rs.form = Some(FormState { orig: Some(name.to_string()), f: RemoteForm::from_spec(s), error: None, test: None });
        }
    }

    /// Validates and stores the form. Returns false (error kept on the form) when invalid.
    pub fn rs_form_save(&mut self) -> bool {
        let Some(form) = &mut self.rs.form else { return false };
        let others: Vec<&str> = self.settings.remotes.iter().map(|r| r.name.as_str()).filter(|n| Some(*n) != form.orig.as_deref()).collect();
        let spec = match form.f.build(&others) {
            Ok(s) => s,
            Err(e) => {
                form.error = Some(e);
                return false;
            }
        };
        if form.orig.is_none() && self.settings.remotes.len() >= crate::settings::MAX_REMOTES {
            form.error = Some(format!("At most {} remotes are supported.", crate::settings::MAX_REMOTES));
            return false;
        }
        let orig = form.orig.clone();
        match orig.and_then(|o| self.settings.remotes.iter().position(|r| r.name == o)) {
            Some(i) => {
                let old = std::mem::replace(&mut self.settings.remotes[i], spec.clone());
                if old.name != spec.name {
                    self.rs.status.remove(&old.name);
                    for r in self.settings.recent.iter_mut().filter(|r| r.remote.as_deref() == Some(old.name.as_str())) {
                        r.remote = Some(spec.name.clone());
                    }
                }
            }
            None => self.settings.remotes.push(spec.clone()),
        }
        self.rs.status.remove(&spec.name);
        self.rs.form = None;
        self.settings_dirty_at = Some(std::time::Instant::now());
        true
    }

    pub fn rs_delete(&mut self, name: &str) {
        self.settings.remotes.retain(|r| r.name != name);
        self.rs.status.remove(name);
        self.settings_dirty_at = Some(std::time::Instant::now());
    }

    pub fn rs_import_open(&mut self) {
        let taken: Vec<&str> = self.settings.remotes.iter().map(|r| r.name.as_str()).collect();
        let hosts = self.rs.backend().ssh_hosts().into_iter().filter(|h| !taken.contains(&h.alias.as_str())).map(|h| (h, false)).collect();
        self.rs.import = Some(ImportState { hosts });
    }

    /// Adds the ticked hosts. Aliases are passed as the ssh host so `~/.ssh/config` stays the
    /// single source for user/port/identity; invalid or over-limit ones are skipped.
    pub fn rs_import_apply(&mut self) -> usize {
        let Some(imp) = self.rs.import.take() else { return 0 };
        let mut added = 0;
        for (h, _) in imp.hosts.into_iter().filter(|(_, on)| *on) {
            if self.settings.remotes.len() >= crate::settings::MAX_REMOTES || self.settings.remotes.iter().any(|r| r.name == h.alias) {
                continue;
            }
            let spec = RemoteSpec { name: h.alias.clone(), host: h.alias.clone(), user: None, port: None, identity_file: None, transport: Default::default(), trust_new_host: false };
            if spec.validate().is_ok() {
                self.settings.remotes.push(spec);
                added += 1;
            }
        }
        if added > 0 {
            self.settings_dirty_at = Some(std::time::Instant::now());
        }
        added
    }

    pub fn rs_discover(&mut self) {
        self.rs.peers = Some(Loadable::Loading);
        let be = self.rs.backend();
        self.jobs.spawn(JobKind::Discover, move |c| {
            c.finish(Msg::Peers(be.tailscale_peers()));
        });
    }

    pub fn rs_peers_done(&mut self, res: Result<Vec<TailnetPeer>, String>) {
        self.rs.peers = Some(match res {
            Ok(mut v) => {
                v.retain(|p| !p.is_self);
                v.sort_by_key(|p| (!p.online, p.name.to_lowercase()));
                Loadable::Ready(v)
            }
            Err(e) => Loadable::Failed(e),
        });
    }

    /// Prefills the add form from a tailnet peer (host = MagicDNS name, Tailscale transport off:
    /// a peer reachable by name works with plain ssh; the nc transport is for hosts without sshd on
    /// the tailnet interface).
    pub fn rs_peer_use(&mut self, p: &TailnetPeer) {
        let name: String = p.name.chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).collect();
        self.rs.form = Some(FormState { orig: None, f: RemoteForm { name, host: p.address().to_string(), ..Default::default() }, error: None, test: None });
    }

    // ------------------------------------------------------------ project picker

    pub fn remote_spec(&self, name: &str) -> Option<RemoteSpec> {
        self.settings.remotes.iter().find(|r| r.name == name).cloned()
    }

    /// Switches the folder browser to a remote (starts at `~`).
    pub fn pj_remote_pick(&mut self, spec: RemoteSpec) {
        self.pj_remote_pick_at(spec, "~");
    }

    pub fn pj_remote_pick_at(&mut self, spec: RemoteSpec, path: &str) {
        self.rs.picker = Some(RemotePicker { spec, cwd: path.into(), input: path.into(), listing: Loadable::Loading, token: 0, error: None, auto: Default::default() });
        self.pj.open_error = None;
        self.pj_remote_go(path.into(), None);
    }

    pub fn pj_remote_local(&mut self) {
        self.rs.picker = None;
        self.jobs.cancel(JobKind::RemoteBrowse);
    }

    /// Lists `path`; a newer navigation cancels the previous request (the job system raises its
    /// cancel flag, which ct-core turns into a process-group kill) and its answer is ignored.
    pub fn pj_remote_go(&mut self, path: String, known_git: Option<bool>) {
        let Some(p) = &mut self.rs.picker else { return };
        p.token += 1;
        let (token, spec) = (p.token, p.spec.clone());
        if p.listing.ready().is_none() {
            p.listing = Loadable::Loading;
        }
        let be = self.rs.backend();
        self.jobs.spawn(JobKind::RemoteBrowse, move |c| {
            let res = browse_remote(&*be, &spec, &path, known_git, c.cancel.clone());
            c.finish(Msg::RemoteBrowse { token, res });
        });
    }

    pub fn pj_remote_handle(&mut self, token: u64, res: Result<RemoteListing, String>) {
        let Some(p) = &mut self.rs.picker else { return };
        if token != p.token {
            return;
        }
        match res {
            Ok(l) => {
                p.cwd = l.path.clone();
                p.input = l.path.clone();
                p.auto.close();
                p.error = None;
                p.listing = Loadable::Ready(l);
            }
            Err(e) => {
                p.error = Some(e);
                if matches!(p.listing, Loadable::Loading) {
                    p.listing = Loadable::Idle;
                }
            }
        }
    }

    /// Opens the picked remote directory as the project. Only offered for git working trees.
    pub fn pj_open_remote(&mut self, spec: RemoteSpec, path: String) -> Result<(), String> {
        if path.trim().is_empty() {
            return Err("empty remote path".into());
        }
        self.pj.browser = None;
        self.rs.picker = None;
        self.pj.popover = false;
        self.switch_to(std::path::PathBuf::from(path), Some(spec));
        Ok(())
    }

    /// Opens a recent entry (local path or remote project).
    pub fn pj_open_recent(&mut self, remote: Option<&str>, path: &str) {
        let res = match remote {
            None => self.pj_open(std::path::PathBuf::from(path)),
            Some(n) => match self.remote_spec(n) {
                Some(spec) => self.pj_open_remote(spec, path.to_string()),
                None => Err(format!("Remote \"{n}\" is no longer configured (Settings → Remotes).")),
            },
        };
        if let Err(e) = res {
            self.flash(e);
            self.pj_forget(remote, path);
        }
    }

    // ------------------------------------------------------------ connection state

    /// Records a request failure; link-level failures raise the "Connection lost" banner.
    pub fn rs_note_error(&mut self, err: &str) {
        if self.remote.is_some() && crate::remote_backend::is_connection_error(err) {
            self.rs.conn_lost = Some(err.to_string());
        }
    }

    pub fn rs_retry(&mut self) {
        self.rs.conn_lost = None;
        self.rs.poll_fails = 0;
        if self.repo.is_none() {
            self.open_repo();
        } else {
            self.request_status();
            if self.commits.is_empty() {
                self.request_log(false);
            }
        }
    }
}

// ---------------------------------------------------------------- UI

use crate::projects::home_dir;
use crate::settingspage::{hint, row};
use crate::widgets::{self, Btn, BtnKind, Level};
use egui::{pos2, vec2, Align, Layout, RichText, ScrollArea, Ui};

/// What the remote half of the folder browser asked for during one frame.
#[derive(Default)]
pub struct PickAct {
    pub go: Option<(String, Option<bool>)>,
    pub open: Option<String>,
    pub cancel: bool,
}

fn dot(ui: &mut Ui, col: egui::Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, col);
    resp
}

fn text_field(ui: &mut Ui, th: &crate::theme::Theme, label: &str, value: &mut String, hint_text: &str) {
    row(ui, th, label, |ui| {
        let w = ui.available_width().clamp(120.0, 360.0);
        ui.add(egui::TextEdit::singleline(value).hint_text(hint_text).desired_width(w).margin(vec2(8.0, 6.0)));
    });
}

/// Private keys in `~/.ssh` (no `.pub`, no config/known_hosts), for the identity "Browse" menu.
fn ssh_key_candidates() -> Vec<String> {
    let dir = home_dir().join(".ssh");
    let mut v: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.ends_with(".pub") && !n.starts_with("known_hosts") && !n.starts_with("authorized_keys") && n != "config" && !n.ends_with(".old"))
        .map(|n| dir.join(n).to_string_lossy().into_owned())
        .collect();
    v.sort();
    v.truncate(32);
    v
}

fn info_line(i: &RemoteInfo) -> String {
    format!("{} · {} · {} ms", i.hostname, i.git_version, i.latency_ms)
}

impl App {
    pub(crate) fn sec_remotes(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        hint(ui, &th, "Remote repositories are read-only: git runs on the remote over ssh. Nothing connects until you test a remote or open a project on it. Passwords are never asked for; use keys or ssh-agent.");
        let mut act: Option<(&'static str, String)> = None;
        if self.settings.remotes.is_empty() && self.rs.form.is_none() {
            widgets::empty_state(ui, &th, "No remotes yet", "Add one below, import from ~/.ssh/config, or discover Tailscale peers.");
        }
        let remotes = self.settings.remotes.clone();
        for r in &remotes {
            egui::Frame::new().fill(th.raised()).stroke(egui::Stroke::new(1.0, th.border())).corner_radius(th.radius()).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let testing = self.rs.testing == Some(TestTarget::Named(r.name.clone()));
                ui.horizontal(|ui| {
                    let (col, tip) = match self.rs.status.get(&r.name) {
                        None => (th.muted(), "Not tested yet".to_string()),
                        Some(Status::Ok(i)) => (th.p().diff.add.fg.0, info_line(i)),
                        Some(Status::Failed(e)) => (widgets::level_color(&th, Level::Error), e.clone()),
                    };
                    dot(ui, col).on_hover_text(tip);
                    let w = ui.available_width();
                    let target = format!("{}{}", r.target(), r.port.map(|p| format!(":{p}")).unwrap_or_default());
                    let g = widgets::fit(ui, &format!("{}  ·  {target}", r.name), widgets::ui_font(&th), th.fg(), w);
                    ui.label(g);
                });
                match self.rs.status.get(&r.name) {
                    Some(Status::Ok(i)) => hint(ui, &th, &info_line(i)),
                    Some(Status::Failed(e)) => {
                        ui.label(RichText::new(e).font(widgets::small_font(&th)).color(widgets::level_color(&th, Level::Error)));
                    }
                    None => {}
                }
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                    if Btn::new("Test connection").compact().enabled(self.rs.testing.is_none()).show(ui, &th).clicked() {
                        act = Some(("test", r.name.clone()));
                    }
                    if Btn::new("Edit").compact().show(ui, &th).clicked() {
                        act = Some(("edit", r.name.clone()));
                    }
                    if Btn::new("Delete").compact().show(ui, &th).clicked() {
                        act = Some(("delete", r.name.clone()));
                    }
                    if testing {
                        ui.add(egui::Spinner::new().size(14.0));
                        hint(ui, &th, "Connecting…");
                    }
                });
            });
        }
        match act {
            Some(("test", n)) => {
                if let Some(s) = self.remote_spec(&n) {
                    self.rs_test(TestTarget::Named(n), s);
                }
            }
            Some(("edit", n)) => self.rs_form_edit(&n),
            Some(("delete", n)) => self.rs_delete(&n),
            _ => {}
        }
        self.remote_form_ui(ui);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            if Btn::new("Add").kind(BtnKind::Primary).show(ui, &th).clicked() {
                self.rs_form_new();
            }
            if Btn::new("Import from ~/.ssh/config").show(ui, &th).clicked() {
                self.rs_import_open();
            }
            if Btn::new("Discover Tailscale peers").enabled(!matches!(self.rs.peers, Some(Loadable::Loading))).show(ui, &th).clicked() {
                self.rs_discover();
            }
        });
        self.remote_import_ui(ui);
        self.remote_peers_ui(ui);
    }

    fn remote_form_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let Some(mut form) = self.rs.form.take() else { return };
        let (mut save, mut cancel, mut test) = (false, false, false);
        let testing = self.rs.testing == Some(TestTarget::Form);
        egui::Frame::new().fill(th.sunken()).stroke(egui::Stroke::new(1.0, th.border())).corner_radius(th.radius()).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::heading(ui, &th, if form.orig.is_some() { "Edit remote" } else { "Add remote" });
            let f = &mut form.f;
            text_field(ui, &th, "Name", &mut f.name, "dev-box");
            text_field(ui, &th, "Host", &mut f.host, "dev.example.com, 100.x.y.z or ssh alias");
            text_field(ui, &th, "User", &mut f.user, "optional");
            text_field(ui, &th, "Port", &mut f.port, "22");
            row(ui, &th, "Identity file", |ui| {
                let w = (ui.available_width() - 96.0).clamp(100.0, 280.0);
                ui.add(egui::TextEdit::singleline(&mut f.identity_file).hint_text("optional, e.g. ~/.ssh/id_ed25519").desired_width(w).margin(vec2(8.0, 6.0)));
                let keys = ssh_key_candidates();
                ui.menu_button("Browse", |ui| {
                    if keys.is_empty() {
                        ui.label("No keys in ~/.ssh");
                    }
                    for k in keys {
                        if ui.button(&k).clicked() {
                            f.identity_file = k;
                            ui.close_menu();
                        }
                    }
                });
            });
            row(ui, &th, "Transport", |ui| {
                let mut i = usize::from(f.tailscale_nc);
                if widgets::segmented(ui, &th, &["SSH", "SSH via Tailscale nc"], &mut i) {
                    f.tailscale_nc = i == 1;
                }
            });
            ui.checkbox(&mut f.trust_new_host, "Trust new host key");
            if f.trust_new_host {
                widgets::banner(ui, &th, Level::Warn, "A host key seen for the first time is accepted without asking (accept-new). A changed key is still refused. Only enable this for hosts you control.", None, false);
            }
            if let Some(e) = &form.error {
                widgets::banner(ui, &th, Level::Error, e, None, false);
            }
            match &form.test {
                Some(Ok(i)) => hint(ui, &th, &format!("Connected: {}", info_line(i))),
                Some(Err(e)) => {
                    ui.label(RichText::new(e).font(widgets::small_font(&th)).color(widgets::level_color(&th, Level::Error)));
                }
                None => {}
            }
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                if Btn::new("Save").kind(BtnKind::Primary).show(ui, &th).clicked() {
                    save = true;
                }
                if Btn::new("Cancel").show(ui, &th).clicked() {
                    cancel = true;
                }
                if Btn::new("Test connection").enabled(self.rs.testing.is_none()).show(ui, &th).clicked() {
                    test = true;
                }
                if testing {
                    ui.add(egui::Spinner::new().size(14.0));
                }
            });
        });
        if test {
            let others: Vec<&str> = Vec::new();
            match form.f.build(&others) {
                Ok(spec) => {
                    form.error = None;
                    form.test = None;
                    self.rs.form = Some(form);
                    self.rs_test(TestTarget::Form, spec);
                    return;
                }
                Err(e) => form.error = Some(e),
            }
        }
        self.rs.form = if cancel { None } else { Some(form) };
        if save {
            self.rs_form_save();
        }
    }

    fn remote_import_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let Some(mut imp) = self.rs.import.take() else { return };
        let (mut apply, mut cancel) = (false, false);
        egui::Frame::new().fill(th.sunken()).stroke(egui::Stroke::new(1.0, th.border())).corner_radius(th.radius()).inner_margin(egui::Margin::same(16)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::heading(ui, &th, "Import from ~/.ssh/config");
            if imp.hosts.is_empty() {
                hint(ui, &th, "No importable hosts found (wildcards are skipped, existing names are hidden).");
            }
            ScrollArea::vertical().id_salt("import_hosts").max_height(200.0).auto_shrink([true, true]).show(ui, |ui| {
                for (h, on) in &mut imp.hosts {
                    let detail = [h.user.clone().map(|u| format!("{u}@")), Some(h.hostname.clone().unwrap_or_else(|| h.alias.clone())), h.port.map(|p| format!(":{p}"))].into_iter().flatten().collect::<String>();
                    ui.checkbox(on, format!("{}  ·  {detail}", h.alias));
                }
            });
            let n = imp.hosts.iter().filter(|(_, on)| *on).count();
            ui.horizontal(|ui| {
                if Btn::new(&format!("Add {n} selected")).kind(BtnKind::Primary).enabled(n > 0).show(ui, &th).clicked() {
                    apply = true;
                }
                if Btn::new("Cancel").show(ui, &th).clicked() {
                    cancel = true;
                }
            });
        });
        self.rs.import = if cancel { None } else { Some(imp) };
        if apply {
            self.rs_import_apply();
        }
    }

    fn remote_peers_ui(&mut self, ui: &mut Ui) {
        let th = self.th.clone();
        let Some(state) = &self.rs.peers else { return };
        let mut pick: Option<TailnetPeer> = None;
        let mut close = false;
        egui::Frame::new().fill(th.sunken()).stroke(egui::Stroke::new(1.0, th.border())).corner_radius(th.radius()).inner_margin(egui::Margin::same(16)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::heading(ui, &th, "Tailscale peers");
            match state {
                Loadable::Loading => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0));
                        hint(ui, &th, "Asking the local tailscale daemon…");
                    });
                }
                Loadable::Failed(e) => {
                    widgets::banner(ui, &th, Level::Warn, e, None, false);
                }
                Loadable::Ready(v) if v.is_empty() => hint(ui, &th, "No other peers on this tailnet."),
                Loadable::Ready(v) => {
                    hint(ui, &th, "Click a peer to prefill the add form. Online peers first.");
                    ScrollArea::vertical().id_salt("peer_list").max_height(220.0).auto_shrink([true, true]).show(ui, |ui| {
                        for p in v {
                            let (resp, rect) = widgets::list_row(ui, &th, 32.0, false);
                            let col = if p.online { th.p().diff.add.fg.0 } else { th.muted() };
                            ui.painter().circle_filled(pos2(rect.min.x + 6.0, rect.center().y), 4.0, col);
                            let text = format!("{}  ·  {}{}", p.name, p.address(), if p.os.is_empty() { String::new() } else { format!("  ·  {}", p.os) });
                            widgets::paint_text_fit(ui, egui::Rect::from_min_max(pos2(rect.min.x + 18.0, rect.min.y), rect.max), &text, widgets::ui_font(&th), if p.online { th.fg() } else { th.muted() });
                            if resp.on_hover_text(if p.online { "Use as host" } else { "Offline" }).clicked() {
                                pick = Some(p.clone());
                            }
                        }
                    });
                }
                Loadable::Idle => {}
            }
            if Btn::new("Close").show(ui, &th).clicked() {
                close = true;
            }
        });
        if let Some(p) = pick {
            self.rs_peer_use(&p);
        }
        if close {
            self.rs.peers = None;
        }
    }
}

/// Remote half of the folder browser (inside the dialog).
pub fn remote_picker_ui(ui: &mut Ui, th: &crate::theme::Theme, p: &mut RemotePicker, act: &mut PickAct) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
        for (i, (label, path)) in remote_breadcrumbs(&p.cwd).into_iter().enumerate() {
            if i > 0 {
                ui.label(RichText::new("›").color(th.muted()));
            }
            if Btn::new(&label).compact().show(ui, th).clicked() {
                act.go = Some((path, None));
            }
        }
    });
    ui.horizontal(|ui| {
        let up = parent_of(&p.cwd).map(|(d, _)| d);
        if Btn::new("Up").compact().enabled(up.is_some()).show(ui, th).clicked() {
            act.go = up.map(|d| (d, None));
        }
        if Btn::new("Home").compact().show(ui, th).clicked() {
            act.go = Some(("~".into(), None));
        }
        let w = (ui.available_width() - 8.0).max(120.0);
        let f = crate::autocomplete::path_field(ui, th, &mut p.input, &mut p.auto, "remote", "Type a remote path, e.g. ~/projects/app", w, false);
        if f.submit && !p.input.trim().is_empty() {
            act.go = Some((p.input.trim().to_string(), None));
        }
    });
    let (text, col) = match (&p.error, &p.listing) {
        (Some(e), _) => (e.clone(), widgets::level_color(th, Level::Error)),
        (None, Loadable::Ready(l)) if l.is_git => ("Git repository: ready to open.".to_string(), th.p().diff.add.fg.0),
        (None, Loadable::Ready(_)) => ("Not a git repository. Open a folder that contains .git.".to_string(), th.warn()),
        (None, Loadable::Loading) => ("Reading remote folder…".to_string(), th.muted()),
        _ => (String::new(), th.muted()),
    };
    ui.label(RichText::new(if text.is_empty() { " " } else { text.as_str() }).font(widgets::small_font(th)).color(col));
    egui::Frame::new().fill(th.sunken()).stroke(egui::Stroke::new(1.0, th.border())).corner_radius(th.radius()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ScrollArea::vertical().id_salt("remote_folder_list").max_height(240.0).min_scrolled_height(240.0).auto_shrink([false, false]).show(ui, |ui| match &p.listing {
            Loadable::Ready(l) if l.entries.is_empty() => widgets::empty_state(ui, th, "No sub-folders", "Open this folder if it is a repository, or go up."),
            Loadable::Ready(l) => {
                for e in &l.entries {
                    let (resp, rect) = widgets::list_row(ui, th, 28.0, false);
                    let mut right = rect.max.x;
                    if e.is_git_repo {
                        let g = widgets::galley(ui, "git", widgets::small_font(th), th.accent());
                        right -= g.size().x + 14.0;
                        widgets::paint_badge(ui.painter(), th, pos2(right, rect.center().y), "git", th.accent());
                    }
                    widgets::paint_text_fit(ui, egui::Rect::from_min_max(rect.min, pos2(right - 8.0, rect.max.y)), &e.name, widgets::ui_font(th), th.fg());
                    if resp.double_clicked() || resp.clicked() {
                        act.go = Some((join_remote(&l.path, &e.name), Some(e.is_git_repo)));
                    }
                }
            }
            Loadable::Loading => widgets::skeleton(ui, th, 6, 28.0),
            _ => {}
        });
    });
    let can_open = p.error.is_none() && matches!(&p.listing, Loadable::Ready(l) if l.is_git);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if Btn::new("Open here").kind(BtnKind::Primary).enabled(can_open).show(ui, th).on_disabled_hover_text("not a git repository").clicked() {
            act.open = Some(p.cwd.clone());
        }
        if Btn::new("Cancel").show(ui, th).clicked() {
            act.cancel = true;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents_fake::FakeAgents;
    use crate::app::{AskPhase, RailTab, TargetSel};
    use crate::remote_backend::fake::ScriptedRemote;
    use crate::settings::ViewMode;
    use ct_core::remote::Transport;
    use std::path::Path;
    use std::time::Instant;

    fn spec(name: &str, host: &str) -> RemoteSpec {
        RemoteSpec { name: name.into(), host: host.into(), user: Some("me".into()), port: None, identity_file: None, transport: Transport::Ssh, trust_new_host: false }
    }

    fn app_with(path: &Path, remote: Option<RemoteSpec>, be: Option<Arc<dyn RemoteBackend>>) -> App {
        let (th, _) = crate::theme::ThemeFile::load(Path::new("/nonexistent/theme.toml"));
        let mut settings = crate::settings::Settings::default();
        settings.remotes = remote.iter().cloned().collect();
        let mut app = App::new_with(egui::Context::default(), path.to_path_buf(), remote, th, settings, Vec::new(), String::new(), None, Arc::new(FakeAgents::new()));
        app.rs.backend = be;
        app
    }

    fn pump_until(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
        let t = Instant::now();
        while !done(app) {
            assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
            app.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn scripted_app() -> (App, Arc<ScriptedRemote>) {
        let be = ScriptedRemote::with(&[("~", &[("code", false), ("notes", false)]), ("~/code", &[("app", true), ("docs", false)]), ("~/code/app", &[("src", false)]), ("~/code/docs", &[])]);
        let app = app_with(Path::new(""), None, Some(be.clone()));
        (app, be)
    }

    fn listing(app: &App) -> RemoteListing {
        app.rs.picker.as_ref().unwrap().listing.ready().unwrap().clone()
    }

    #[test]
    fn breadcrumbs_parent_and_join_handle_home_root_and_trailing_slashes() {
        assert_eq!(remote_breadcrumbs("~"), [("~".to_string(), "~".to_string())]);
        assert_eq!(remote_breadcrumbs("~/a/b").iter().map(|c| c.1.as_str()).collect::<Vec<_>>(), ["~", "~/a", "~/a/b"]);
        assert_eq!(remote_breadcrumbs("/srv/x").iter().map(|c| c.1.as_str()).collect::<Vec<_>>(), ["/", "/srv", "/srv/x"]);
        assert_eq!(remote_breadcrumbs("/")[0].1, "/");
        assert_eq!(parent_of("~/a/b"), Some(("~/a".into(), "b".into())));
        assert_eq!(parent_of("/srv"), Some(("/".into(), "srv".into())));
        assert_eq!(parent_of("/srv/x/"), Some(("/srv".into(), "x".into())));
        assert_eq!((parent_of("~"), parent_of("/")), (None, None));
        assert_eq!(join_remote("~", "a"), "~/a");
        assert_eq!(join_remote("/", "a"), "/a");
    }

    #[test]
    fn remote_autocomplete_makes_one_list_call_per_parent_not_one_per_keystroke() {
        let be = ScriptedRemote::with(&[("~", &[("code", false)]), ("~/code/", &[("app", true), ("apps-old", false), ("docs", false)])]);
        let mut app = app_with(Path::new(""), None, Some(be.clone()));
        app.pj_open_browser();
        app.pj_remote_pick(spec("box", "box.example"));
        pump_until(&mut app, "home listing", |a| a.rs.picker.as_ref().unwrap().listing.ready().is_some());
        let ctx = egui::Context::default();
        let baseline = be.calls.lock().len();
        let past = Instant::now() - Duration::from_secs(1);
        let edit = |app: &mut App, text: &str, at: Instant| app.rs.picker.as_mut().unwrap().auto.edit(text, at);
        edit(&mut app, "~/code/a", past);
        let t = Instant::now();
        while app.rs.picker.as_ref().unwrap().auto.matches.is_empty() {
            assert!(t.elapsed() < Duration::from_secs(10), "no answer");
            app.pj_complete_poll(&ctx);
            app.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        let names: Vec<String> = app.rs.picker.as_ref().unwrap().auto.matches.iter().map(|c| c.name.clone()).collect();
        assert_eq!(names, ["app", "apps-old"]);
        for text in ["~/code/ap", "~/code/app", "~/code/apps", "~/code/d"] {
            edit(&mut app, text, Instant::now());
            app.pj_complete_poll(&ctx);
        }
        std::thread::sleep(Duration::from_millis(50));
        app.pump();
        assert_eq!(be.calls.lock().len() - baseline, 1, "{:?}", be.calls.lock());
        assert_eq!(app.rs.picker.as_ref().unwrap().auto.matches.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["docs"]);
    }

    #[test]
    fn picker_starts_at_home_marks_git_repos_and_offers_open_only_for_git_dirs() {
        let (mut app, be) = scripted_app();
        app.pj_remote_pick(spec("box", "box.example"));
        pump_until(&mut app, "home listing", |a| a.rs.picker.as_ref().unwrap().listing.ready().is_some());
        let l = listing(&app);
        assert_eq!((l.path.as_str(), l.is_git), ("~", false));
        assert_eq!(l.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["code", "notes"]);
        // Entering a git-marked child: known, no parent probe.
        app.pj_remote_go("~/code".into(), None);
        pump_until(&mut app, "code", |a| a.rs.picker.as_ref().unwrap().cwd == "~/code");
        assert!(!listing(&app).is_git);
        assert!(listing(&app).entries.iter().any(|e| e.name == "app" && e.is_git_repo));
        let before = be.calls.lock().len();
        app.pj_remote_go("~/code/app".into(), Some(true));
        pump_until(&mut app, "app", |a| a.rs.picker.as_ref().unwrap().cwd == "~/code/app");
        assert!(listing(&app).is_git);
        assert_eq!(be.calls.lock().len(), before + 1, "a known answer costs one request");
        // A typed path: git-ness is learned from the parent listing (one extra request).
        app.pj_remote_go("~/code/docs".into(), None);
        pump_until(&mut app, "docs", |a| a.rs.picker.as_ref().unwrap().cwd == "~/code/docs");
        assert!(!listing(&app).is_git, "docs is not marked git in its parent");
        app.pj_remote_go("~/code/app".into(), None);
        pump_until(&mut app, "app again", |a| a.rs.picker.as_ref().unwrap().cwd == "~/code/app");
        assert!(listing(&app).is_git, "typed path resolved via parent");
    }

    #[test]
    fn picker_ignores_stale_answers_and_keeps_the_listing_on_errors() {
        let (mut app, be) = scripted_app();
        app.pj_remote_pick(spec("box", "box.example"));
        pump_until(&mut app, "home", |a| a.rs.picker.as_ref().unwrap().listing.ready().is_some());
        let old = app.rs.picker.as_ref().unwrap().token;
        app.pj_remote_go("~/code".into(), None);
        let stale = RemoteListing { path: "~/notes".into(), entries: vec![], is_git: false };
        app.pj_remote_handle(old, Ok(stale));
        assert_eq!(app.rs.picker.as_ref().unwrap().cwd, "~", "an answer for a superseded navigation is dropped");
        pump_until(&mut app, "code", |a| a.rs.picker.as_ref().unwrap().cwd == "~/code");
        *be.fail.lock() = Some("host unreachable".into());
        app.pj_remote_go("~/code/app".into(), Some(true));
        pump_until(&mut app, "error", |a| a.rs.picker.as_ref().unwrap().error.is_some());
        let p = app.rs.picker.as_ref().unwrap();
        assert_eq!(p.cwd, "~/code", "failed navigation keeps the old directory");
        assert!(p.listing.ready().is_some());
        app.pj_remote_local();
        assert!(app.rs.picker.is_none());
    }

    #[test]
    fn opening_a_remote_project_closes_the_previous_connection_once_and_records_the_recent() {
        let (mut app, be) = scripted_app();
        let a = spec("a", "a.example");
        app.settings.remotes = vec![a.clone(), spec("b", "b.example")];
        app.pj_open_remote(a.clone(), "~/code/app".into()).unwrap();
        assert_eq!(app.remote, Some(a.clone()));
        assert!(app.rs.picker.is_none() && app.pj.browser.is_none());
        // Same host again: connection kept. Different project kind: closed.
        app.pj_open_remote(a.clone(), "~/code/docs".into()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!be.calls.lock().iter().any(|c| c.starts_with("close")));
        app.pj_open_remote(spec("b", "b.example"), "~/x".into()).unwrap();
        let t = Instant::now();
        while !be.calls.lock().iter().any(|c| c == "close a") {
            assert!(t.elapsed() < Duration::from_secs(5), "close not called: {:?}", be.calls.lock());
            std::thread::sleep(Duration::from_millis(10));
        }
        app.pj_record_remote_open("b", "~/x");
        assert_eq!((app.settings.recent[0].remote.as_deref(), app.settings.recent[0].path.as_str()), (Some("b"), "~/x"));
    }

    #[test]
    fn recent_of_a_deleted_remote_is_refused_and_forgotten() {
        let (mut app, _) = scripted_app();
        app.settings.recent = vec![crate::projects::RecentProject { path: "~/x".into(), opened_ms: 1, remote: Some("gone".into()) }];
        app.pj_open_recent(Some("gone"), "~/x");
        assert!(app.settings.recent.is_empty());
        assert!(app.status_msg.as_ref().is_some_and(|(m, _)| m.contains("no longer configured")));
        assert!(app.remote.is_none());
    }

    #[test]
    fn form_save_validates_edits_renames_with_recents_and_delete_cleans_up() {
        let (mut app, _) = scripted_app();
        app.rs_form_new();
        assert!(!app.rs_form_save());
        assert!(app.rs.form.as_ref().unwrap().error.as_deref().unwrap().contains("name"));
        {
            let f = &mut app.rs.form.as_mut().unwrap().f;
            f.name = "dev".into();
            f.host = "dev.example".into();
            f.port = "2200".into();
        }
        assert!(app.rs_form_save());
        assert!(app.rs.form.is_none());
        assert_eq!(app.settings.remotes[0].port, Some(2200));
        assert!(app.settings_dirty_at.is_some(), "persistence is debounced through settings_dirty_at");
        app.rs_form_new();
        {
            let f = &mut app.rs.form.as_mut().unwrap().f;
            f.name = "dev".into();
            f.host = "other.example".into();
        }
        assert!(!app.rs_form_save(), "duplicate name");
        app.settings.recent = vec![crate::projects::RecentProject { path: "~/p".into(), opened_ms: 1, remote: Some("dev".into()) }];
        app.rs_form_edit("dev");
        app.rs.form.as_mut().unwrap().f.name = "prod".into();
        assert!(app.rs_form_save());
        assert_eq!(app.settings.remotes.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["prod"]);
        assert_eq!(app.settings.recent[0].remote.as_deref(), Some("prod"), "recents follow a rename");
        app.rs_delete("prod");
        assert!(app.settings.remotes.is_empty());
    }

    #[test]
    fn test_connection_result_lands_on_the_tested_remote_and_stale_tokens_are_ignored() {
        let (mut app, be) = scripted_app();
        app.rs_test(TestTarget::Named("box".into()), spec("box", "box.example"));
        pump_until(&mut app, "test", |a| a.rs.status.contains_key("box"));
        assert!(matches!(&app.rs.status["box"], Status::Ok(i) if i.latency_ms == 12));
        *be.fail.lock() = Some("host key CHANGED".into());
        app.rs_test(TestTarget::Named("box".into()), spec("box", "box.example"));
        let stale = app.rs.test_token - 1;
        app.rs_test_done(stale, Ok(RemoteInfo { hostname: "x".into(), home: "".into(), git_version: "".into(), latency_ms: 1 }));
        assert!(app.rs.testing.is_some(), "stale answer must not complete the running test");
        pump_until(&mut app, "failure", |a| matches!(a.rs.status.get("box"), Some(Status::Failed(_))));
        assert!(app.rs.testing.is_none());
        app.rs_test(TestTarget::Form, spec("form", "f.example"));
        app.settings.remotes.clear();
        app.rs_form_new();
        pump_until(&mut app, "form test", |a| a.rs.testing.is_none());
        assert!(matches!(app.rs.form.as_ref().unwrap().test, Some(Err(_))));
    }

    #[test]
    fn import_adds_only_ticked_valid_hosts_without_duplicates_and_peers_sort_online_first() {
        let (mut app, _) = scripted_app();
        let h = |a: &str| SshHost { alias: a.into(), hostname: None, user: None, port: None, identity_file: None };
        app.settings.remotes = vec![spec("taken", "t.example")];
        app.rs.import = Some(ImportState { hosts: vec![(h("web"), true), (h("db"), false), (h("taken"), true), (h("bad host"), true)] });
        assert_eq!(app.rs_import_apply(), 1);
        assert_eq!(app.settings.remotes.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["taken", "web"]);
        assert_eq!(app.settings.remotes[1].host, "web", "the alias is kept so ~/.ssh/config applies");
        let p = |n: &str, online: bool, me: bool| TailnetPeer { name: n.into(), dns_name: format!("{n}.ts.net"), ips: vec![], os: "linux".into(), online, is_self: me };
        app.rs_peers_done(Ok(vec![p("zeta", false, false), p("alpha", true, false), p("me", true, true), p("beta", true, false)]));
        let Some(Loadable::Ready(v)) = &app.rs.peers else { panic!() };
        assert_eq!(v.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["alpha", "beta", "zeta"]);
        let peer = v[0].clone();
        app.rs_peer_use(&peer);
        let f = &app.rs.form.as_ref().unwrap().f;
        assert_eq!((f.name.as_str(), f.host.as_str()), ("alpha", "alpha.ts.net"));
        app.rs_peers_done(Err("`tailscale` was not found in PATH".into()));
        assert!(matches!(app.rs.peers, Some(Loadable::Failed(_))));
    }

    #[test]
    fn remote_mode_disables_edit_ask_and_runs_with_a_reason() {
        let mut app = app_with(Path::new(""), Some(spec("box", "box.example")), None);
        app.rail = RailTab::Commits;
        app.set_view_pref(ViewMode::Edit);
        assert_eq!(app.settings.view_mode, ViewMode::Diff, "Edit is refused, the stored preference stays");
        assert!(app.status_msg.as_ref().is_some_and(|(m, _)| m.contains("read-only")));
        app.start_ask();
        assert!(matches!(app.ask.phase, AskPhase::Idle));
        assert!(app.status_msg.as_ref().is_some_and(|(m, _)| m.contains("Ask AI")));
        app.ag_new_run_dialog();
        assert!(app.ag.new_run.is_none());
        assert!(crate::app::REMOTE_RO.contains("read-only"));
        // A local app keeps all three.
        let mut local = app_with(Path::new(""), None, None);
        local.ag_new_run_dialog();
        assert!(local.ag.new_run.is_some());
        local.set_view_pref(ViewMode::Edit);
        assert_eq!(local.settings.view_mode, ViewMode::Edit);
    }

    #[test]
    fn poll_failures_back_off_raise_the_banner_only_for_link_errors_and_recover() {
        let mut app = app_with(Path::new(""), Some(spec("box", "box.example")), None);
        app.wt.token = 7;
        app.wt_handle(7, Err("fatal: bad revision".into()));
        assert_eq!((app.rs.poll_fails, app.rs.conn_lost.is_some()), (1, false), "a git error backs off but is no outage");
        app.wt.token = 8;
        app.wt_handle(8, Err("host unreachable: check the name".into()));
        assert_eq!(app.rs.poll_fails, 2);
        assert!(app.rs.conn_lost.is_some());
        assert_eq!(crate::remote_backend::remote_poll_interval(app.rs.poll_fails), Duration::from_secs(40));
        app.wt.token = 9;
        app.wt_handle(9, Ok(crate::worktree::Summary::default()));
        assert_eq!(app.rs.poll_fails, 0, "a success resets the backoff");
    }

    // ------------------------------------------------------------ end to end through the fake ssh

    fn fake_ssh_env() {
        static E: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        E.get_or_init(|| {
            let d = tempfile::tempdir().unwrap();
            let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ct-core/tests/support/fake-ssh.sh");
            assert!(fake.exists(), "{fake:?}");
            std::env::set_var("CT_SSH_BIN", fake);
            std::env::set_var("XDG_RUNTIME_DIR", d.path());
            std::env::set_var("XDG_CACHE_HOME", d.path().join("cache"));
            d
        });
    }

    fn git(dir: &Path, args: &[&str]) {
        let o = std::process::Command::new("git").current_dir(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap();
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }

    #[test]
    fn open_remote_shows_commits_diff_blame_and_a_dirty_current_tab_through_fake_ssh() {
        fake_ssh_env();
        let d = tempfile::tempdir().unwrap();
        let p = d.path().canonicalize().unwrap();
        git(&p, &["init", "-q", "-b", "main"]);
        std::fs::write(p.join("a.txt"), "1\n2\n3\n").unwrap();
        git(&p, &["add", "."]);
        git(&p, &["commit", "-q", "-m", "first"]);
        std::fs::write(p.join("a.txt"), "1\n2\n3\n4\n").unwrap();
        git(&p, &["commit", "-qam", "second"]);
        let mut app = app_with(&p, Some(spec("fake", "fake-host")), None);
        pump_until(&mut app, "remote commits", |a| a.commits.len() == 2 && a.target.is_some());
        assert!(app.repo.as_ref().unwrap().is_remote());
        assert_eq!(app.commits[0].subject, "second");
        pump_until(&mut app, "commit diff", |a| a.diffset.ready().is_some_and(|s| s.files.len() == 1));
        assert_eq!(app.diffset.ready().unwrap().files[0].path, "a.txt");
        assert_eq!(app.settings.recent[0].remote.as_deref(), Some("fake"));
        // Files/search/editor requests are served through the repo handle: opening a file shows blame, never an editor.
        app.open_editor("a.txt", None);
        assert_eq!(app.centre, crate::app::Centre::Blame);
        assert!(app.editor.is_none());
        pump_until(&mut app, "remote blame", |a| a.blame.data.ready().is_some());
        // Dirty working tree: tracked edit + untracked file appear in Current.
        std::fs::write(p.join("a.txt"), "1\n2\n3\n4\n5\n").unwrap();
        std::fs::write(p.join("new.txt"), "n1\nn2\n").unwrap();
        app.request_status();
        pump_until(&mut app, "dirty summary", |a| a.wt.summary.as_ref().is_some_and(|s| s.dirty()));
        let s = app.wt.summary.clone().unwrap();
        assert_eq!((s.files, s.untracked), (2, 1), "{s:?}");
        app.rail = RailTab::Current;
        app.select_worktree();
        pump_until(&mut app, "worktree diff", |a| a.diffset.ready().is_some_and(|s| s.files.len() == 2));
        assert_eq!(app.target, Some(TargetSel::Worktree));
        app.open_file_diff("new.txt");
        pump_until(&mut app, "untracked remote diff", |a| a.prepared.ready().is_some_and(|p| p.model.file.path == "new.txt"));
    }

    #[test]
    fn an_unreachable_remote_fails_the_open_with_the_connection_banner() {
        fake_ssh_env();
        let mut app = app_with(Path::new("/tmp"), Some(spec("bad", "refused")), None);
        pump_until(&mut app, "failed open", |a| matches!(a.open_state, Loadable::Failed(_)));
        assert!(app.rs.conn_lost.as_deref().is_some_and(|e| e.contains("unreachable")), "{:?}", app.rs.conn_lost);
        app.remote = None;
        app.rs_retry();
        assert!(app.rs.conn_lost.is_none());
    }

    #[test]
    fn settings_remotes_section_fits_every_width_with_forms_open_and_does_not_grow() {
        use egui::{vec2, Rect};
        for size in [vec2(1440.0, 900.0), vec2(720.0, 760.0), vec2(640.0, 420.0)] {
            let ctx = egui::Context::default();
            let mut app = crate::ui::settings_window_tests::test_app(&ctx);
            app.settings.remotes = vec![spec("alpha", "alpha.tail1234.ts.net"), spec("beta-with-a-rather-long-name", "10.0.0.7")];
            app.rs.status.insert("alpha".into(), Status::Failed("host key not trusted yet: enable 'Trust new host key' or run `ssh <host>` once [Host key verification failed.]".into()));
            app.rs_form_new();
            app.rs.form.as_mut().unwrap().f.trust_new_host = true;
            app.rs.import = Some(ImportState { hosts: vec![(SshHost { alias: "web".into(), hostname: Some("web.example".into()), user: Some("me".into()), port: Some(22), identity_file: None }, false)] });
            app.rs_peers_done(Err("`tailscale` was not found in PATH: install Tailscale or enter the host manually".into()));
            app.settings_open_section(crate::settingspage::Section::Remotes);
            let mut heights = Vec::new();
            for _ in 0..8 {
                let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::pos2(0.0, 0.0), size)), ..Default::default() };
                let mut r = Rect::NOTHING;
                let _ = ctx.run(input, |ctx| {
                    if App::settings_wide(ctx) {
                        app.settings_nav_panel(ctx);
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        app.settings_page(ui);
                        r = ui.min_rect();
                    });
                });
                assert!(r.width() <= size.x + 0.5, "overflow at {size:?}: {r:?}");
                heights.push(r.height());
            }
            assert!(heights[3..].windows(2).all(|w| (w[0] - w[1]).abs() < 0.5), "layout grows per frame at {size:?}: {heights:?}");
        }
    }
}
