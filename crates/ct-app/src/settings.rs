//! ~/.config/codetrail/config.toml — behavioural settings (colours live in theme.toml).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub ui_font_size: f32,
    pub code_font_size: f32,
    pub tab_width: usize,
    pub split_view: bool,
    pub ask_preview: bool,
    pub ask_agent: String,
    pub ask_timeout_secs: u64,
    pub git_timeout_secs: u64,
    pub rail_width: f32,
    pub inspector_width: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ui_font_size: 13.0,
            code_font_size: 13.0,
            tab_width: 4,
            split_view: false,
            ask_preview: true,
            ask_agent: "claude".into(),
            ask_timeout_secs: 180,
            git_timeout_secs: 30,
            rail_width: 340.0,
            inspector_width: 360.0,
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
        self.tab_width = self.tab_width.clamp(1, 16);
        self.rail_width = self.rail_width.clamp(240.0, 640.0);
        self.inspector_width = self.inspector_width.clamp(260.0, 640.0);
        self.ask_timeout_secs = self.ask_timeout_secs.clamp(5, 3600);
        self.git_timeout_secs = self.git_timeout_secs.clamp(1, 600);
        self
    }

    pub fn load(path: &Path) -> (Settings, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(s) => match toml::from_str::<Settings>(&s) {
                Ok(v) => (v.sanitize(), None),
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
}
