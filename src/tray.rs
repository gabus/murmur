//! StatusNotifierItem tray icon (ksni): state icons, left-click toggle, menu.

use std::{collections::HashMap, time::Duration};

use ksni::TrayMethods;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::{
    app::{Command, Status},
    icons::{self, Look},
    model_manager::ModelsView,
    models::MODELS,
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
    models: ModelsView,
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
        let description = match &self.models.download {
            Some(d) => {
                let name = MODELS
                    .iter()
                    .find(|m| m.id == d.model)
                    .map_or(d.model, |m| m.name);
                let what = if d.unpacking {
                    "unpacking".to_owned()
                } else {
                    format!("{}%", d.percent)
                };
                format!("{description}\nDownloading {name}… {what}")
            }
            None => description,
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
        use ksni::menu::{RadioGroup, RadioItem, StandardItem, SubMenu};
        let label = if self.listening() {
            "Stop dictation"
        } else {
            "Start dictation"
        };
        let mv = &self.models;
        let options = MODELS
            .iter()
            .map(|m| {
                let mut label = format!("{} — {}", m.name, m.summary);
                match &mv.download {
                    Some(d) if d.model == m.id && d.unpacking => label.push_str("  (unpacking…)"),
                    Some(d) if d.model == m.id => {
                        label.push_str(&format!("  (downloading {}%)", d.percent));
                    }
                    _ if !mv.installed.contains(&m.id) => {
                        label.push_str(&format!("  ({} download)", m.size_label()));
                    }
                    _ => {}
                }
                RadioItem {
                    label,
                    ..Default::default()
                }
            })
            .collect();
        let selected = MODELS.iter().position(|m| m.id == mv.active).unwrap_or(0);
        let mut model_menu: Vec<ksni::MenuItem<Self>> = vec![
            RadioGroup {
                selected,
                select: Box::new(|t: &mut Self, i| {
                    if let Some(m) = MODELS.get(i) {
                        let _ = t.commands.send(Command::SelectModel(m.id.to_owned()));
                    }
                }),
                options,
            }
            .into(),
        ];
        if mv.download.is_some() {
            model_menu.push(
                StandardItem {
                    label: "Cancel download".into(),
                    icon_name: "process-stop".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.commands.send(Command::CancelDownload);
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        let deletable: Vec<ksni::MenuItem<Self>> = MODELS
            .iter()
            .filter(|m| mv.installed.contains(&m.id) && m.id != mv.active)
            .map(|m| {
                let id = m.id;
                StandardItem {
                    label: format!("{} ({})", m.name, m.size_label()),
                    icon_name: "edit-delete".into(),
                    activate: Box::new(move |t: &mut Self| {
                        let _ = t.commands.send(Command::DeleteModel(id.to_owned()));
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect();
        if !deletable.is_empty() {
            model_menu.push(ksni::MenuItem::Separator);
            model_menu.push(
                SubMenu {
                    label: "Delete downloaded model".into(),
                    icon_name: "edit-delete".into(),
                    submenu: deletable,
                    ..Default::default()
                }
                .into(),
            );
        }
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
            SubMenu {
                label: "Model".into(),
                submenu: model_menu,
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
    mut models: watch::Receiver<ModelsView>,
    commands: mpsc::UnboundedSender<Command>,
    shortcut: Option<String>,
) {
    let mut tray = MurmurTray {
        shown: Shown::Idle,
        models: models.borrow_and_update().clone(),
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
            changed = models.changed() => {
                if changed.is_err() {
                    break;
                }
                let v = models.borrow_and_update().clone();
                handle.update(|t| t.models = v).await;
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
