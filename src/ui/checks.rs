//! The "Your system" checklist (D-019): what the distro must provide (README → Supported
//! desktops), each a pass or a fail with the reason. Shown in Settings and by
//! `murmur --check`. Pure functions of the probed [`SystemReport`].

use super::protocol::{SystemReport, TypingState};
use crate::portal::is_gnome;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Check {
    pub title: &'static str,
    pub ok: bool,
    /// Why it fails and what still works; empty when it passes.
    pub detail: String,
}

fn pass(title: &'static str) -> Check {
    Check {
        title,
        ok: true,
        detail: String::new(),
    }
}

fn fail(title: &'static str, detail: impl Into<String>) -> Check {
    Check {
        title,
        ok: false,
        detail: detail.into(),
    }
}

/// The first desktop in `XDG_CURRENT_DESKTOP`, for messages.
fn desktop_name(desktop: &str) -> &str {
    desktop
        .split(':')
        .map(str::trim)
        .find(|d| !d.is_empty())
        .unwrap_or("")
}

fn is_kde(desktop: &str) -> bool {
    desktop
        .split(':')
        .any(|d| d.trim().eq_ignore_ascii_case("KDE"))
}

const NO_PORTAL: &str = "Needs the desktop portal.";

/// The checklist, base system first (Settings shows it in two columns of four).
pub fn list(r: &SystemReport, typing: &TypingState) -> Vec<Check> {
    let gnome = is_gnome(Some(&r.desktop));
    let kde = is_kde(&r.desktop);

    let wayland = if r.wayland {
        pass("Wayland session")
    } else {
        fail(
            "Wayland session",
            "Murmur needs Wayland. X11 isn't supported.",
        )
    };

    // Only Plasma's major version is visible from the sandbox: Plasma 6.0 passes here, and
    // its missing EIS support shows on the Typing line after the first attempt. GNOME's
    // version shows through its portal: GlobalShortcuts arrived in GNOME 48.
    const DESKTOP: &str = "Plasma 6.1+ or GNOME 48+";
    let desktop = match (kde, r.kde_version) {
        (true, Some(v)) if v < 6 => fail(
            DESKTOP,
            format!("Found Plasma {v}. Murmur needs Plasma 6.1 or later."),
        ),
        (true, _) => pass(DESKTOP),
        _ if gnome && r.portal && r.global_shortcuts.is_none() => fail(
            DESKTOP,
            "Found GNOME 47 or older. Murmur needs GNOME 48 or later.",
        ),
        _ if gnome => pass(DESKTOP),
        _ => match desktop_name(&r.desktop) {
            "" => fail(DESKTOP, "Couldn't tell which desktop this is."),
            other => fail(
                DESKTOP,
                format!("Found {other}, which Murmur doesn't support yet."),
            ),
        },
    };

    let portal = if r.portal {
        pass("Desktop portal")
    } else {
        fail(
            "Desktop portal",
            "xdg-desktop-portal isn't running. Install it with your desktop's backend.",
        )
    };

    const AUDIO: &str = "PipeWire or PulseAudio";
    let audio = if !r.audio_server {
        fail(AUDIO, "No audio server found. Murmur can't hear you.")
    } else if r.microphones == 0 {
        fail(AUDIO, "No microphone found.")
    } else {
        pass(AUDIO)
    };

    const SHORTCUT: &str = "Global shortcut";
    let shortcut = match r.global_shortcuts {
        Some(_) => pass(SHORTCUT),
        None if !r.portal => fail(SHORTCUT, NO_PORTAL),
        None if gnome => fail(
            SHORTCUT,
            "Needs GNOME 48 or later. Click the tray icon to dictate.",
        ),
        None => fail(
            SHORTCUT,
            "The desktop portal has no global shortcuts. Click the tray icon to dictate.",
        ),
    };

    const TYPING: &str = "Typing";
    let typing = match (r.remote_desktop, typing) {
        (_, TypingState::Unavailable(why)) => fail(TYPING, why.clone()),
        (None, _) if !r.portal => fail(TYPING, NO_PORTAL),
        (None, _) => fail(TYPING, "The desktop portal can't type for apps."),
        (Some(v), _) if v < 2 => fail(
            TYPING,
            "xdg-desktop-portal is too old to type (needs 1.18 or later).",
        ),
        _ if !r.remote_desktop_keyboard => fail(
            TYPING,
            "The desktop portal offers no keyboard to type with.",
        ),
        (_, TypingState::Denied) => fail(
            TYPING,
            "Permission denied. Dictate again to be asked once more.",
        ),
        _ => pass(TYPING),
    };

    const CLIPBOARD: &str = "Clipboard";
    let clipboard = match r.clipboard {
        Some(_) => pass(CLIPBOARD),
        None if !r.portal => fail(CLIPBOARD, NO_PORTAL),
        None => fail(
            CLIPBOARD,
            "Characters missing from your keyboard layout will be skipped.",
        ),
    };

    const TRAY: &str = "Tray icon";
    let tray = if r.tray {
        pass(TRAY)
    } else if gnome {
        fail(
            TRAY,
            "Install the AppIndicator extension to see it. Dictation works without it.",
        )
    } else {
        fail(TRAY, "No system tray found. Dictation works without it.")
    };

    vec![
        wayland, desktop, portal, audio, shortcut, typing, clipboard, tray,
    ]
}

/// "All good", "1 problem", "3 problems".
pub fn summary(checks: &[Check]) -> String {
    match checks.iter().filter(|c| !c.ok).count() {
        0 => "All good".into(),
        1 => "1 problem".into(),
        n => format!("{n} problems"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_kde() -> SystemReport {
        SystemReport {
            wayland: true,
            desktop: "KDE".into(),
            kde_version: Some(6),
            portal: true,
            global_shortcuts: Some(2),
            remote_desktop: Some(2),
            remote_desktop_keyboard: true,
            clipboard: Some(1),
            audio_server: true,
            microphones: 1,
            tray: true,
        }
    }

    fn failing(checks: &[Check]) -> Vec<(&'static str, &str)> {
        checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| (c.title, c.detail.as_str()))
            .collect()
    }

    #[test]
    fn healthy_plasma_passes_everything() {
        let checks = list(&healthy_kde(), &TypingState::Granted);
        assert_eq!(checks.len(), 8);
        assert!(failing(&checks).is_empty(), "{checks:?}");
        assert!(checks.iter().all(|c| c.detail.is_empty()));
        assert_eq!(summary(&checks), "All good");
    }

    #[test]
    fn gnome_without_appindicator_fails_only_the_tray() {
        let r = SystemReport {
            desktop: "ubuntu:GNOME".into(),
            kde_version: None,
            global_shortcuts: Some(1),
            tray: false,
            ..healthy_kde()
        };
        let checks = list(&r, &TypingState::Unknown);
        let f = failing(&checks);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].0, "Tray icon");
        assert!(f[0].1.contains("AppIndicator"), "{}", f[0].1);
        assert_eq!(summary(&checks), "1 problem");
    }

    #[test]
    fn old_gnome_has_no_global_shortcut() {
        let r = SystemReport {
            desktop: "GNOME".into(),
            global_shortcuts: None,
            ..healthy_kde()
        };
        let f = failing(&list(&r, &TypingState::Unknown))
            .into_iter()
            .map(|(t, d)| format!("{t}: {d}"))
            .collect::<Vec<_>>();
        // Both lines say why, and agree.
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f[0].contains("GNOME 47 or older"), "{f:?}");
        assert!(
            f[1].starts_with("Global shortcut") && f[1].contains("GNOME 48"),
            "{f:?}"
        );
    }

    #[test]
    fn sway_and_x11() {
        let r = SystemReport {
            wayland: false,
            desktop: "sway".into(),
            kde_version: None,
            global_shortcuts: None,
            ..healthy_kde()
        };
        let checks = list(&r, &TypingState::Unknown);
        let titles: Vec<_> = failing(&checks).iter().map(|f| f.0).collect();
        assert_eq!(
            titles,
            [
                "Wayland session",
                "Plasma 6.1+ or GNOME 48+",
                "Global shortcut"
            ]
        );
        assert!(
            checks[1].detail.contains("Found sway"),
            "{}",
            checks[1].detail
        );
        assert_eq!(summary(&checks), "3 problems");
    }

    #[test]
    fn plasma_5_fails_the_desktop_check() {
        let r = SystemReport {
            kde_version: Some(5),
            ..healthy_kde()
        };
        let checks = list(&r, &TypingState::Unknown);
        assert_eq!(failing(&checks).len(), 1);
        assert!(
            checks[1].detail.contains("Plasma 5"),
            "{}",
            checks[1].detail
        );
    }

    #[test]
    fn typing_reports_the_real_failure() {
        let r = healthy_kde();
        let why = "The desktop doesn't support typing for apps";
        let c = &list(&r, &TypingState::Unavailable(why.into()))[5];
        assert_eq!((c.title, c.ok, c.detail.as_str()), ("Typing", false, why));
        let c = &list(&r, &TypingState::Denied)[5];
        assert!(!c.ok && c.detail.contains("denied"), "{c:?}");
        let old = SystemReport {
            remote_desktop: Some(1),
            ..healthy_kde()
        };
        let c = &list(&old, &TypingState::Unknown)[5];
        assert!(!c.ok && c.detail.contains("1.18"), "{c:?}");
        let no_keyboard = SystemReport {
            remote_desktop_keyboard: false,
            ..healthy_kde()
        };
        let c = &list(&no_keyboard, &TypingState::Unknown)[5];
        assert!(!c.ok && c.detail.contains("no keyboard"), "{c:?}");
        // Not asked yet is not a problem.
        assert!(list(&r, &TypingState::Unknown)[5].ok);
    }

    #[test]
    fn no_portal_explains_every_portal_feature() {
        let r = SystemReport {
            portal: false,
            global_shortcuts: None,
            remote_desktop: None,
            clipboard: None,
            ..healthy_kde()
        };
        let checks = list(&r, &TypingState::Unknown);
        let f = failing(&checks);
        assert_eq!(
            f.iter().map(|f| f.0).collect::<Vec<_>>(),
            ["Desktop portal", "Global shortcut", "Typing", "Clipboard"]
        );
        assert!(f[1..].iter().all(|f| f.1 == NO_PORTAL), "{f:?}");
    }

    #[test]
    fn audio_server_and_microphone() {
        let r = SystemReport {
            audio_server: false,
            microphones: 0,
            ..healthy_kde()
        };
        assert!(
            list(&r, &TypingState::Unknown)[3]
                .detail
                .contains("No audio server")
        );
        let r = SystemReport {
            microphones: 0,
            ..healthy_kde()
        };
        assert_eq!(
            list(&r, &TypingState::Unknown)[3].detail,
            "No microphone found."
        );
    }

    #[test]
    fn unknown_desktop() {
        let r = SystemReport {
            desktop: String::new(),
            kde_version: None,
            ..healthy_kde()
        };
        assert_eq!(
            list(&r, &TypingState::Unknown)[1].detail,
            "Couldn't tell which desktop this is."
        );
    }
}
