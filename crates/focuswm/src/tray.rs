//! System tray (StatusNotifierItem / SNI) host.
//!
//! Wraps the `system-tray` crate on a dedicated Tokio thread: it hosts the
//! `StatusNotifierWatcher`, subscribes to item add/update/remove events and
//! forwards them (with icon pixmaps converted to RGBA) to the UI thread over a
//! plain channel. Activation requests flow back the other way. If there's no
//! session bus the host logs and exits without affecting the rest of the shell.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use system_tray::client::{ActivateRequest, Client, Event, UpdateEvent};
use system_tray::item::StatusNotifierItem;

/// An update about one tray item, sent to the UI thread. `Add` is an upsert.
#[derive(Debug, Clone)]
pub enum TrayUpdate {
    Add {
        /// The item's bus address; pass it back in [`TrayCommand::Activate`].
        id: String,
        title: String,
        /// Pre-converted RGBA8 icon `(width, height, pixels)`.
        pixmap: Option<(u32, u32, Vec<u8>)>,
    },
    Remove {
        id: String,
    },
}

/// A request from the UI thread to the tray host.
#[derive(Debug, Clone)]
pub enum TrayCommand {
    /// Default-activate the item (typically a left click).
    Activate(String),
}

/// Start the SNI host on its own thread. Returns the update receiver (drained on
/// the UI thread) and the command sender, or `None` if the thread can't spawn.
pub fn run() -> Option<(Receiver<TrayUpdate>, Sender<TrayCommand>)> {
    let (up_tx, up_rx) = std::sync::mpsc::channel::<TrayUpdate>();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<TrayCommand>();

    std::thread::Builder::new()
        .name("focuswm-tray".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    log::warn!("tray: could not start tokio runtime: {err}");
                    return;
                }
            };
            rt.block_on(host_loop(up_tx, cmd_rx));
        })
        .ok()?;

    Some((up_rx, cmd_tx))
}

async fn host_loop(up_tx: Sender<TrayUpdate>, cmd_rx: Receiver<TrayCommand>) {
    let client = match Client::new().await {
        Ok(client) => client,
        Err(err) => {
            log::warn!("tray: SNI host unavailable: {err}");
            return;
        }
    };
    let mut events = client.subscribe();
    let mut items: HashMap<String, ItemState> = HashMap::new();

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(event) => {
                    if handle_event(event, &mut items, &up_tx).is_err() {
                        break; // UI side hung up
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            },
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        TrayCommand::Activate(address) => {
                            let req = ActivateRequest::Default { address, x: 0, y: 0 };
                            if let Err(err) = client.activate(req).await {
                                log::debug!("tray: activate failed: {err}");
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Default, Clone)]
struct ItemState {
    title: String,
    icon_name: String,
    pixmap: Option<(u32, u32, Vec<u8>)>,
}

impl ItemState {
    fn emit(&self, id: &str, up_tx: &Sender<TrayUpdate>) -> Result<(), ()> {
        up_tx
            .send(TrayUpdate::Add {
                id: id.to_string(),
                title: self.title.clone(),
                pixmap: self.pixmap.clone(),
            })
            .map_err(|_| ())
    }
}

fn handle_event(
    event: Event,
    items: &mut HashMap<String, ItemState>,
    up_tx: &Sender<TrayUpdate>,
) -> Result<(), ()> {
    match event {
        Event::Add(id, item) => {
            let state = item_state(&item);
            state.emit(&id, up_tx)?;
            items.insert(id, state);
        }
        Event::Update(
            id,
            UpdateEvent::Icon {
                icon_name,
                icon_pixmap,
            },
        ) => {
            let state = items.entry(id.clone()).or_default();
            state.icon_name = icon_name.unwrap_or_default();
            state.pixmap = best_pixmap(icon_pixmap.as_deref());
            state.emit(&id, up_tx)?;
        }
        Event::Update(id, UpdateEvent::Title(title)) => {
            let state = items.entry(id.clone()).or_default();
            state.title = title.unwrap_or_default();
            state.emit(&id, up_tx)?;
        }
        Event::Update(..) => {} // menu/status/tooltip changes don't affect the icon
        Event::Remove(id) => {
            items.remove(&id);
            up_tx.send(TrayUpdate::Remove { id }).map_err(|_| ())?;
        }
    }
    Ok(())
}

fn item_state(item: &StatusNotifierItem) -> ItemState {
    ItemState {
        title: item
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| item.id.clone()),
        icon_name: item.icon_name.clone().unwrap_or_default(),
        pixmap: best_pixmap(item.icon_pixmap.as_deref()),
    }
}

/// Pick the largest pixmap and convert it from SNI's ARGB32 (bytes `A R G B`) to
/// the RGBA8 Slint expects.
fn best_pixmap(pixmaps: Option<&[system_tray::item::IconPixmap]>) -> Option<(u32, u32, Vec<u8>)> {
    let best = pixmaps?
        .iter()
        .filter(|p| p.width > 0 && p.height > 0)
        .max_by_key(|p| p.width * p.height)?;
    let expected = (best.width * best.height * 4) as usize;
    if best.pixels.len() < expected {
        return None;
    }
    let mut rgba = Vec::with_capacity(expected);
    for chunk in best.pixels.chunks_exact(4) {
        let [a, r, g, b] = [chunk[0], chunk[1], chunk[2], chunk[3]];
        rgba.extend_from_slice(&[r, g, b, a]);
    }
    Some((best.width as u32, best.height as u32, rgba))
}
