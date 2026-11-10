//! Shared xdg-desktop-portal helpers.

use tokio::sync::OnceCell;
use tracing::{info, warn};

/// Host apps (not Flatpak) must register their app ID before the first portal call.
/// Done once per process; failure is logged and ignored.
pub async fn register_host_app() {
    static REGISTERED: OnceCell<()> = OnceCell::const_new();
    REGISTERED
        .get_or_init(|| async {
            if ashpd::is_sandboxed() {
                return;
            }
            let res = match ashpd::AppID::try_from(crate::paths::APP_ID) {
                Ok(id) => ashpd::register_host_app(id).await,
                Err(e) => Err(e),
            };
            match res {
                Ok(()) => info!(app_id = crate::paths::APP_ID, "registered host app"),
                Err(e) => warn!("register_host_app failed (continuing): {e}"),
            }
        })
        .await;
}

/// Whether `XDG_CURRENT_DESKTOP` (a colon-separated list such as `ubuntu:GNOME`) names GNOME.
/// xdg-desktop-portal picks its backend from the same variable, which Flatpak passes into the
/// sandbox; GNOME's backend and shell differ in places (D-016, the clipboard).
pub fn is_gnome(current_desktop: Option<&str>) -> bool {
    current_desktop.is_some_and(|desktops| {
        desktops
            .split(':')
            .any(|d| d.trim().eq_ignore_ascii_case("GNOME"))
    })
}

/// [`is_gnome`] for this process's `XDG_CURRENT_DESKTOP`.
pub fn on_gnome() -> bool {
    is_gnome(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnome_is_found_in_the_desktop_list() {
        for gnome in [
            "GNOME",
            "gnome",
            "ubuntu:GNOME",
            "GNOME-Classic:GNOME",
            "Pop:GNOME ",
        ] {
            assert!(is_gnome(Some(gnome)), "{gnome}");
        }
        for other in [
            "KDE",
            "Hyprland",
            "sway",
            "GNOME-Flashback",
            "",
            "X-Cinnamon",
        ] {
            assert!(!is_gnome(Some(other)), "{other}");
        }
        assert!(!is_gnome(None));
    }
}
