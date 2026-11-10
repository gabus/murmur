//! `$XDG_CONFIG_HOME/murmur/config.toml`. Missing keys get defaults; an
//! unreadable file is reported and replaced by defaults (never fatal).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::models;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Model id from the registry.
    pub model: String,
    /// Unload the model after this many idle minutes (0 = never).
    pub idle_unload_minutes: u32,
    /// The first-run setup page has been completed.
    pub setup_done: bool,
    /// Microphone name ("" = system default).
    pub microphone: String,
    /// Pause (seconds) after which a sentence is finished.
    pub pause_secs: f32,
    /// Start murmur at login (requested through the Background portal).
    pub autostart: bool,
    /// Keys used to paste characters missing from the layout:
    /// "ctrl+v" (default) or "ctrl+shift+v" (terminals).
    pub paste_keys: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: models::DEFAULT_MODEL.to_owned(),
            idle_unload_minutes: 10,
            setup_done: false,
            microphone: String::new(),
            pause_secs: 0.8,
            autostart: false,
            paste_keys: "ctrl+v".into(),
        }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        crate::paths::config_dir().map(|d| d.join("config.toml"))
    }

    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<Self>(&text)
                .map(Self::sanitized)
                .unwrap_or_else(|e| {
                    warn!(error = %e, path = %path.display(), "invalid config, using defaults");
                    Self::default()
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                warn!(error = %e, path = %path.display(), "cannot read config, using defaults");
                Self::default()
            }
        }
    }

    /// Replaces out-of-range values from a hand-edited file with defaults.
    fn sanitized(mut self) -> Self {
        let d = Self::default();
        if !self.pause_secs.is_finite() || !(0.3..=3.0).contains(&self.pause_secs) {
            self.pause_secs = d.pause_secs;
        }
        self.idle_unload_minutes = self.idle_unload_minutes.min(24 * 60);
        self
    }

    /// Writes atomically (temp file + rename).
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let dir = std::env::temp_dir().join(format!("murmur-cfg-{}", std::process::id()));
        let path = dir.join("config.toml");
        assert_eq!(Config::load_from(&path), Config::default());
        let c = Config {
            model: "moonshine-tiny-en".into(),
            idle_unload_minutes: 3,
            setup_done: true,
            microphone: "USB mic".into(),
            pause_secs: 1.2,
            autostart: true,
            paste_keys: "ctrl+shift+v".into(),
        };
        c.save_to(&path).expect("save");
        assert_eq!(Config::load_from(&path), c);
        std::fs::write(&path, "model = \"x\"\n").expect("write");
        let partial = Config::load_from(&path);
        assert_eq!(partial.model, "x");
        assert_eq!(partial.idle_unload_minutes, 10);
        std::fs::write(&path, "pause_secs = nan\n").expect("write");
        assert_eq!(Config::load_from(&path).pause_secs, 0.8);
        std::fs::write(&path, "this is not toml").expect("write");
        assert_eq!(Config::load_from(&path), Config::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
