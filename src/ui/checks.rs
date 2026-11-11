//! The "Your system" checklist (D-019, D-022): the components Murmur needs from the host, each
//! found or missing, with what it's for and what to install. Shown in Settings and by
//! `murmur --check`. Pure functions of the probed [`SystemReport`] and the typing state.
//!
//! Desktops aren't named as requirements: any desktop whose portal backends and compositor
//! provide these components works.

use super::protocol::{SystemReport, TypingState};
use crate::portal::is_gnome;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Check {
    /// The component, as a distro packages or names it.
    pub title: &'static str,
    /// What Murmur uses it for.
    pub purpose: &'static str,
    pub ok: bool,
    /// Murmur works without it (with less).
    pub optional: bool,
    /// Not known yet: still probing, or only known once Murmur uses it.
    pub pending: bool,
    /// What's wrong and what to install; empty when it's found. For a pending line, when it
    /// will be known.
    pub detail: String,
}

/// (title, purpose, optional), in display order: system services on the left, what the
/// portal backends and the compositor provide on the right.
const LINES: [(&str, &str, bool); 10] = [
    ("Wayland compositor", "Murmur runs on Wayland only", false),
    (
        "xdg-desktop-portal 1.18+",
        "Connects Murmur to the desktop",
        false,
    ),
    ("PipeWire or PulseAudio", "Microphone", false),
    ("System tray host", "Tray icon (optional)", true),
    ("Notification portal", "Error messages (optional)", true),
    ("GlobalShortcuts portal", "The start/stop key", false),
    ("RemoteDesktop portal", "Permission to type", false),
    ("Input emulation (libei)", "Typing into apps", false),
    ("Clipboard portal", "Characters not on your keyboard", false),
    ("Background portal", "Start at login (optional)", true),
];

fn line(i: usize) -> Check {
    let (title, purpose, optional) = LINES[i];
    Check {
        title,
        purpose,
        optional,
        ..Default::default()
    }
}

fn pass(i: usize) -> Check {
    Check {
        ok: true,
        ..line(i)
    }
}

fn fail(i: usize, detail: impl Into<String>) -> Check {
    Check {
        detail: detail.into(),
        ..line(i)
    }
}

fn wait(i: usize, detail: impl Into<String>) -> Check {
    Check {
        pending: true,
        detail: detail.into(),
        ..line(i)
    }
}

/// The checklist before the probe has answered: every line pending, same size as the result.
pub fn pending() -> Vec<Check> {
    (0..LINES.len()).map(|i| wait(i, "")).collect()
}

const NO_PORTAL: &str = "Needs xdg-desktop-portal.";

/// A portal interface the backend may or may not provide.
fn portal_line(r: &SystemReport, i: usize, version: Option<u32>, missing: &str) -> Check {
    match version {
        Some(_) => pass(i),
        None if !r.portal => fail(i, NO_PORTAL),
        None => fail(i, missing),
    }
}

/// The checklist, in [`LINES`] order.
pub fn list(r: &SystemReport, typing: &TypingState) -> Vec<Check> {
    let gnome = is_gnome(Some(&r.desktop));

    let wayland = if r.wayland {
        pass(0)
    } else {
        fail(0, "No Wayland session found. X11 isn't supported.")
    };

    let portal = if !r.portal {
        fail(1, "Not running. Install xdg-desktop-portal.")
    } else if r.remote_desktop.is_some_and(|v| v < 2) {
        fail(1, "Too old to type: Murmur needs 1.18 or later.")
    } else {
        pass(1)
    };

    let audio = if !r.audio_server {
        fail(
            2,
            "Not found. Install PipeWire (with pipewire-pulse) or PulseAudio.",
        )
    } else if r.microphones == 0 {
        fail(2, "Running, but no microphone found.")
    } else {
        pass(2)
    };

    let tray = if r.tray {
        pass(3)
    } else if gnome {
        fail(3, "Install the GNOME AppIndicator extension.")
    } else {
        fail(3, "Your panel has no system tray.")
    };

    let notification = portal_line(
        r,
        4,
        r.notification,
        "Your portal backend doesn't provide it.",
    );

    let shortcuts = portal_line(
        r,
        5,
        r.global_shortcuts,
        "Your portal backend doesn't provide it (xdg-desktop-portal-kde, -gnome 48+ and \
         -hyprland do). Click the tray icon to dictate meanwhile.",
    );

    let remote = match (r.remote_desktop, typing) {
        (None, _) => portal_line(
            r,
            6,
            None,
            "Your portal backend doesn't provide it (xdg-desktop-portal-kde and -gnome do).",
        ),
        (Some(_), _) if !r.remote_desktop_keyboard => {
            fail(6, "Your portal backend offers no keyboard.")
        }
        (Some(_), TypingState::Denied) => fail(
            6,
            "You didn't allow typing. Dictate again to be asked once more.",
        ),
        (Some(_), _) => pass(6),
    };

    // Only known once Murmur connects: at startup when typing was allowed before, or after
    // the user allows it.
    let emulation = match (r.remote_desktop, typing) {
        (_, TypingState::Granted) => pass(7),
        (_, TypingState::Unavailable(why)) => fail(7, why.clone()),
        (None, _) => fail(7, "Needs the RemoteDesktop portal."),
        _ => wait(7, "Checked once you allow typing."),
    };

    let clipboard = portal_line(
        r,
        8,
        r.clipboard,
        "Your portal backend doesn't provide it. Characters missing from your keyboard \
         layout will be skipped.",
    );

    let background = portal_line(
        r,
        9,
        r.background,
        "Your portal backend doesn't provide it. Start at login won't work.",
    );

    vec![
        wayland,
        portal,
        audio,
        tray,
        notification,
        shortcuts,
        remote,
        emulation,
        clipboard,
        background,
    ]
}

/// "All found", "2 missing", or "All needed parts found" when only optional ones are missing.
pub fn summary(checks: &[Check]) -> String {
    let missing = |optional: bool| {
        checks
            .iter()
            .filter(|c| !c.ok && !c.pending && c.optional == optional)
            .count()
    };
    match (missing(false), missing(true)) {
        (0, 0) => "All found".into(),
        (0, _) => "All needed parts found".into(),
        (n, _) => format!("{n} missing"),
    }
}

/// Whether everything Murmur needs is there (optional parts and pending lines aside).
pub fn needed_found(checks: &[Check]) -> bool {
    checks.iter().all(|c| c.ok || c.optional || c.pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kde() -> SystemReport {
        SystemReport {
            wayland: true,
            desktop: "KDE".into(),
            kde_version: Some(6),
            portal: true,
            global_shortcuts: Some(2),
            remote_desktop: Some(2),
            remote_desktop_keyboard: true,
            clipboard: Some(1),
            notification: Some(2),
            background: Some(2),
            audio_server: true,
            microphones: 1,
            tray: true,
        }
    }

    fn missing(checks: &[Check]) -> Vec<(&'static str, &str)> {
        checks
            .iter()
            .filter(|c| !c.ok && !c.pending)
            .map(|c| (c.title, c.detail.as_str()))
            .collect()
    }

    #[test]
    fn pending_has_the_same_lines() {
        let found: Vec<_> = list(&kde(), &TypingState::Granted)
            .iter()
            .map(|c| c.title)
            .collect();
        let waiting: Vec<_> = pending().iter().map(|c| c.title).collect();
        assert_eq!(found, waiting);
        assert!(pending().iter().all(|c| c.pending && !c.ok));
    }

    #[test]
    fn plasma_with_typing_allowed_has_everything() {
        let checks = list(&kde(), &TypingState::Granted);
        assert_eq!(checks.len(), 10);
        assert!(missing(&checks).is_empty(), "{checks:?}");
        assert!(checks.iter().all(|c| c.ok && c.detail.is_empty()));
        assert_eq!(summary(&checks), "All found");
        assert!(needed_found(&checks));
    }

    #[test]
    fn input_emulation_is_known_only_after_connecting() {
        let checks = list(&kde(), &TypingState::Unknown);
        let c = &checks[7];
        assert!(c.pending && !c.ok, "{c:?}");
        assert_eq!(c.detail, "Checked once you allow typing.");
        assert_eq!(summary(&checks), "All found");
        let why = "The desktop doesn't support typing for apps";
        let c = &list(&kde(), &TypingState::Unavailable(why.into()))[7];
        assert_eq!((c.ok, c.pending, c.detail.as_str()), (false, false, why));
    }

    #[test]
    fn gnome_without_appindicator_misses_only_an_optional_part() {
        let r = SystemReport {
            desktop: "ubuntu:GNOME".into(),
            global_shortcuts: Some(1),
            tray: false,
            ..kde()
        };
        let checks = list(&r, &TypingState::Granted);
        assert_eq!(
            missing(&checks),
            [(
                "System tray host",
                "Install the GNOME AppIndicator extension."
            )]
        );
        assert_eq!(summary(&checks), "All needed parts found");
        assert!(needed_found(&checks));
    }

    #[test]
    fn a_backend_without_global_shortcuts_names_the_part() {
        // e.g. xdg-desktop-portal-wlr, or GNOME before 48.
        let r = SystemReport {
            desktop: "sway".into(),
            global_shortcuts: None,
            ..kde()
        };
        let checks = list(&r, &TypingState::Unknown);
        let m = missing(&checks);
        assert_eq!(m.len(), 1, "{m:?}");
        assert_eq!(m[0].0, "GlobalShortcuts portal");
        assert!(m[0].1.contains("-hyprland"), "{}", m[0].1);
        assert_eq!(summary(&checks), "1 missing");
        assert!(!needed_found(&checks));
    }

    #[test]
    fn no_wayland() {
        let r = SystemReport {
            wayland: false,
            ..kde()
        };
        let m: Vec<_> = missing(&list(&r, &TypingState::Granted))
            .into_iter()
            .map(|m| m.0)
            .collect();
        assert_eq!(m, ["Wayland compositor"]);
    }

    #[test]
    fn typing_permission_and_old_portal() {
        let c = &list(&kde(), &TypingState::Denied)[6];
        assert!(!c.ok && c.detail.contains("didn't allow"), "{c:?}");
        let old = SystemReport {
            remote_desktop: Some(1),
            ..kde()
        };
        let c = &list(&old, &TypingState::Unknown)[1];
        assert!(!c.ok && c.detail.contains("1.18"), "{c:?}");
        let no_keyboard = SystemReport {
            remote_desktop_keyboard: false,
            ..kde()
        };
        let c = &list(&no_keyboard, &TypingState::Unknown)[6];
        assert!(!c.ok && c.detail.contains("no keyboard"), "{c:?}");
    }

    #[test]
    fn no_portal_names_the_portal_on_every_portal_line() {
        let r = SystemReport {
            portal: false,
            global_shortcuts: None,
            remote_desktop: None,
            clipboard: None,
            notification: None,
            background: None,
            ..kde()
        };
        let checks = list(&r, &TypingState::Unknown);
        let m = missing(&checks);
        assert_eq!(
            m.iter().map(|m| m.0).collect::<Vec<_>>(),
            [
                "xdg-desktop-portal 1.18+",
                "Notification portal",
                "GlobalShortcuts portal",
                "RemoteDesktop portal",
                "Input emulation (libei)",
                "Clipboard portal",
                "Background portal"
            ]
        );
        assert_eq!(m[1].1, NO_PORTAL);
    }

    #[test]
    fn audio_server_and_microphone() {
        let r = SystemReport {
            audio_server: false,
            microphones: 0,
            ..kde()
        };
        assert!(
            list(&r, &TypingState::Granted)[2]
                .detail
                .contains("PipeWire")
        );
        let r = SystemReport {
            microphones: 0,
            ..kde()
        };
        assert_eq!(
            list(&r, &TypingState::Granted)[2].detail,
            "Running, but no microphone found."
        );
    }
}
