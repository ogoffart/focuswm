//! The compositor's global state and per-client data.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::{LoopHandle, LoopSignal};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_callback::WlCallback;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::shell::wlr_layer::{Layer, LayerSurface, WlrLayerShellState};
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::{PopupSurface, ToplevelSurface, XdgShellState};
use smithay::wayland::shm::ShmState;

use focuswm_shell::WindowId;

use crate::Event;

/// The single state value calloop hands to every protocol handler.
pub struct FocusState {
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,
    /// Event-loop handle, used to drive XWayland clipboard transfers.
    pub loop_handle: LoopHandle<'static, FocusState>,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub idle_inhibit_state: smithay::wayland::idle_inhibit::IdleInhibitManagerState,
    /// Surfaces currently inhibiting idle (video players etc.).
    pub idle_inhibitors: std::collections::HashSet<WlSurface>,
    /// Extra protocol globals kept alive for their lifetime.
    pub viewporter_state: smithay::wayland::viewporter::ViewporterState,
    /// `cursor-shape-v1`: lets clients request named cursor shapes we can map to
    /// the host cursor (see `cursor_image`).
    pub cursor_shape_state: smithay::wayland::cursor_shape::CursorShapeManagerState,
    pub single_pixel_buffer_state:
        smithay::wayland::single_pixel_buffer::SinglePixelBufferState,
    pub fractional_scale_state: smithay::wayland::fractional_scale::FractionalScaleManagerState,
    pub xdg_activation_state: smithay::wayland::xdg_activation::XdgActivationState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<FocusState>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    /// GPU (dmabuf) buffer protocol; the global exists only when dmabuf is on.
    pub dmabuf_state: smithay::wayland::dmabuf::DmabufState,
    pub dmabuf_global: Option<smithay::wayland::dmabuf::DmabufGlobal>,
    pub dmabuf_enabled: bool,
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
    /// Mapped layer-shell surfaces (bars, wallpapers), keyed by `wl_surface`.
    pub layer_surfaces: HashMap<WlSurface, LayerEntry>,

    /// XWayland window manager (created once XWayland signals it is ready).
    pub xwm: Option<smithay::xwayland::X11Wm>,
    /// xwayland-shell global, associating X11 windows with `wl_surface`s.
    pub xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState,
    /// Mapped X11 windows, keyed by their associated `wl_surface`.
    pub x11_windows: HashMap<WlSurface, X11Entry>,

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

    /// Outbound channel notifying the UI thread of shell events (wakes the UI
    /// event loop on send, for low input→display latency).
    pub events: crate::EventSender,

    /// Builds keymaps on the fly so the focused client can receive arbitrary
    /// composed Unicode (accents, AltGr, dead keys) regardless of layout.
    pub text_input: crate::input::TextInput,

    /// The drag icon surface of an in-progress client drag-and-drop, if the
    /// client supplied one. Composited following the cursor while set.
    pub dnd_icon: Option<WlSurface>,
}

/// A tracked layer-shell surface (panel, bar, wallpaper, notification).
pub struct LayerEntry {
    pub id: WindowId,
    pub surface: LayerSurface,
    pub layer: Layer,
}

/// A tracked X11 (XWayland) window.
pub struct X11Entry {
    pub id: WindowId,
    pub surface: smithay::xwayland::X11Surface,
}

/// A tracked toplevel window.
pub struct WindowEntry {
    pub id: WindowId,
    pub toplevel: ToplevelSurface,
    /// Whether the compositor draws server-side decorations for this window
    /// (false when the client negotiated client-side decorations).
    pub decorated: bool,
    /// Top-left of the client's declared window geometry within its buffer; the
    /// displayed buffer is cropped to it, so this offset maps pointer coordinates
    /// back to surface-local space.
    pub geometry_offset: (i32, i32),
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
            .or_else(|| {
                self.layer_surfaces
                    .values()
                    .find(|e| e.id == id)
                    .map(|e| e.surface.wl_surface().clone())
            })
            .or_else(|| {
                self.x11_windows
                    .values()
                    .find(|e| e.id == id)
                    .and_then(|e| e.surface.wl_surface())
            })
    }

    /// The window/popup id owning a surface (to resolve a popup's parent).
    pub fn id_of_surface(&self, surface: &WlSurface) -> Option<WindowId> {
        self.windows
            .get(surface)
            .map(|e| e.id)
            .or_else(|| self.popups.get(surface).map(|e| e.id))
    }

    /// Offer the clipboard and primary selection to the client owning `surface`
    /// (called when keyboard focus changes, so copy/paste — including middle-click
    /// primary paste — works).
    pub fn set_selection_focus(&self, surface: &WlSurface) {
        use smithay::reexports::wayland_server::Resource;
        let client = surface.client();
        smithay::wayland::selection::data_device::set_data_device_focus(
            &self.display_handle,
            &self.seat,
            client.clone(),
        );
        smithay::wayland::selection::primary_selection::set_primary_focus(
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
