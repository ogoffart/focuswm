//! The focuswm Wayland-protocol engine, built on Smithay.
//!
//! This crate is rendering-agnostic: it owns the Wayland display, socket, client
//! state and protocol handlers, and runs them on a `calloop` event loop. It does
//! **not** present anything — Slint owns output/input/GL in the binary crate.
//! State updates flow out to the UI thread over an [`Event`] channel; the UI
//! drives input and window management back over a [`Command`] channel.

use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::Context as _;
use smithay::input::keyboard::XkbConfig;
use smithay::input::SeatState;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::{BindError, Display, ListeningSocket};
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;

mod handlers;
mod input;
mod state;
mod xwayland;

pub use focuswm_shell::WindowId;
pub use state::FocusState;

/// Default output size (logical px) when starting nested.
pub const OUTPUT_W: i32 = 1280;
pub const OUTPUT_H: i32 = 800;

/// Events emitted by the compositor thread for the UI thread to consume.
pub enum Event {
    /// The compositor is up and accepting clients on `socket_name`.
    Ready {
        socket_name: String,
        /// Directory holding the socket; launched apps need it as their
        /// `XDG_RUNTIME_DIR` to connect.
        runtime_dir: String,
        width: i32,
        height: i32,
    },
    WindowAdded(WindowId),
    WindowRemoved(WindowId),
    /// A window asked to be moved interactively (the client dragged its own
    /// client-side title bar → `xdg_toplevel.move`). The UI then drives the
    /// floating move from the ongoing pointer drag.
    MoveRequested(WindowId),
    /// The window's decoration mode changed (true = compositor draws SSD).
    WindowDecorated { id: WindowId, decorated: bool },
    /// A window committed a new frame: tightly-packed RGBA8 of `width`x`height`,
    /// plus its current title, app-id and whether it wants server-side
    /// decorations.
    WindowBuffer {
        id: WindowId,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        title: String,
        app_id: String,
        decorated: bool,
        /// Client size hints in surface (content) logical px; 0 = unset, i.e.
        /// no minimum / no maximum on that axis.
        min_w: i32,
        min_h: i32,
        max_w: i32,
        max_h: i32,
    },
    /// A popup committed a frame, drawn at offset `(ox, oy)` from `parent`.
    PopupBuffer {
        id: WindowId,
        parent: Option<WindowId>,
        ox: i32,
        oy: i32,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    PopupRemoved(WindowId),
    /// A layer-shell surface committed a frame, drawn at `(x, y)`. `layer` is
    /// 0=background, 1=bottom, 2=top, 3=overlay.
    LayerBuffer {
        id: WindowId,
        layer: u8,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    LayerRemoved(WindowId),
    /// A client requested activation/attention (xdg-activation) for this window.
    ActivationRequested(WindowId),
    /// A client started (true) or stopped (false) inhibiting idle — e.g. a video
    /// player; the shell should suppress its idle lock while true.
    IdleInhibited(bool),
    /// XWayland is up; X11 apps should be launched with `DISPLAY=:{display}`.
    XwaylandReady { display: u32 },
    /// A window committed a GPU (dmabuf) buffer. The planes carry owned fds for
    /// the UI thread to import as an EGLImage-backed GL texture.
    WindowDmabuf {
        id: WindowId,
        width: u32,
        height: u32,
        /// DRM FourCC format code.
        fourcc: u32,
        /// DRM format modifier.
        modifier: u64,
        planes: Vec<DmabufPlane>,
    },
    /// The output was resized (echoed back after a `Command::ResizeOutput`).
    OutputResized { width: i32, height: i32 },
    /// A client started an interactive drag-and-drop. The UI should route
    /// pointer motion globally (to whichever window is under the cursor) until
    /// the matching `DragEnded`, so the drag can cross between applications.
    DragStarted,
    /// The drag-and-drop ended (drop or cancel); stop global routing and hide
    /// the drag icon.
    DragEnded,
    /// The drag icon committed a frame: tightly-packed RGBA8 of `width`x`height`,
    /// to be drawn following the cursor. `hot_x`/`hot_y` is the cursor hotspot
    /// within the image (logical px).
    DragIcon {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        hot_x: i32,
        hot_y: i32,
    },
}

/// One plane of a dmabuf: an owned file descriptor plus its offset and stride.
#[derive(Debug)]
pub struct DmabufPlane {
    pub fd: std::os::fd::OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Event::Ready {
                socket_name,
                runtime_dir,
                width,
                height,
            } => f
                .debug_struct("Ready")
                .field("socket_name", socket_name)
                .field("runtime_dir", runtime_dir)
                .field("width", width)
                .field("height", height)
                .finish(),
            Event::WindowAdded(id) => f.debug_tuple("WindowAdded").field(id).finish(),
            Event::WindowRemoved(id) => f.debug_tuple("WindowRemoved").field(id).finish(),
            Event::MoveRequested(id) => f.debug_tuple("MoveRequested").field(id).finish(),
            Event::WindowDecorated { id, decorated } => f
                .debug_struct("WindowDecorated")
                .field("id", id)
                .field("decorated", decorated)
                .finish(),
            Event::WindowBuffer {
                id,
                width,
                height,
                title,
                app_id,
                decorated,
                ..
            } => f
                .debug_struct("WindowBuffer")
                .field("id", id)
                .field("width", width)
                .field("height", height)
                .field("title", title)
                .field("app_id", app_id)
                .field("decorated", decorated)
                .finish_non_exhaustive(),
            Event::PopupBuffer {
                id,
                parent,
                width,
                height,
                ..
            } => f
                .debug_struct("PopupBuffer")
                .field("id", id)
                .field("parent", parent)
                .field("width", width)
                .field("height", height)
                .finish_non_exhaustive(),
            Event::PopupRemoved(id) => f.debug_tuple("PopupRemoved").field(id).finish(),
            Event::LayerBuffer {
                id, layer, x, y, width, height, ..
            } => f
                .debug_struct("LayerBuffer")
                .field("id", id)
                .field("layer", layer)
                .field("x", x)
                .field("y", y)
                .field("width", width)
                .field("height", height)
                .finish_non_exhaustive(),
            Event::LayerRemoved(id) => f.debug_tuple("LayerRemoved").field(id).finish(),
            Event::ActivationRequested(id) => {
                f.debug_tuple("ActivationRequested").field(id).finish()
            }
            Event::IdleInhibited(on) => f.debug_tuple("IdleInhibited").field(on).finish(),
            Event::XwaylandReady { display } => {
                f.debug_struct("XwaylandReady").field("display", display).finish()
            }
            Event::WindowDmabuf {
                id, width, height, ..
            } => f
                .debug_struct("WindowDmabuf")
                .field("id", id)
                .field("width", width)
                .field("height", height)
                .finish_non_exhaustive(),
            Event::OutputResized { width, height } => f
                .debug_struct("OutputResized")
                .field("width", width)
                .field("height", height)
                .finish(),
            Event::DragStarted => f.debug_struct("DragStarted").finish(),
            Event::DragEnded => f.debug_struct("DragEnded").finish(),
            Event::DragIcon {
                width, height, hot_x, hot_y, ..
            } => f
                .debug_struct("DragIcon")
                .field("width", width)
                .field("height", height)
                .field("hot_x", hot_x)
                .field("hot_y", hot_y)
                .finish_non_exhaustive(),
        }
    }
}

/// Commands sent from the UI thread to the compositor.
#[derive(Debug)]
pub enum Command {
    /// Ask a window to close (sends `xdg_toplevel.close`).
    CloseWindow(WindowId),
    /// Give keyboard focus to a window.
    FocusWindow(WindowId),
    /// Pointer moved to surface-local `(x, y)` over the given window.
    PointerMotion { id: WindowId, x: f64, y: f64 },
    /// Pointer button (evdev code) pressed/released over the given window.
    PointerButton {
        id: WindowId,
        button: u32,
        pressed: bool,
    },
    /// Pointer left all client windows.
    PointerLeave,
    /// Scroll over the given window.
    PointerAxis { id: WindowId, dx: f64, dy: f64 },
    /// A key (evdev keycode) pressed/released for the focused window.
    Key { keycode: u32, pressed: bool },
    /// Type already-composed Unicode text into the focused window. Used for
    /// printable input (letters, digits, symbols, accents, AltGr layers,
    /// dead-key results) so it is independent of the compositor's keymap and of
    /// the host keyboard layout.
    TypeText(String),
    /// Resize a window to the given size (sends an xdg configure).
    ResizeWindow {
        id: WindowId,
        width: i32,
        height: i32,
    },
    /// Tell a window whether it is maximized (sets the xdg `Maximized` state and
    /// sizes it to the output).
    SetMaximized { id: WindowId, maximized: bool },
    /// The output (host window) was resized.
    ResizeOutput { width: i32, height: i32 },
    /// Dismiss all open popups (e.g. a click landed outside them).
    DismissPopups,
}

/// Re-exported so the UI crate can hold the sending half.
pub use smithay::reexports::calloop::channel::Sender as CommandSender;

/// Create the command channel. The [`CommandSender`] stays on the UI thread; the
/// channel is handed to [`run`].
pub fn command_channel() -> (
    CommandSender<Command>,
    smithay::reexports::calloop::channel::Channel<Command>,
) {
    smithay::reexports::calloop::channel::channel()
}

/// Run the Wayland compositor event loop on a dedicated thread; blocks until the
/// loop is torn down.
pub fn run(
    events: Sender<Event>,
    commands: smithay::reexports::calloop::channel::Channel<Command>,
) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<FocusState> =
        EventLoop::try_new().context("failed to create calloop event loop")?;
    let display: Display<FocusState> = Display::new().context("failed to create wl_display")?;
    let dh = display.handle();

    let compositor_state = CompositorState::new::<FocusState>(&dh);
    let xdg_shell_state = XdgShellState::new::<FocusState>(&dh);
    let xdg_decoration_state =
        smithay::wayland::shell::xdg::decoration::XdgDecorationState::new::<FocusState>(&dh);
    let layer_shell_state =
        smithay::wayland::shell::wlr_layer::WlrLayerShellState::new::<FocusState>(&dh);
    let idle_inhibit_state =
        smithay::wayland::idle_inhibit::IdleInhibitManagerState::new::<FocusState>(&dh);
    let viewporter_state = smithay::wayland::viewporter::ViewporterState::new::<FocusState>(&dh);
    let single_pixel_buffer_state =
        smithay::wayland::single_pixel_buffer::SinglePixelBufferState::new::<FocusState>(&dh);
    let fractional_scale_state =
        smithay::wayland::fractional_scale::FractionalScaleManagerState::new::<FocusState>(&dh);
    let xdg_activation_state =
        smithay::wayland::xdg_activation::XdgActivationState::new::<FocusState>(&dh);
    let shm_state = ShmState::new::<FocusState>(&dh, Vec::new());
    let output_manager_state = OutputManagerState::new_with_xdg_output::<FocusState>(&dh);
    let mut seat_state = SeatState::<FocusState>::new();
    let data_device_state = DataDeviceState::new::<FocusState>(&dh);
    let primary_selection_state =
        smithay::wayland::selection::primary_selection::PrimarySelectionState::new::<FocusState>(&dh);
    // GPU buffers (dmabuf) are on unless explicitly disabled; import happens on
    // the UI thread and needs a real GPU.
    let dmabuf_enabled = std::env::var_os("FOCUSWM_NO_DMABUF").is_none();
    let dmabuf_state = smithay::wayland::dmabuf::DmabufState::new();
    let xwayland_shell_state =
        smithay::wayland::xwayland_shell::XWaylandShellState::new::<FocusState>(&dh);

    let mut seat = seat_state.new_wl_seat(&dh, "seat0");
    seat.add_keyboard(XkbConfig::default(), 200, 25)
        .context("failed to add keyboard to seat")?;
    seat.add_pointer();

    // Advertise a single output (many clients refuse to map without one).
    let output = smithay::output::Output::new(
        "focuswm-0".to_string(),
        smithay::output::PhysicalProperties {
            size: (0, 0).into(),
            subpixel: smithay::output::Subpixel::Unknown,
            make: "focuswm".into(),
            model: "virtual".into(),
        },
    );
    output.create_global::<FocusState>(&dh);
    let mode = smithay::output::Mode {
        size: (OUTPUT_W, OUTPUT_H).into(),
        refresh: 60_000,
    };
    output.change_current_state(Some(mode), None, None, Some((0, 0).into()));
    output.set_preferred(mode);

    let mut state = FocusState {
        display_handle: dh.clone(),
        loop_signal: event_loop.get_signal(),
        loop_handle: event_loop.handle(),
        compositor_state,
        xdg_shell_state,
        xdg_decoration_state,
        layer_shell_state,
        idle_inhibit_state,
        idle_inhibitors: std::collections::HashSet::new(),
        viewporter_state,
        single_pixel_buffer_state,
        fractional_scale_state,
        xdg_activation_state,
        shm_state,
        output_manager_state,
        seat_state,
        data_device_state,
        primary_selection_state,
        dmabuf_state,
        dmabuf_global: None,
        dmabuf_enabled,
        seat,
        output,
        current_output_size: (OUTPUT_W, OUTPUT_H),
        next_window_id: 0,
        windows: std::collections::HashMap::new(),
        popups: std::collections::HashMap::new(),
        layer_surfaces: std::collections::HashMap::new(),
        xwm: None,
        xwayland_shell_state,
        x11_windows: std::collections::HashMap::new(),
        surface_pixels: std::collections::HashMap::new(),
        start_time: std::time::Instant::now(),
        pending_callbacks: Vec::new(),
        events: events.clone(),
        text_input: Default::default(),
        dnd_icon: None,
    };

    // Install a keymap pre-loaded with common Unicode characters before any
    // client connects, so text input works for any layout without ever swapping
    // a focused client's keymap (which wedges some toolkits). Best-effort: if the
    // base keymap can't be compiled, fall back to the default US keymap.
    if let Some(keymap) = state.text_input.prime() {
        if let Some(keyboard) = state.seat.get_keyboard() {
            if let Err(err) = keyboard.set_keymap_from_string(&mut state, keymap) {
                log::warn!("failed to install Unicode keymap: {err:?}");
            }
        }
    }

    if state.dmabuf_enabled {
        use smithay::backend::allocator::{Format, Fourcc, Modifier};
        // Advertise common 32-bit formats with implicit/linear modifiers. The
        // real importable set is unknown on this UI-less thread, so import is
        // attempted on the render thread and may fall back.
        let formats: Vec<Format> = [Fourcc::Argb8888, Fourcc::Xrgb8888]
            .into_iter()
            .flat_map(|code| {
                [Modifier::Invalid, Modifier::Linear]
                    .into_iter()
                    .map(move |modifier| Format { code, modifier })
            })
            .collect();
        let global = state.dmabuf_state.create_global::<FocusState>(&dh, formats);
        state.dmabuf_global = Some(global);
        log::info!("dmabuf: advertising zwp_linux_dmabuf_v1");
    }

    let (socket, socket_name, runtime_dir) =
        bind_socket().context("failed to create wayland socket")?;
    let handle = event_loop.handle();

    handle
        .insert_source(
            Generic::new(socket, Interest::READ, Mode::Level),
            move |_, socket, state: &mut FocusState| {
                while let Some(stream) = socket.accept()? {
                    match state
                        .display_handle
                        .insert_client(stream, state::ClientState::arc())
                    {
                        Ok(_) => log::info!("wayland: client connected"),
                        Err(err) => log::warn!("failed to accept client: {err}"),
                    }
                }
                Ok::<_, std::io::Error>(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert socket source: {e}"))?;

    handle
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state: &mut FocusState| {
                // SAFETY: the display is not dropped while the loop runs.
                unsafe {
                    display
                        .get_mut()
                        .dispatch_clients(state)
                        .map_err(std::io::Error::other)?;
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert display source: {e}"))?;

    handle
        .insert_source(commands, |event, _, state: &mut FocusState| {
            if let smithay::reexports::calloop::channel::Event::Msg(command) = event {
                state.handle_command(command);
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert command source: {e}"))?;

    // Fire queued frame callbacks at ~60Hz so clients pace their rendering.
    handle
        .insert_source(
            Timer::from_duration(Duration::from_millis(16)),
            |_, _, state: &mut FocusState| {
                let now = state.millis_since_start();
                for callback in state.pending_callbacks.drain(..) {
                    callback.done(now);
                }
                if let Err(err) = state.display_handle.flush_clients() {
                    log::warn!("failed to flush clients: {err}");
                }
                TimeoutAction::ToDuration(Duration::from_millis(16))
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert frame timer: {e}"))?;

    // Start XWayland so X11 apps can run (best-effort; needs the Xwayland
    // binary). Readiness arrives asynchronously as an `XwaylandReady` event; we
    // do not block on it. Until then X11 clients get an empty DISPLAY and fail
    // to connect rather than leaking into the parent session's X server.
    xwayland::setup(&handle, &dh);

    log::info!("focuswm listening on {runtime_dir}/{socket_name}");
    let _ = events.send(Event::Ready {
        socket_name,
        runtime_dir,
        width: OUTPUT_W,
        height: OUTPUT_H,
    });

    event_loop
        .run(None, &mut state, |state| {
            if let Err(err) = state.display_handle.flush_clients() {
                log::warn!("failed to flush clients: {err}");
            }
        })
        .context("event loop terminated unexpectedly")?;

    Ok(())
}

/// Bind the compositor's listening socket. Tries `XDG_RUNTIME_DIR` first, then a
/// private `focuswm-<uid>` directory under the temp dir.
fn bind_socket() -> anyhow::Result<(ListeningSocket, String, String)> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::PathBuf;

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(&dir);
        if path.is_absolute() {
            candidates.push(path);
        }
    }
    let uid = std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0);
    let fallback = std::env::temp_dir().join(format!("focuswm-{uid}"));
    candidates.push(fallback.clone());

    let mut last_err: Option<String> = None;
    for dir in candidates {
        if dir == fallback {
            if let Err(err) = std::fs::create_dir_all(&dir) {
                last_err = Some(format!("create {}: {err}", dir.display()));
                continue;
            }
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        for n in 1..33u32 {
            let name = format!("wayland-{n}");
            match ListeningSocket::bind_absolute(dir.join(&name)) {
                Ok(socket) => return Ok((socket, name, dir.to_string_lossy().into_owned())),
                Err(BindError::AlreadyInUse) => continue,
                Err(err) => {
                    last_err = Some(format!("{}: {err}", dir.display()));
                    break;
                }
            }
        }
    }
    anyhow::bail!(
        "no writable runtime directory for the wayland socket ({})",
        last_err.unwrap_or_else(|| "unknown".into())
    )
}
