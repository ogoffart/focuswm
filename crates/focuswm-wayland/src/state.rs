//! The compositor's global state and per-client data.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::LoopSignal;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_callback::WlCallback;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::{PopupSurface, ToplevelSurface, XdgShellState};
use smithay::wayland::shm::ShmState;

use focuswm_shell::WindowId;

use crate::Event;

/// The single state value calloop hands to every protocol handler.
pub struct FocusState {
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<FocusState>,
    pub data_device_state: DataDeviceState,
    pub seat: Seat<FocusState>,
    pub output: Output,
    /// Current size (logical px) of the output; tracks the host window when
    /// nested.
    pub current_output_size: (i32, i32),

    pub next_window_id: u64,

    /// Mapped toplevel windows, keyed by their root `wl_surface`.
    pub windows: HashMap<WlSurface, WindowEntry>,
    /// Mapped popups (menus etc.), keyed by their `wl_surface`.
    pub popups: HashMap<WlSurface, PopupEntry>,

    /// Last-known RGBA8 contents of every live surface (window roots and their
    /// subsurfaces), keyed by `wl_surface`. Lets us flatten a surface tree into
    /// one buffer on commit so subsurfaces that don't re-attach every frame stay
    /// visible.
    pub surface_pixels: HashMap<WlSurface, (u32, u32, Vec<u8>)>,

    /// Start of the compositor, used to timestamp `wl_callback.done`.
    pub start_time: Instant,
    /// Frame callbacks awaiting completion, fired on a ~60Hz timer so clients
    /// pace rendering instead of busy-looping.
    pub pending_callbacks: Vec<WlCallback>,

    /// Outbound channel notifying the UI thread of shell events.
    pub events: std::sync::mpsc::Sender<Event>,
}

/// A tracked toplevel window.
pub struct WindowEntry {
    pub id: WindowId,
    pub toplevel: ToplevelSurface,
}

/// A tracked popup (menu, dropdown, tooltip).
pub struct PopupEntry {
    pub id: WindowId,
    pub popup: PopupSurface,
    /// The id of the parent window/popup this is positioned against.
    pub parent_id: Option<WindowId>,
    /// Offset from the parent surface, from the positioner geometry.
    pub offset: (i32, i32),
}

impl FocusState {
    /// Allocate the next stable window id.
    pub fn allocate_window_id(&mut self) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id += 1;
        id
    }

    /// Milliseconds since startup, for frame-callback timestamps.
    pub fn millis_since_start(&self) -> u32 {
        self.start_time.elapsed().as_millis() as u32
    }

    /// The `wl_surface` of the window or popup with the given id, if mapped.
    pub fn surface_for(&self, id: WindowId) -> Option<WlSurface> {
        self.windows
            .values()
            .find(|e| e.id == id)
            .map(|e| e.toplevel.wl_surface().clone())
            .or_else(|| {
                self.popups
                    .values()
                    .find(|e| e.id == id)
                    .map(|e| e.popup.wl_surface().clone())
            })
    }

    /// The window/popup id owning a surface (to resolve a popup's parent).
    pub fn id_of_surface(&self, surface: &WlSurface) -> Option<WindowId> {
        self.windows
            .get(surface)
            .map(|e| e.id)
            .or_else(|| self.popups.get(surface).map(|e| e.id))
    }

    /// Offer the clipboard to the client owning `surface` (called when keyboard
    /// focus changes, so copy/paste works).
    pub fn set_selection_focus(&self, surface: &WlSurface) {
        use smithay::reexports::wayland_server::Resource;
        let client = surface.client();
        smithay::wayland::selection::data_device::set_data_device_focus(
            &self.display_handle,
            &self.seat,
            client,
        );
    }
}

/// Per-client data stored by the Wayland backend.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

impl ClientState {
    pub fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }
}
