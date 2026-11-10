//! XDG base directories. Inside Flatpak these env vars point into
//! `~/.var/app/<app-id>/`; outside, the XDG spec defaults apply.

use std::path::PathBuf;

/// Application ID, also used for host-app portal registration.
pub const APP_ID: &str = "io.github.gabus.Murmur";

fn xdg(var: &str, default_rel: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(default_rel)))
}

/// `$XDG_DATA_HOME/murmur` (models live below it).
pub fn data_dir() -> Option<PathBuf> {
    xdg("XDG_DATA_HOME", ".local/share").map(|p| p.join("murmur"))
}

/// `$XDG_STATE_HOME/murmur` (portal restore tokens).
pub fn state_dir() -> Option<PathBuf> {
    xdg("XDG_STATE_HOME", ".local/state").map(|p| p.join("murmur"))
}

/// `$XDG_CONFIG_HOME/murmur` (config.toml).
pub fn config_dir() -> Option<PathBuf> {
    xdg("XDG_CONFIG_HOME", ".config").map(|p| p.join("murmur"))
}

/// `$XDG_DATA_HOME/murmur/models`.
pub fn models_dir() -> Option<PathBuf> {
    data_dir().map(|p| p.join("models"))
}
