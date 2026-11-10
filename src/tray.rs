//! StatusNotifierItem tray icon (ksni): state icons, left-click toggle, menu.

use std::{collections::HashMap, time::Duration};

use ksni::TrayMethods;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::{
    app::{Command, Status},
    icons::{self, Look},
};

/// Show the loading animation only if loading takes longer than this (spec).
const LOADING_GRACE: Duration = Duration::from_millis(300);
const LOADING_FRAME: Duration = Duration::from_millis(250);
const IDLE_ICON: &str = "audio-input-microphone-symbolic";

/// What the icon currently shows (derived from `Status` and timing).
#[derive(Debug, Clone, PartialEq)]
enum Shown {
    Idle,
    Listening(u8),
    Loading(u8),
    Error(String),
}

struct MurmurTray {
    shown: Shown,
    /// Human-readable shortcut, e.g. "Meta+H".
    shortcut: Option<String>,
    commands: mpsc::UnboundedSender<Command>,
    cache: HashMap<String, Vec<ksni::Icon>>,
}

impl MurmurTray {
    fn pixmaps(&mut self, look: Look) -> Vec<ksni::Icon> {
        let key = format!("{look:?}");
        self.cache
            .entry(key)
            .or_insert_with(|| {
                icons::SIZES
                    .iter()
                    .map(|s| {
                        let p = icons::render(look, *s);
                        ksni::Icon {
                            width: p.width,
                            height: p.height,
                            data: p.data,
                        }
                    })
                    .collect()
            })
            .clone()
    }

    fn listening(&self) -> bool {
        matches!(self.shown, Shown::Listening(_) | Shown::Loading(_))
    }
}

impl ksni::Tray for MurmurTray {
    fn id(&self) -> String {
        "murmur".into()
    }

    fn title(&self) -> String {
        "murmur".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn icon_name(&self) -> String {
        match self.shown {
            Shown::Idle => IDLE_ICON.into(),
            _ => String::new(),
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let look = match &self.shown {
            Shown::Idle => return Vec::new(),
            Shown::Listening(l) => Look::Listening { level: *l },
            Shown::Loading(p) => Look::Loading { phase: *p },
            Shown::Error(_) => Look::Error,
        };
        // `icon_pixmap` takes &self; render on cache miss.
        self.cache
            .get(&format!("{look:?}"))
            .cloned()
            .unwrap_or_else(|| {
                icons::SIZES
                    .iter()
                    .map(|s| {
                        let p = icons::render(look, *s);
                        ksni::Icon {
                            width: p.width,
                            height: p.height,
                            data: p.data,
                        }
                    })
                    .collect()
            })
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let description = match &self.shown {
            Shown::Idle => match &self.shortcut {
                Some(s) => format!("Press {s} or click to dictate"),
                None => "Click to dictate".into(),
            },
            Shown::Listening(_) => "Listening — press again to stop".into(),
            Shown::Loading(_) => "Loading the speech model… (already recording)".into(),
            Shown::Error(e) => e.clone(),
        };
        ksni::ToolTip {
            title: "murmur".into(),
            description,
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.commands.send(Command::Toggle);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        let label = if self.listening() {
            "Stop dictation"
        } else {
            "Start dictation"
        };
        vec![
            StandardItem {
                label: label.into(),
                icon_name: IDLE_ICON.into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.commands.send(Command::Toggle);
                }),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.commands.send(Command::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        // No tray host (e.g. GNOME without the AppIndicator extension): keep
        // running; the shortcut still works, and the icon appears if a host starts.
        warn!(?reason, "no system tray available");
        true
    }
}

/// Runs the tray until the status channel closes.
pub async fn run(
    mut status: watch::Receiver<Status>,
    mut level: watch::Receiver<u8>,
    commands: mpsc::UnboundedSender<Command>,
    shortcut: Option<String>,
) {
    let mut tray = MurmurTray {
        shown: Shown::Idle,
        shortcut,
        commands,
        cache: HashMap::new(),
    };
    // Pre-render the frequently swapped pixmaps.
    for l in 0..icons::LEVELS {
        tray.pixmaps(Look::Listening { level: l });
    }
    let handle = match tray
        .disable_dbus_name(ashpd::is_sandboxed())
        .assume_sni_available(true)
        .spawn()
        .await
    {
        Ok(h) => h,
        Err(e) => {
            warn!(error = %e, "tray icon unavailable");
            return;
        }
    };
    info!("tray icon ready");

    let mut loading_since: Option<tokio::time::Instant> = None;
    let mut phase = 0u8;
    loop {
        let anim = async {
            match loading_since {
                Some(t) => {
                    let next = (t + LOADING_GRACE).max(tokio::time::Instant::now() + LOADING_FRAME);
                    tokio::time::sleep_until(next).await;
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            changed = status.changed() => {
                if changed.is_err() {
                    break;
                }
                let s = status.borrow_and_update().clone();
                loading_since = matches!(s, Status::Loading).then(tokio::time::Instant::now);
                let shown = match s {
                    Status::Idle => Shown::Idle,
                    // Loading starts out looking like Listening (spec: icon switches immediately).
                    Status::Loading | Status::Listening => Shown::Listening(*level.borrow()),
                    Status::Error(e) => Shown::Error(e),
                };
                handle.update(|t| t.shown = shown).await;
            }
            changed = level.changed() => {
                if changed.is_err() {
                    break;
                }
                let l = *level.borrow_and_update();
                handle
                    .update(|t| {
                        if let Shown::Listening(cur) = &mut t.shown {
                            *cur = l;
                        }
                    })
                    .await;
            }
            () = anim => {
                phase = phase.wrapping_add(1);
                let p = phase % 4;
                handle.update(|t| t.shown = Shown::Loading(p)).await;
            }
        }
    }
    handle.shutdown().await;
}
