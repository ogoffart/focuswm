//! Wayland protocol handler implementations.
//!
//! Each `impl` here satisfies a Smithay handler trait and is registered with the
//! matching `delegate_*!` macro at the bottom. The set is deliberately scoped to
//! the foundation milestone: core compositor, `xdg-shell` toplevels + popups,
//! `wl_shm`, seat and data-device (clipboard). Layer-shell, decorations,
//! XWayland and dmabuf are later milestones.

use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_callback::WlCallback;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Client;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    with_states, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SurfaceAttributes,
};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{with_buffer_contents as shm_with_buffer_contents, BufferData, ShmHandler, ShmState};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
};

use focuswm_render::{convert_to_rgba, ShmFormat};

use crate::state::{ClientState, FocusState, PopupEntry, WindowEntry};
use crate::Event;

impl CompositorHandler for FocusState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("client missing ClientState")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // A commit may arrive on a subsurface (desync mode); normalize to the
        // root so we always re-flatten the whole tree and answer every surface's
        // frame callbacks.
        let root = root_surface(surface);
        let mut callbacks = Vec::new();

        // Toplevel window?
        if let Some(entry) = self.windows.get(&root) {
            let id = entry.id;
            let title = read_title(&root);
            let app_id = read_app_id(&root);
            let mut cache = std::mem::take(&mut self.surface_pixels);
            let buffer = composite_tree(&root, &mut cache, &mut callbacks);
            self.surface_pixels = cache;
            self.pending_callbacks.append(&mut callbacks);
            if let Some((width, height, pixels)) = buffer {
                let _ = self.events.send(Event::WindowBuffer {
                    id,
                    width,
                    height,
                    pixels,
                    title,
                    app_id,
                });
            }
            return;
        }

        // Popup (menu, dropdown, tooltip)?
        if let Some(entry) = self.popups.get(&root) {
            let (id, parent, offset) = (entry.id, entry.parent_id, entry.offset);
            let mut cache = std::mem::take(&mut self.surface_pixels);
            let buffer = composite_tree(&root, &mut cache, &mut callbacks);
            self.surface_pixels = cache;
            self.pending_callbacks.append(&mut callbacks);
            if let Some((width, height, pixels)) = buffer {
                let _ = self.events.send(Event::PopupBuffer {
                    id,
                    parent,
                    ox: offset.0,
                    oy: offset.1,
                    width,
                    height,
                    pixels,
                });
            }
            return;
        }

        // Unknown root (a surface that hasn't taken a role yet, or an orphan
        // subsurface). Still drain its frame callbacks so the client isn't left
        // blocked.
        with_states(surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            callbacks.append(&mut guard.current().frame_callbacks);
        });
        self.pending_callbacks.append(&mut callbacks);
    }
}

/// Walk up the subsurface parent chain to the root `wl_surface` of a tree.
fn root_surface(surface: &WlSurface) -> WlSurface {
    use smithay::wayland::compositor::get_parent;
    let mut current = surface.clone();
    while let Some(parent) = get_parent(&current) {
        current = parent;
    }
    current
}

/// Flatten a surface tree (a root plus its subsurfaces) into a single
/// tightly-packed RGBA8 buffer sized to the root's buffer, draining every
/// surface's frame callbacks into `callbacks`. `cache` holds the last-known
/// pixels of each surface so subsurfaces that don't re-attach every frame stay
/// visible; dead surfaces are pruned.
fn composite_tree(
    root: &WlSurface,
    cache: &mut std::collections::HashMap<WlSurface, (u32, u32, Vec<u8>)>,
    callbacks: &mut Vec<WlCallback>,
) -> Option<(u32, u32, Vec<u8>)> {
    use smithay::reexports::wayland_server::Resource;
    use smithay::wayland::compositor::{
        with_surface_tree_downward, SubsurfaceCachedState, TraversalAction,
    };

    cache.retain(|s, _| s.is_alive());

    // (location, surface) for every surface with pixels, in render order
    // (parent before child).
    let mut draw: Vec<((i32, i32), WlSurface)> = Vec::new();

    with_surface_tree_downward(
        root,
        (0i32, 0i32),
        |_surface, states, &location| {
            let off = states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location;
            TraversalAction::DoChildren((location.0 + off.x, location.1 + off.y))
        },
        |surface, states, &location| {
            let off = states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location;
            let pos = (location.0 + off.x, location.1 + off.y);

            let new_buffer = {
                let mut guard = states.cached_state.get::<SurfaceAttributes>();
                let attrs = guard.current();
                callbacks.append(&mut attrs.frame_callbacks);
                attrs.buffer.take()
            };

            match new_buffer {
                Some(BufferAssignment::NewBuffer(buffer)) => {
                    if let Ok(Some(frame)) = shm_with_buffer_contents(&buffer, read_shm) {
                        cache.insert(surface.clone(), frame);
                    }
                }
                Some(BufferAssignment::Removed) => {
                    cache.remove(surface);
                }
                None => {}
            }

            if cache.contains_key(surface) {
                draw.push((pos, surface.clone()));
            }
        },
        |_, _, _| true,
    );

    let (cw, ch, _) = cache.get(root)?;
    let (cw, ch) = (*cw as usize, *ch as usize);
    let mut canvas = vec![0u8; cw * ch * 4];
    for (pos, surface) in &draw {
        if let Some((w, h, pixels)) = cache.get(surface) {
            focuswm_render::blit_over(
                &mut canvas, cw, ch, pos.0, pos.1, pixels, *w as usize, *h as usize,
            );
        }
    }
    Some((cw as u32, ch as u32, canvas))
}

fn read_shm(ptr: *const u8, len: usize, data: BufferData) -> Option<(u32, u32, Vec<u8>)> {
    let format = match data.format {
        wl_shm::Format::Argb8888 => ShmFormat::Argb8888,
        wl_shm::Format::Xrgb8888 => ShmFormat::Xrgb8888,
        _ => return None,
    };
    if data.width <= 0 || data.height <= 0 {
        return None;
    }
    let offset = data.offset.max(0) as usize;
    if offset > len {
        return None;
    }
    // SAFETY: `ptr` is valid for `len` bytes for the duration of this callback,
    // and we only read within `[offset, len)`.
    let slice = unsafe { std::slice::from_raw_parts(ptr.add(offset), len - offset) };
    let rgba = convert_to_rgba(
        slice,
        data.width as usize,
        data.height as usize,
        data.stride as usize,
        format,
    );
    Some((data.width as u32, data.height as u32, rgba))
}

fn read_title(surface: &WlSurface) -> String {
    with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().unwrap().title.clone())
            .unwrap_or_default()
    })
}

fn read_app_id(surface: &WlSurface) -> String {
    with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().unwrap().app_id.clone())
            .unwrap_or_default()
    })
}

impl XdgShellHandler for FocusState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Send an initial empty configure so the client can map itself.
        surface.send_configure();

        let id = self.allocate_window_id();
        let wl_surface = surface.wl_surface().clone();
        self.windows.insert(
            wl_surface,
            WindowEntry {
                id,
                toplevel: surface,
            },
        );
        let _ = self.events.send(Event::WindowAdded(id));
        log::info!("new toplevel -> window {id:?}");
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(entry) = self.windows.remove(surface.wl_surface()) {
            let _ = self.events.send(Event::WindowRemoved(entry.id));
            log::info!("toplevel destroyed -> window {:?}", entry.id);
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        let geometry = positioner.get_geometry();
        surface.with_pending_state(|state| {
            state.geometry = geometry;
        });
        let _ = surface.send_configure();

        let parent_id = surface
            .get_parent_surface()
            .and_then(|parent| self.id_of_surface(&parent));
        let id = self.allocate_window_id();
        self.popups.insert(
            surface.wl_surface().clone(),
            PopupEntry {
                id,
                popup: surface,
                parent_id,
                offset: (geometry.loc.x, geometry.loc.y),
            },
        );
        log::info!("new popup {id:?} (parent {parent_id:?})");
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        if let Some(entry) = self.popups.remove(surface.wl_surface()) {
            let _ = self.events.send(Event::PopupRemoved(entry.id));
        }
    }

    fn grab(
        &mut self,
        _surface: PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
    }

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }

    fn move_request(
        &mut self,
        _surface: ToplevelSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
    }

    fn resize_request(
        &mut self,
        _surface: ToplevelSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
        _edges: xdg_toplevel::ResizeEdge,
    ) {
    }
}

impl BufferHandler for FocusState {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for FocusState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl OutputHandler for FocusState {}

impl SeatHandler for FocusState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}

impl SelectionHandler for FocusState {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        _ty: SelectionTarget,
        _source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
    }

    fn send_selection(
        &mut self,
        _ty: SelectionTarget,
        _mime_type: String,
        _fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
    }
}

impl DataDeviceHandler for FocusState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for FocusState {}
impl ServerDndGrabHandler for FocusState {}

delegate_compositor!(FocusState);
delegate_shm!(FocusState);
delegate_xdg_shell!(FocusState);
delegate_seat!(FocusState);
delegate_output!(FocusState);
delegate_data_device!(FocusState);
