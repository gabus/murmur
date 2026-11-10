//! Desktop notifications through the Notification portal, only for errors the
//! user must act on (spec: no notifications in normal use).

use ashpd::desktop::notification::{Notification, NotificationProxy, Priority};
use tokio::sync::mpsc;
use tracing::warn;

/// Handle used by the app to request a notification.
#[derive(Clone)]
pub struct Notifier {
    tx: mpsc::UnboundedSender<(String, String)>,
}

impl Notifier {
    /// Spawns the sending task.
    pub fn spawn() -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, String)>();
        tokio::spawn(async move {
            let mut proxy: Option<NotificationProxy> = None;
            while let Some((title, body)) = rx.recv().await {
                if proxy.is_none() {
                    proxy = NotificationProxy::new()
                        .await
                        .inspect_err(|e| warn!(error = %e, "notification portal unavailable"))
                        .ok();
                }
                let Some(p) = proxy.as_ref() else { continue };
                let n = Notification::new(&title)
                    .body(body.as_str())
                    .priority(Priority::High);
                if let Err(e) = p.add_notification("murmur-error", n).await {
                    warn!(error = %e, "could not show notification");
                }
            }
        });
        Self { tx }
    }

    /// A notifier that drops everything (tests, dev harness).
    pub fn disabled() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self { tx }
    }

    pub fn error(&self, title: &str, body: &str) {
        let _ = self.tx.send((title.to_owned(), body.to_owned()));
    }
}
