//! Start at login through the Background portal (opt-in, README → Autostart).

use ashpd::desktop::background::Background;
use tracing::info;

/// Asks the desktop to (not) start murmur at login. Returns the resulting state.
pub async fn set(enabled: bool) -> Result<bool, String> {
    crate::portal::register_host_app().await;
    let request = Background::request()
        .reason("Start murmur when you log in, so the dictation shortcut is always ready.")
        .auto_start(enabled)
        .command(["murmur"])
        .dbus_activatable(false);
    let response = request
        .send()
        .await
        .map_err(|e| e.to_string())?
        .response()
        .map_err(|e| match e {
            ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => {
                "the request was declined".to_owned()
            }
            other => other.to_string(),
        })?;
    info!(autostart = response.auto_start(), "autostart updated");
    Ok(response.auto_start())
}
