//! Wayland protocol handler implementations.
//!
//! Each `impl` here satisfies a Smithay handler trait and is registered with the
//! matching `delegate_*!` macro at the bottom. The set is deliberately scoped to
//! the foundation milestone: core compositor, `xdg-shell` toplevels + popups,
//! `wl_shm`, seat and data-device (clipboard). Layer-shell, decorations,
//! XWayland and dmabuf are later milestones.

use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
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
use smithay::wayland::shell::wlr_layer::{
    Anchor, Layer, LayerSurface, LayerSurfaceCachedState, Margins, WlrLayerShellHandler,
    WlrLayerShellState,
};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler,
    XdgShellState, XdgToplevelSurfaceData,
};
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::shm::{with_buffer_contents as shm_with_buffer_contents, BufferData, ShmHandler, ShmState};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
};

use focuswm_render::{convert_to_rgba, ShmFormat};

use crate::state::{ClientState, FocusState, LayerEntry, PopupEntry, WindowEntry};
use crate::Event;

impl CompositorHandler for FocusState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // XWayland's own client is created by smithay with its own data type;
        // handle both so the compositor doesn't panic when X11 apps connect.
        if let Some(state) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &state.compositor_state;
        }
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

        // Drag-and-drop icon? Composite it and hand the UI the frame to draw
        // following the cursor (it's not a window/popup/layer/X11 surface).
        if self.dnd_icon.as_ref() == Some(&root) {
            let mut cache = std::mem::take(&mut self.surface_pixels);
            let buffer = composite_tree(&root, &mut cache, &mut callbacks);
            self.surface_pixels = cache;
            self.pending_callbacks.append(&mut callbacks);
            if let Some((width, height, pixels)) = buffer {
                let _ = self.events.send(Event::DragIcon {
                    width,
                    height,
                    pixels,
                    hot_x: 0,
                    hot_y: 0,
                });
            }
            return;
        }

        // Toplevel window?
        if let Some(entry) = self.windows.get(&root) {
            let (id, decorated) = (entry.id, entry.decorated);
            // GPU (dmabuf) buffer? Hand the planes to the UI thread to import.
            if self.dmabuf_enabled {
                if let Some(event) = take_dmabuf(&root, id, &mut callbacks) {
                    self.pending_callbacks.append(&mut callbacks);
                    let _ = self.events.send(event);
                    return;
                }
            }
            let title = read_title(&root);
            let app_id = read_app_id(&root);
            let (min_w, min_h, max_w, max_h) = read_size_hints(&root);
            let mut cache = std::mem::take(&mut self.surface_pixels);
            let buffer = composite_tree(&root, &mut cache, &mut callbacks);
            self.surface_pixels = cache;
            self.pending_callbacks.append(&mut callbacks);
            if let Some(buffer) = buffer {
                // Crop to the client's declared window geometry, dropping any
                // client-side-decoration shadow margin. Record the offset so
                // pointer input maps back to surface-local coordinates.
                let geometry = window_geometry(&root);
                let ((width, height, pixels), offset) = crop_to_geometry(buffer, geometry);
                if let Some(entry) = self.windows.get_mut(&root) {
                    entry.geometry_offset = offset;
                }
                let _ = self.events.send(Event::WindowBuffer {
                    id,
                    width,
                    height,
                    pixels,
                    title,
                    app_id,
                    decorated,
                    min_w,
                    min_h,
                    max_w,
                    max_h,
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

        // Layer-shell surface (bar, wallpaper, notification)?
        if let Some(entry) = self.layer_surfaces.get(&root) {
            let (id, layer) = (entry.id, entry.layer);
            let mut cache = std::mem::take(&mut self.surface_pixels);
            let buffer = composite_tree(&root, &mut cache, &mut callbacks);
            self.surface_pixels = cache;
            self.pending_callbacks.append(&mut callbacks);
            if let Some((width, height, pixels)) = buffer {
                let (anchor, margin) = with_states(&root, |states| {
                    let mut guard = states.cached_state.get::<LayerSurfaceCachedState>();
                    let state = guard.current();
                    (state.anchor, state.margin)
                });
                let (x, y) = layer_position(
                    self.current_output_size,
                    anchor,
                    margin,
                    width as i32,
                    height as i32,
                );
                let _ = self.events.send(Event::LayerBuffer {
                    id,
                    layer: layer_to_u8(layer),
                    x,
                    y,
                    width,
                    height,
                    pixels,
                });
            }
            return;
        }

        // X11 (XWayland) window?
        if let Some(entry) = self.x11_windows.get(&root) {
            let id = entry.id;
            let title = entry.surface.title();
            let app_id = entry.surface.class();
            let decorated = !entry.surface.is_override_redirect();
            // ICCCM WM_NORMAL_HINTS: 0 on either axis means "unconstrained".
            let min = entry.surface.min_size().unwrap_or_default();
            let max = entry.surface.max_size().unwrap_or_default();
            let (min_w, min_h, max_w, max_h) = (min.w, min.h, max.w, max.h);
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
                    decorated,
                    min_w,
                    min_h,
                    max_w,
                    max_h,
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
        with_surface_tree_upward, SubsurfaceCachedState, TraversalAction,
    };
    use smithay::wayland::viewporter::ViewportCachedState;

    cache.retain(|s, _| s.is_alive());

    // (location, surface) for every surface with pixels, in render order.
    // `upward` walks the tree deepest-first (back-to-front in stacking order,
    // honouring subsurface place_above/below), which is painting order — the
    // `downward` variant is front-to-back, for hit-testing.
    let mut draw: Vec<((i32, i32), WlSurface)> = Vec::new();

    with_surface_tree_upward(
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
                    let frame = shm_with_buffer_contents(&buffer, read_shm)
                        .ok()
                        .flatten()
                        .or_else(|| {
                            // Not shm: a wp_single_pixel_buffer is a 1x1 solid
                            // colour (stretched by the viewport below).
                            smithay::wayland::single_pixel_buffer::get_single_pixel_buffer(&buffer)
                                .ok()
                                .map(|spb| (1, 1, spb.rgba8888().to_vec()))
                        });
                    if let Some(frame) = frame {
                        // wp_viewport: crop to `src` and scale to `dst`.
                        let (src, dst) = {
                            let mut guard = states.cached_state.get::<ViewportCachedState>();
                            let v = guard.current();
                            (v.src, v.dst)
                        };
                        cache.insert(surface.clone(), apply_viewport(frame, src, dst));
                    }
                    // We copied the pixels into an owned buffer above, so the
                    // client's shm buffer can be reused immediately. Without this
                    // release the client's buffer pool drains ("all buffers are
                    // held by the server") and apps like weston-terminal exit.
                    buffer.release();
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

/// Apply a `wp_viewport` to a decoded RGBA frame: crop to the (fractional)
/// `src` rectangle, then scale to the `dst` size, nearest-neighbour. Without a
/// viewport the frame passes through untouched. Degenerate rectangles fall back
/// to the previous stage rather than producing an empty frame.
fn apply_viewport(
    frame: (u32, u32, Vec<u8>),
    src: Option<smithay::utils::Rectangle<f64, smithay::utils::Logical>>,
    dst: Option<smithay::utils::Size<i32, smithay::utils::Logical>>,
) -> (u32, u32, Vec<u8>) {
    let (w, h, pixels) = frame;
    if src.is_none() && dst.is_none() {
        return (w, h, pixels);
    }
    // Crop region in buffer pixels (defaults to the whole buffer).
    let (sx, sy, sw, sh) = match src {
        Some(r) if r.size.w > 0.0 && r.size.h > 0.0 => (r.loc.x, r.loc.y, r.size.w, r.size.h),
        _ => (0.0, 0.0, w as f64, h as f64),
    };
    // Output size (defaults to the integer crop size, per the viewport spec).
    let (dw, dh) = match dst {
        Some(s) if s.w > 0 && s.h > 0 => (s.w as u32, s.h as u32),
        _ => (sw.round().max(1.0) as u32, sh.round().max(1.0) as u32),
    };
    if dw == w && dh == h && sx == 0.0 && sy == 0.0 && sw == w as f64 && sh == h as f64 {
        return (w, h, pixels); // identity
    }
    let mut out = vec![0u8; dw as usize * dh as usize * 4];
    for oy in 0..dh {
        // Sample the centre of each destination pixel within the crop region.
        let fy = sy + (oy as f64 + 0.5) * sh / dh as f64;
        let by = (fy as i64).clamp(0, h as i64 - 1) as usize;
        for ox in 0..dw {
            let fx = sx + (ox as f64 + 0.5) * sw / dw as f64;
            let bx = (fx as i64).clamp(0, w as i64 - 1) as usize;
            let s = (by * w as usize + bx) * 4;
            let d = (oy as usize * dw as usize + ox as usize) * 4;
            out[d..d + 4].copy_from_slice(&pixels[s..s + 4]);
        }
    }
    (dw, dh, out)
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

/// The client's `xdg_toplevel` min/max size hints `(min_w, min_h, max_w, max_h)`
/// in surface logical px; 0 on an axis means unset (no constraint).
fn read_size_hints(surface: &WlSurface) -> (i32, i32, i32, i32) {
    use smithay::wayland::shell::xdg::SurfaceCachedState;
    with_states(surface, |states| {
        let mut guard = states.cached_state.get::<SurfaceCachedState>();
        let s = guard.current();
        (s.min_size.w, s.min_size.h, s.max_size.w, s.max_size.h)
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
                // Assume client-side decorations until the client negotiates
                // server-side ones via xdg-decoration.
                decorated: false,
                geometry_offset: (0, 0),
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
        surface: ToplevelSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
        // Client-side-decorated clients (which draw their own title bar) request
        // an interactive move when the user drags it. focuswm owns window
        // positions on the UI side, so hand the request to the UI, which drives
        // the move from the in-flight pointer drag.
        if let Some(entry) = self.windows.get(surface.wl_surface()) {
            let _ = self.events.send(Event::MoveRequested(entry.id));
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        // A client (usually client-side-decorated) asked for an interactive
        // resize from the in-flight pointer drag. The UI owns window geometry,
        // so hand it the request; it drives the resize like its own edge grips.
        if let Some(entry) = self.windows.get(surface.wl_surface()) {
            let edges = resize_edges_mask(edges);
            if edges != 0 {
                let _ = self.events.send(Event::ResizeRequested { id: entry.id, edges });
            }
        }
    }

    // The UI owns window geometry (the floating frame), so hand (un)maximize
    // requests to it; it answers with `SetMaximized` + `ResizeWindow`, which
    // produce the configure the client is waiting for — sized to the frame's
    // content area rather than the whole output (which includes the sidebar).
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(entry) = self.windows.get(surface.wl_surface()) {
            let _ = self.events.send(Event::MaximizeRequested {
                id: entry.id,
                maximized: true,
            });
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(entry) = self.windows.get(surface.wl_surface()) {
            let _ = self.events.send(Event::MaximizeRequested {
                id: entry.id,
                maximized: false,
            });
        }
    }

    // A client-side-decorated client (e.g. GTK's header-bar minimize button)
    // asked to be minimized. focuswm owns minimize state on the UI side, so hand
    // the request off to the UI to apply.
    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(entry) = self.windows.get(surface.wl_surface()) {
            let _ = self.events.send(Event::MinimizeRequested(entry.id));
        }
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        let size = self.current_output_size;
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.size = Some((size.0.max(1), size.1.max(1)).into());
        });
        surface.send_configure();
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Fullscreen);
        });
        surface.send_configure();
    }
}

impl XdgDecorationHandler for FocusState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        self.set_decoration_mode(&toplevel, DecorationMode::ServerSide);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        // focuswm draws the title bar itself — and that bar is the drag handle
        // for moving floating windows — so always use server-side decorations,
        // even when a client (e.g. weston-terminal) asks for client-side ones.
        // Otherwise the client draws its own title bar, which can't be dragged.
        self.set_decoration_mode(&toplevel, DecorationMode::ServerSide);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.set_decoration_mode(&toplevel, DecorationMode::ServerSide);
    }
}

impl FocusState {
    fn set_decoration_mode(&mut self, toplevel: &ToplevelSurface, mode: DecorationMode) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        toplevel.send_configure();
        let decorated = mode == DecorationMode::ServerSide;
        if let Some(entry) = self.windows.get_mut(toplevel.wl_surface()) {
            entry.decorated = decorated;
            let id = entry.id;
            let _ = self.events.send(Event::WindowDecorated { id, decorated });
        }
    }
}

/// If the root surface committed a GPU (dmabuf) buffer, drain its frame
/// callbacks, dup its plane fds into a [`Event::WindowDmabuf`], and release it.
fn take_dmabuf(
    root: &WlSurface,
    id: crate::WindowId,
    callbacks: &mut Vec<WlCallback>,
) -> Option<Event> {
    use smithay::backend::allocator::Buffer;
    use smithay::wayland::dmabuf::get_dmabuf;
    with_states(root, |states| {
        let mut guard = states.cached_state.get::<SurfaceAttributes>();
        let attrs = guard.current();
        let is_dmabuf = matches!(
            &attrs.buffer,
            Some(BufferAssignment::NewBuffer(b)) if get_dmabuf(b).is_ok()
        );
        if !is_dmabuf {
            return None;
        }
        callbacks.append(&mut attrs.frame_callbacks);
        let buffer = match attrs.buffer.take() {
            Some(BufferAssignment::NewBuffer(b)) => b,
            _ => return None,
        };
        let dmabuf = get_dmabuf(&buffer).ok()?;
        let planes: Vec<crate::DmabufPlane> = dmabuf
            .handles()
            .zip(dmabuf.offsets())
            .zip(dmabuf.strides())
            .filter_map(|((fd, offset), stride)| {
                fd.try_clone_to_owned()
                    .ok()
                    .map(|fd| crate::DmabufPlane { fd, offset, stride })
            })
            .collect();
        let format = dmabuf.format();
        let event = Event::WindowDmabuf {
            id,
            width: dmabuf.width(),
            height: dmabuf.height(),
            fourcc: format.code as u32,
            modifier: u64::from(format.modifier),
            planes,
        };
        buffer.release();
        Some(event)
    })
}

/// The client's declared window geometry (`xdg_surface.set_window_geometry`).
fn window_geometry(surface: &WlSurface) -> Option<Rectangle<i32, Logical>> {
    with_states(surface, |states| {
        states.cached_state.get::<SurfaceCachedState>().current().geometry
    })
}

/// Crop a tightly-packed RGBA8 `(w, h, pixels)` buffer to `geometry` (clamped),
/// returning the cropped buffer and the top-left offset used. A missing or
/// full-buffer geometry is a no-op (offset `(0, 0)`).
fn crop_to_geometry(
    buffer: (u32, u32, Vec<u8>),
    geometry: Option<Rectangle<i32, Logical>>,
) -> ((u32, u32, Vec<u8>), (i32, i32)) {
    let (w, h, pixels) = buffer;
    let Some(rect) = geometry else {
        return ((w, h, pixels), (0, 0));
    };
    let (cw, ch) = (w as i32, h as i32);
    let x0 = rect.loc.x.clamp(0, cw);
    let y0 = rect.loc.y.clamp(0, ch);
    let x1 = (rect.loc.x + rect.size.w).clamp(0, cw);
    let y1 = (rect.loc.y + rect.size.h).clamp(0, ch);
    let nw = x1 - x0;
    let nh = y1 - y0;
    if nw <= 0 || nh <= 0 || (x0 == 0 && y0 == 0 && nw == cw && nh == ch) {
        return ((w, h, pixels), (0, 0));
    }
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    let row_bytes = (nw * 4) as usize;
    for row in 0..nh {
        let src = (((y0 + row) * cw + x0) * 4) as usize;
        let dst = (row * nw * 4) as usize;
        out[dst..dst + row_bytes].copy_from_slice(&pixels[src..src + row_bytes]);
    }
    ((nw as u32, nh as u32, out), (x0, y0))
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

impl WlrLayerShellHandler for FocusState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: LayerSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
        layer: Layer,
        _namespace: String,
    ) {
        // Size the surface from its request, filling 0 dimensions to the output.
        let desired = with_states(surface.wl_surface(), |states| {
            states.cached_state.get::<LayerSurfaceCachedState>().current().size
        });
        let (out_w, out_h) = self.current_output_size;
        let w = if desired.w > 0 { desired.w } else { out_w };
        let h = if desired.h > 0 { desired.h } else { out_h };
        surface.with_pending_state(|state| {
            state.size = Some((w, h).into());
        });
        surface.send_configure();

        let id = self.allocate_window_id();
        let wl = surface.wl_surface().clone();
        self.layer_surfaces
            .insert(wl, LayerEntry { id, surface, layer });
        log::info!("new layer surface {id:?} ({layer:?}) {w}x{h}");
    }

    fn layer_destroyed(&mut self, surface: LayerSurface) {
        if let Some(entry) = self.layer_surfaces.remove(surface.wl_surface()) {
            let _ = self.events.send(Event::LayerRemoved(entry.id));
        }
    }
}

impl smithay::wayland::idle_inhibit::IdleInhibitHandler for FocusState {
    fn inhibit(&mut self, surface: WlSurface) {
        let was_empty = self.idle_inhibitors.is_empty();
        self.idle_inhibitors.insert(surface);
        if was_empty {
            let _ = self.events.send(Event::IdleInhibited(true));
        }
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.idle_inhibitors.remove(&surface);
        if self.idle_inhibitors.is_empty() {
            let _ = self.events.send(Event::IdleInhibited(false));
        }
    }
}

fn layer_to_u8(layer: Layer) -> u8 {
    match layer {
        Layer::Background => 0,
        Layer::Bottom => 1,
        Layer::Top => 2,
        Layer::Overlay => 3,
    }
}

/// Position a layer surface against the output edges per its anchors + margins.
fn layer_position(
    (out_w, out_h): (i32, i32),
    anchor: Anchor,
    margin: Margins,
    w: i32,
    h: i32,
) -> (i32, i32) {
    let x = if anchor.contains(Anchor::RIGHT) && !anchor.contains(Anchor::LEFT) {
        out_w - w - margin.right
    } else {
        margin.left
    };
    let y = if anchor.contains(Anchor::BOTTOM) && !anchor.contains(Anchor::TOP) {
        out_h - h - margin.bottom
    } else {
        margin.top
    };
    (x, y)
}

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
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        // The focused client asked for a cursor. We don't own the host pointer
        // (Slint/winit does), so map the request to a shape code the UI applies
        // to the window's cursor. Bitmap (`Surface`) cursors fall back to the
        // default arrow — modern clients use `cursor-shape-v1` (Named) once we
        // advertise it, which covers the common cases (text, resize, grab, …).
        let _ = self.events.send(Event::CursorShape(cursor_shape_code(&image)));
    }
}

/// Map an `xdg_toplevel` resize edge to the UI's edge bitmask
/// (1=left, 2=right, 4=top, 8=bottom — same encoding as the resize grips).
fn resize_edges_mask(edges: xdg_toplevel::ResizeEdge) -> u32 {
    use xdg_toplevel::ResizeEdge as E;
    match edges {
        E::Left => 1,
        E::Right => 2,
        E::Top => 4,
        E::Bottom => 8,
        E::TopLeft => 4 | 1,
        E::TopRight => 4 | 2,
        E::BottomLeft => 8 | 1,
        E::BottomRight => 8 | 2,
        _ => 0, // None / unknown: nothing to drive
    }
}

/// Map a cursor request to a small stable code shared with the UI (see
/// `cursor-for` in `main.slint`). 0 = default arrow.
fn cursor_shape_code(status: &smithay::input::pointer::CursorImageStatus) -> u32 {
    use smithay::input::pointer::{CursorIcon as C, CursorImageStatus as S};
    match status {
        S::Hidden => 1, // none
        S::Named(icon) => match icon {
            C::Pointer => 2,
            C::Text | C::VerticalText => 3,
            C::Crosshair => 4,
            C::Move | C::AllScroll => 5,
            C::Wait => 6,
            C::Progress => 7,
            C::Help => 8,
            C::NotAllowed => 9,
            C::Grab => 10,
            C::Grabbing => 11,
            C::ColResize => 12,
            C::RowResize => 13,
            C::EwResize | C::EResize | C::WResize => 14,
            C::NsResize | C::NResize | C::SResize => 15,
            C::NeswResize | C::NeResize | C::SwResize => 16,
            C::NwseResize | C::NwResize | C::SeResize => 17,
            C::Alias => 18,
            C::Copy => 19,
            C::NoDrop => 20,
            _ => 0, // Default, ContextMenu, Cell, ZoomIn/Out, …
        },
        S::Surface(_) => 0, // bitmap cursor: fall back to the default arrow
    }
}

impl SelectionHandler for FocusState {
    type SelectionUserData = ();

    /// A Wayland client set a selection: mirror it to X so X11 apps can paste.
    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        if let Some(xwm) = self.xwm.as_mut() {
            let mimes = source.map(|s| s.mime_types());
            if let Err(err) = xwm.new_selection(ty, mimes) {
                log::warn!("xwayland: failed to advertise selection to X: {err}");
            }
        }
    }

    /// A Wayland client reads a selection X owns: ask X to write it.
    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        let handle = self.loop_handle.clone();
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.send_selection(ty, mime_type, fd, handle) {
                log::warn!("xwayland: failed to send X selection to Wayland: {err}");
            }
        }
    }
}

impl DataDeviceHandler for FocusState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for FocusState {
    fn started(
        &mut self,
        _source: Option<smithay::reexports::wayland_server::protocol::wl_data_source::WlDataSource>,
        icon: Option<WlSurface>,
        _seat: smithay::input::Seat<Self>,
    ) {
        // Remember the icon surface so its commits get composited and drawn
        // following the cursor; tell the UI to route motion globally so the
        // drag can cross between application windows.
        self.dnd_icon = icon;
        let _ = self.events.send(Event::DragStarted);
    }

    fn dropped(
        &mut self,
        _target: Option<WlSurface>,
        _validated: bool,
        _seat: smithay::input::Seat<Self>,
    ) {
        // Drop the icon's cached pixels along with the reference; a fresh drag
        // brings a fresh icon surface.
        if let Some(icon) = self.dnd_icon.take() {
            self.surface_pixels.remove(&icon);
        }
        let _ = self.events.send(Event::DragEnded);
    }
}
impl ServerDndGrabHandler for FocusState {}

impl smithay::wayland::selection::primary_selection::PrimarySelectionHandler for FocusState {
    fn primary_selection_state(
        &self,
    ) -> &smithay::wayland::selection::primary_selection::PrimarySelectionState {
        &self.primary_selection_state
    }
}

impl smithay::wayland::dmabuf::DmabufHandler for FocusState {
    fn dmabuf_state(&mut self) -> &mut smithay::wayland::dmabuf::DmabufState {
        &mut self.dmabuf_state
    }
    fn dmabuf_imported(
        &mut self,
        _global: &smithay::wayland::dmabuf::DmabufGlobal,
        _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: smithay::wayland::dmabuf::ImportNotifier,
    ) {
        // The real EGLImage import happens on the UI/render thread (which owns the
        // GL context) when the buffer is committed; accept optimistically here.
        let _ = notifier.successful::<FocusState>();
    }
}

delegate_compositor!(FocusState);
delegate_shm!(FocusState);
delegate_xdg_shell!(FocusState);
smithay::delegate_xdg_decoration!(FocusState);
delegate_seat!(FocusState);
delegate_output!(FocusState);
delegate_data_device!(FocusState);
smithay::delegate_primary_selection!(FocusState);
smithay::delegate_dmabuf!(FocusState);
smithay::delegate_cursor_shape!(FocusState);
// Required by `delegate_cursor_shape!` (it also covers tablet tools). We don't
// support tablets, so the defaulted no-op is enough.
impl smithay::wayland::tablet_manager::TabletSeatHandler for FocusState {}
impl smithay::wayland::fractional_scale::FractionalScaleHandler for FocusState {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        // Advertise the output's integer scale as the preferred fractional scale
        // so HiDPI clients render sharply.
        use smithay::wayland::compositor::with_states;
        use smithay::wayland::fractional_scale::with_fractional_scale;
        with_states(&surface, |states| {
            with_fractional_scale(states, |fs| {
                fs.set_preferred_scale(1.0);
            });
        });
    }
}

smithay::delegate_layer_shell!(FocusState);
smithay::delegate_idle_inhibit!(FocusState);
impl smithay::wayland::xdg_activation::XdgActivationHandler for FocusState {
    fn activation_state(&mut self) -> &mut smithay::wayland::xdg_activation::XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        _token: smithay::wayland::xdg_activation::XdgActivationToken,
        _token_data: smithay::wayland::xdg_activation::XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // A client asked for attention/focus: surface it as a task notification.
        if let Some(id) = self.windows.get(&surface).map(|e| e.id) {
            let _ = self.events.send(Event::ActivationRequested(id));
        }
    }
}

smithay::delegate_viewporter!(FocusState);
smithay::delegate_single_pixel_buffer!(FocusState);
smithay::delegate_fractional_scale!(FocusState);
smithay::delegate_xdg_activation!(FocusState);

#[cfg(test)]
mod tests {
    use super::crop_to_geometry;
    use smithay::utils::Rectangle;

    // A 2x2 RGBA image whose pixel (x,y) is the byte (y*2+x) repeated 4 times.
    fn img() -> (u32, u32, Vec<u8>) {
        let px = |v: u8| [v, v, v, v];
        let mut buf = Vec::new();
        for v in 0..4u8 {
            buf.extend_from_slice(&px(v));
        }
        (2, 2, buf)
    }

    #[test]
    fn crop_none_is_noop() {
        let (b, off) = crop_to_geometry(img(), None);
        assert_eq!(b.0, 2);
        assert_eq!(off, (0, 0));
    }

    #[test]
    fn crop_full_size_is_noop() {
        let geo = Rectangle::new((0, 0).into(), (2, 2).into());
        let (b, off) = crop_to_geometry(img(), Some(geo));
        assert_eq!((b.0, b.1), (2, 2));
        assert_eq!(off, (0, 0));
    }

    #[test]
    fn crop_subrect_extracts_column_and_offset() {
        // Right column (x=1), both rows -> pixels for v=1 and v=3.
        let geo = Rectangle::new((1, 0).into(), (1, 2).into());
        let (b, off) = crop_to_geometry(img(), Some(geo));
        assert_eq!((b.0, b.1), (1, 2));
        assert_eq!(off, (1, 0));
        assert_eq!(b.2, vec![1, 1, 1, 1, 3, 3, 3, 3]);
    }

    #[test]
    fn crop_degenerate_is_noop() {
        let geo = Rectangle::new((5, 5).into(), (0, 0).into());
        let (b, off) = crop_to_geometry(img(), Some(geo));
        assert_eq!((b.0, b.1), (2, 2));
        assert_eq!(off, (0, 0));
    }

    #[test]
    fn viewport_passthrough_without_state() {
        let frame = (2u32, 2u32, vec![0u8; 16]);
        let out = super::apply_viewport(frame.clone(), None, None);
        assert_eq!(out, frame);
    }

    #[test]
    fn viewport_scales_to_dst() {
        // 2x1 buffer: left pixel = 1s, right pixel = 2s; stretch to 4x1.
        let frame = (2u32, 1u32, vec![1, 1, 1, 1, 2, 2, 2, 2]);
        let dst = smithay::utils::Size::from((4, 1));
        let (w, h, px) = super::apply_viewport(frame, None, Some(dst));
        assert_eq!((w, h), (4, 1));
        // Nearest-neighbour: two left samples from pixel 0, two right from pixel 1.
        assert_eq!(px, vec![1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2]);
    }

    #[test]
    fn viewport_crops_src() {
        // 2x2 buffer with pixel value = index; crop the right column.
        let (_, _, base) = img();
        let src = smithay::utils::Rectangle::<f64, smithay::utils::Logical>::new(
            (1.0, 0.0).into(),
            (1.0, 2.0).into(),
        );
        let (w, h, px) = super::apply_viewport((2, 2, base), Some(src), None);
        assert_eq!((w, h), (1, 2));
        assert_eq!(px, vec![1, 1, 1, 1, 3, 3, 3, 3]);
    }

    #[test]
    fn resize_edges_mask_maps_all_edges() {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge as E;
        use super::resize_edges_mask;
        assert_eq!(resize_edges_mask(E::Left), 1);
        assert_eq!(resize_edges_mask(E::Right), 2);
        assert_eq!(resize_edges_mask(E::Top), 4);
        assert_eq!(resize_edges_mask(E::Bottom), 8);
        assert_eq!(resize_edges_mask(E::BottomRight), 10);
        assert_eq!(resize_edges_mask(E::TopLeft), 5);
        assert_eq!(resize_edges_mask(E::None), 0);
    }

    #[test]
    fn cursor_shape_codes_map_named_and_hidden() {
        use super::cursor_shape_code;
        use smithay::input::pointer::{CursorIcon, CursorImageStatus};
        let named = |i| cursor_shape_code(&CursorImageStatus::Named(i));
        assert_eq!(cursor_shape_code(&CursorImageStatus::Hidden), 1);
        assert_eq!(named(CursorIcon::Pointer), 2);
        assert_eq!(named(CursorIcon::Text), 3);
        assert_eq!(named(CursorIcon::Grab), 10);
        // The four resize axes collapse onto the bidirectional Slint cursors.
        assert_eq!(named(CursorIcon::EwResize), 14);
        assert_eq!(named(CursorIcon::EResize), 14);
        assert_eq!(named(CursorIcon::WResize), 14);
        assert_eq!(named(CursorIcon::NsResize), 15);
        // Default and unmapped shapes fall back to the default arrow (0).
        assert_eq!(named(CursorIcon::Default), 0);
        assert_eq!(named(CursorIcon::ZoomIn), 0);
    }
}
