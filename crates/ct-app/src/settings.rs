//! ~/.config/codetrail/config.toml — behavioural settings (colours live in theme.toml).

use crate::agents::Backend;
use crate::projects::{normalize, RecentProject, MAX_RECENT};
use ct_core::remote::RemoteSpec;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Main pane mode the user picked in the Diff | Blame | Edit switch. Selection changes never alter it.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    #[default]
    Diff,
    Blame,
    Edit,
}

pub const MAX_REMOTES: usize = 32;

/// Drops invalid and duplicate-name remotes (first wins) and caps the list.
pub fn sanitize_remotes(list: Vec<RemoteSpec>) -> Vec<RemoteSpec> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<RemoteSpec> = list.into_iter().filter(|r| r.validate().is_ok() && seen.insert(r.name.clone())).collect();
    out.truncate(MAX_REMOTES);
    out
}

/// What the app opens when started without a path argument.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Startup {
    /// Reopen the most recent project that still exists.
    #[default]
    Last,
    /// Always start at the project picker.
    Picker,
}

/// `[agent]` table: the one backend/model/thinking selection shared by Ask AI and new runs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentCfg {
    pub backend: Backend,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub max_concurrent: usize,
    /// Per-run RSS limit in MiB; 0 = unlimited.
    pub max_rss_mb: u64,
    /// Per-run wall-clock limit in minutes; 0 = unlimited.
    pub max_runtime_min: u64,
    /// Budget over the RSS of all running runs, MiB.
    pub max_total_rss_mb: u64,
}

impl Default for AgentCfg {
    fn default() -> Self {
        AgentCfg { backend: Backend::Pi, provider: None, model: None, thinking: None, max_concurrent: 3, max_rss_mb: 3072, max_runtime_min: 120, max_total_rss_mb: 6144 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub ui_font_size: f32,
    pub code_font_size: f32,
    /// Whole-UI zoom (text, panels, icons); Ctrl+Plus / Ctrl+Minus / Ctrl+0 change it too.
    pub zoom: f32,
    pub tab_width: usize,
    pub split_view: bool,
    pub ask_preview: bool,
    pub agent: AgentCfg,
    /// Kill switch: account ids listed here are shown as unavailable and never used.
    pub accounts_disabled: Vec<String>,
    pub ask_timeout_secs: u64,
    pub git_timeout_secs: u64,
    pub rail_width: f32,
    /// Share of the rail height given to the changed-files list; `None` = default rule.
    pub files_split: Option<f32>,
    pub inspector_width: f32,
    pub view_mode: ViewMode,
    /// Wrap long lines in the editor.
    pub wrap: bool,
    pub startup: Startup,
    /// Show app/run memory and CPU in the status bar.
    pub status_monitor: bool,
    /// SSH remotes (`[[remotes]]`), unique names, at most [`MAX_REMOTES`].
    pub remotes: Vec<RemoteSpec>,
    /// Recently opened projects, most recent first (at most 12).
    pub recent: Vec<RecentProject>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ui_font_size: 13.0,
            code_font_size: 13.0,
            zoom: 1.0,
            tab_width: 4,
            split_view: false,
            ask_preview: true,
            agent: AgentCfg::default(),
            accounts_disabled: Vec::new(),
            ask_timeout_secs: 180,
            git_timeout_secs: 30,
            rail_width: 340.0,
            files_split: None,
            inspector_width: 360.0,
            view_mode: ViewMode::Diff,
            wrap: false,
            startup: Startup::Last,
            status_monitor: true,
            remotes: Vec::new(),
            recent: Vec::new(),
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        crate::theme::config_dir().join("config.toml")
    }

    /// Clamp values that would make the UI unusable.
    pub fn sanitize(mut self) -> Self {
        self.ui_font_size = self.ui_font_size.clamp(9.0, 24.0);
        self.code_font_size = self.code_font_size.clamp(9.0, 28.0);
        self.zoom = clamp_zoom(self.zoom);
        self.tab_width = self.tab_width.clamp(1, 16);
        self.rail_width = self.rail_width.clamp(240.0, 640.0);
        self.files_split = self.files_split.map(crate::railsplit::clamp);
        self.inspector_width = self.inspector_width.clamp(260.0, 640.0);
        self.ask_timeout_secs = self.ask_timeout_secs.clamp(5, 3600);
        self.git_timeout_secs = self.git_timeout_secs.clamp(1, 600);
        self.agent.max_concurrent = self.agent.max_concurrent.clamp(1, 8);
        self.agent.max_rss_mb = self.agent.max_rss_mb.min(262_144);
        self.agent.max_runtime_min = self.agent.max_runtime_min.min(1440);
        self.agent.max_total_rss_mb = self.agent.max_total_rss_mb.clamp(512, 262_144);
        self.remotes = sanitize_remotes(std::mem::take(&mut self.remotes));
        self.recent.truncate(MAX_RECENT);
        self
    }

    pub fn load(path: &Path) -> (Settings, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(s) => match toml::from_str::<Settings>(&s) {
                Ok(mut v) => {
                    v.recent = normalize(std::mem::take(&mut v.recent), &|p| p.is_dir());
                    (v.sanitize(), None)
                }
                Err(e) => (
                    Settings::default(),
                    Some(format!("config.toml is invalid, using defaults ({})", e.to_string().lines().next().unwrap_or(""))),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Settings::default(), None),
            Err(e) => (Settings::default(), Some(format!("cannot read config.toml: {e}"))),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let s = toml::to_string_pretty(self).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        crate::fsutil::atomic_write(path, s.as_bytes())
    }
}

/// Zoom in 10% steps (egui's Ctrl+Plus/Minus step), 50%..300%; NaN/inf -> 100%.
pub fn clamp_zoom(z: f32) -> f32 {
    if z.is_finite() { (z * 10.0).round().clamp(5.0, 30.0) / 10.0 } else { 1.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_partial_files() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        let mut s = Settings::default();
        s.code_font_size = 15.0;
        s.split_view = true;
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p), (s, None));
        std::fs::write(&p, "ui_font_size = 16.0\n").unwrap();
        let (l, b) = Settings::load(&p);
        assert!(b.is_none());
        assert_eq!(l.ui_font_size, 16.0);
        assert_eq!(l.code_font_size, 13.0);
    }

    #[test]
    fn remotes_round_trip_and_invalid_or_duplicate_entries_are_dropped() {
        use ct_core::remote::Transport;
        let spec = |name: &str, host: &str| RemoteSpec { name: name.into(), host: host.into(), user: Some("me".into()), port: Some(2222), identity_file: Some("/k/id".into()), transport: Transport::TailscaleNc, trust_new_host: true };
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        let mut s = Settings::default();
        s.remotes = vec![spec("a", "a.example"), RemoteSpec { user: None, port: None, identity_file: None, transport: Transport::Ssh, trust_new_host: false, ..spec("b", "10.0.0.2") }];
        s.save(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("[[remotes]]") && text.contains("transport = \"tailscale_nc\""), "{text}");
        assert_eq!(Settings::load(&p), (s.clone(), None));
        let mut bad = s.clone();
        bad.remotes = vec![spec("a", "ok.example"), spec("a", "dup.example"), spec("x", "-oProxyCommand=evil"), spec("y", "has space")];
        for i in 0..40 {
            bad.remotes.push(spec(&format!("n{i}"), "h.example"));
        }
        let clean = bad.sanitize();
        assert_eq!(clean.remotes.len(), MAX_REMOTES);
        assert_eq!(clean.remotes[0].host, "ok.example", "first of two same-named remotes wins");
        assert!(clean.remotes.iter().all(|r| r.name != "x" && r.name != "y"));
    }

    #[test]
    fn legacy_recents_without_remote_field_load_and_local_entries_stay_string_compatible() {
        let d = tempfile::tempdir().unwrap();
        let live = d.path().join("live");
        std::fs::create_dir(&live).unwrap();
        let p = d.path().join("config.toml");
        std::fs::write(&p, format!("[[recent]]\npath = \"{}\"\nopened_ms = 5\n\n[[recent]]\npath = \"/srv/app\"\nopened_ms = 7\nremote = \"box\"\n", live.display())).unwrap();
        let (s, b) = Settings::load(&p);
        assert!(b.is_none());
        assert_eq!(s.recent.len(), 2);
        assert_eq!(s.recent[0].remote.as_deref(), Some("box"));
        assert_eq!(s.recent[1].remote, None);
        s.save(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.matches("remote =").count(), 1, "local entries do not grow a remote key: {text}");
    }

    #[test]
    fn corrupt_file_rolls_back_to_defaults_with_banner() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "ui_font_size = \"huge\"").unwrap();
        let (s, b) = Settings::load(&p);
        assert_eq!(s, Settings::default());
        assert!(b.unwrap().contains("invalid"));
    }

    #[test]
    fn absurd_values_are_clamped() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "ui_font_size = 400.0\ntab_width = 0\n").unwrap();
        let (s, _) = Settings::load(&p);
        assert_eq!(s.ui_font_size, 24.0);
        assert_eq!(s.tab_width, 1);
    }

    #[test]
    fn recent_projects_round_trip_and_missing_paths_are_dropped_on_load() {
        let d = tempfile::tempdir().unwrap();
        let live = d.path().join("live");
        std::fs::create_dir(&live).unwrap();
        let p = d.path().join("config.toml");
        let mut s = Settings::default();
        s.recent = vec![RecentProject::local("/gone/for/good".into(), 9), RecentProject::local(live.to_string_lossy().into_owned(), 5)];
        s.save(&p).unwrap();
        let (l, b) = Settings::load(&p);
        assert!(b.is_none());
        assert_eq!(l.recent.len(), 1);
        assert_eq!(l.recent[0].opened_ms, 5);
    }

    #[test]
    fn files_split_round_trips_clamps_and_defaults_to_none() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        assert_eq!(Settings::default().files_split, None);
        let mut s = Settings::default();
        s.files_split = Some(0.6);
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p).0.files_split, Some(0.6));
        s.files_split = Some(0.99);
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p).0.files_split, Some(crate::railsplit::MAX_FRAC));
        s.files_split = None;
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p).0.files_split, None);
    }

    #[test]
    fn view_mode_round_trips_and_bad_value_falls_back_to_defaults() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        assert_eq!(Settings::default().view_mode, ViewMode::Diff);
        let mut s = Settings::default();
        s.view_mode = ViewMode::Blame;
        s.save(&p).unwrap();
        assert!(std::fs::read_to_string(&p).unwrap().contains("view_mode = \"blame\""));
        assert_eq!(Settings::load(&p).0.view_mode, ViewMode::Blame);
        std::fs::write(&p, "view_mode = \"sideways\"\n").unwrap();
        let (s, warn) = Settings::load(&p);
        assert_eq!(s.view_mode, ViewMode::Diff);
        assert!(warn.is_some());
    }
}
