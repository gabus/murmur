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
