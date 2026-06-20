//! Freedesktop notification daemon (`org.freedesktop.Notifications`).
//!
//! Runs a zbus service on its own thread and forwards incoming notifications to
//! the UI thread, which shows them as on-screen toasts. Display-only: implements
//! `Notify`, `CloseNotification`, `GetCapabilities` and `GetServerInformation`
//! (enough for apps to post notifications); action callbacks aren't emitted back.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use zbus::zvariant::OwnedValue;

/// A notification event for the UI thread.
#[derive(Debug, Clone)]
pub enum NotifyEvent {
    Add {
        id: u32,
        app_name: String,
        summary: String,
        body: String,
        /// Raw `expire_timeout`: -1 = default, 0 = never, >0 = milliseconds.
        timeout_ms: i32,
    },
    Close {
        id: u32,
    },
}

struct Server {
    tx: async_channel::Sender<NotifyEvent>,
    next_id: AtomicU32,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        _actions: Vec<String>,
        _hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        };
        let _ = self
            .tx
            .send(NotifyEvent::Add {
                id,
                app_name,
                summary,
                body,
                timeout_ms: expire_timeout,
            })
            .await;
        id
    }

    async fn close_notification(&self, id: u32) {
        let _ = self.tx.send(NotifyEvent::Close { id }).await;
    }

    fn get_capabilities(&self) -> Vec<String> {
        vec!["body".to_string(), "icon-static".to_string()]
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "focuswm".to_string(),
            "focuswm".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
            "1.2".to_string(),
        )
    }
}

/// Start the notification daemon on its own thread, forwarding incoming
/// notifications to `tx`. If there's no session bus (e.g. in a sandbox) the
/// daemon simply logs and exits without affecting the rest of the shell.
pub fn spawn(tx: async_channel::Sender<NotifyEvent>) {
    let spawned = std::thread::Builder::new()
        .name("focuswm-notify".into())
        .spawn(move || {
            if let Err(err) = zbus::block_on(serve(tx)) {
                log::warn!("notify: daemon unavailable: {err}");
            }
        });
    if spawned.is_err() {
        log::warn!("notify: could not spawn daemon thread");
    }
}

async fn serve(tx: async_channel::Sender<NotifyEvent>) -> zbus::Result<()> {
    let server = Server {
        tx,
        next_id: AtomicU32::new(1),
    };
    let _conn = zbus::connection::Builder::session()?
        .name("org.freedesktop.Notifications")?
        .serve_at("/org/freedesktop/Notifications", server)?
        .build()
        .await?;
    // Keep the connection (and thus the service) alive forever.
    std::future::pending::<()>().await;
    Ok(())
}
