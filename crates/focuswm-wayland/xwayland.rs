//! XWayland integration: spawn XWayland, run the X11 window manager, and surface
//! X11 windows into the shell's window model so they render and receive input
//! like Wayland toplevels.
//!
//! Rootless, first-cut: X11 clients that use shm buffers compose like any other
//! window. Needs the `Xwayland` binary at runtime; if it's missing the shell
//! simply runs Wayland-only. Set `FOCUSWM_NO_XWAYLAND` to skip it.

use std::process::Stdio;

use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler};

use crate::state::{FocusState, X11Entry};
use crate::Event;

/// Spawn XWayland and wire its readiness into the event loop. Best-effort.
pub fn setup(handle: &LoopHandle<'static, FocusState>, dh: &DisplayHandle) {
    if std::env::var_os("FOCUSWM_NO_XWAYLAND").is_some() {
        log::info!("xwayland: disabled via FOCUSWM_NO_XWAYLAND");
        return;
    }
    let (xwayland, client) = match XWayland::spawn(
        dh,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        Stdio::null(),
        Stdio::null(),
        |_| {},
    ) {
        Ok(pair) => pair,
        Err(err) => {
            log::warn!("xwayland: could not spawn (X11 apps unavailable): {err}");
            return;
        }
    };

    let loop_handle = handle.clone();
    let res = handle.insert_source(
        xwayland,
        move |event, _, state: &mut FocusState| match event {
            XWaylandEvent::Ready {
                x11_socket,
                display_number,
            } => match X11Wm::start_wm(loop_handle.clone(), x11_socket, client.clone()) {
                Ok(wm) => {
                    state.xwm = Some(wm);
                    log::info!("xwayland: ready on DISPLAY=:{display_number}");
                    let _ = state.events.send(Event::XwaylandReady {
                        display: display_number,
                    });
                }
                Err(err) => log::warn!("xwayland: failed to start X11 window manager: {err}"),
            },
            XWaylandEvent::Error => log::warn!("xwayland: server error during startup"),
        },
    );
    if let Err(err) = res {
        log::warn!("xwayland: failed to insert event source: {err}");
    }
}

impl FocusState {
    /// Register a mapped X11 window (with a known `wl_surface`) into the model.
    pub fn register_x11(&mut self, surface: X11Surface) {
        let Some(wl) = surface.wl_surface() else {
            return;
        };
        if self.x11_windows.contains_key(&wl) {
            return;
        }
        let id = self.allocate_window_id();
        self.x11_windows.insert(wl, X11Entry { id, surface });
        let _ = self.events.send(Event::WindowAdded(id));
    }

    fn unregister_x11(&mut self, surface: &X11Surface) {
        let target = surface.window_id();
        let key = self
            .x11_windows
            .iter()
            .find(|(_, e)| e.surface.window_id() == target)
            .map(|(wl, _)| wl.clone());
        if let Some(wl) = key {
            if let Some(entry) = self.x11_windows.remove(&wl) {
                self.surface_pixels.remove(&wl);
                let _ = self.events.send(Event::WindowRemoved(entry.id));
            }
        }
    }
}

impl XwmHandler for FocusState {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("xwm not initialized")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_mapped(true);
    }

    fn map_window_notify(&mut self, _xwm: XwmId, window: X11Surface) {
        self.register_x11(window);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.register_x11(window);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.unregister_x11(&window);
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.unregister_x11(&window);
    }

    #[allow(clippy::too_many_arguments)]
    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let mut geo = window.geometry();
        if let Some(x) = x {
            geo.loc.x = x;
        }
        if let Some(y) = y {
            geo.loc.y = y;
        }
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
        let _ = window.configure(geo);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _button: u32,
        resize_edge: ResizeEdge,
    ) {
        // Mirror the xdg path: hand the interactive resize to the UI, which
        // drives it from the in-flight pointer drag (same edge bitmask as the
        // resize grips: 1=left, 2=right, 4=top, 8=bottom).
        use ResizeEdge as E;
        let edges = match resize_edge {
            E::Left => 1,
            E::Right => 2,
            E::Top => 4,
            E::Bottom => 8,
            E::TopLeft => 4 | 1,
            E::TopRight => 4 | 2,
            E::BottomLeft => 8 | 1,
            E::BottomRight => 8 | 2,
        };
        if let Some(wl) = window.wl_surface() {
            if let Some(entry) = self.x11_windows.get(&wl) {
                let _ = self.events.send(Event::ResizeRequested { id: entry.id, edges });
            }
        }
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32) {
        // Mirror the xdg path: focuswm owns window positions on the UI side, so
        // hand an X11 interactive-move request to the UI to drive from the
        // in-flight pointer drag.
        if let Some(wl) = window.wl_surface() {
            if let Some(entry) = self.x11_windows.get(&wl) {
                let _ = self.events.send(Event::MoveRequested(entry.id));
            }
        }
    }

    // --- Clipboard / primary-selection bridge (X -> Wayland) ---

    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        true
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        let dh = self.display_handle.clone();
        let seat = self.seat.clone();
        match selection {
            SelectionTarget::Clipboard => {
                smithay::wayland::selection::data_device::set_data_device_selection(
                    &dh, &seat, mime_types, (),
                );
            }
            SelectionTarget::Primary => {
                smithay::wayland::selection::primary_selection::set_primary_selection(
                    &dh, &seat, mime_types, (),
                );
            }
        }
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
    ) {
        match selection {
            SelectionTarget::Clipboard => {
                if let Err(err) =
                    smithay::wayland::selection::data_device::request_data_device_client_selection(
                        &self.seat, mime_type, fd,
                    )
                {
                    log::debug!("xwayland: clipboard send to X failed: {err}");
                }
            }
            SelectionTarget::Primary => {
                if let Err(err) =
                    smithay::wayland::selection::primary_selection::request_primary_client_selection(
                        &self.seat, mime_type, fd,
                    )
                {
                    log::debug!("xwayland: primary send to X failed: {err}");
                }
            }
        }
    }
}

impl XWaylandShellHandler for FocusState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, _wl_surface: WlSurface, window: X11Surface) {
        if window.is_mapped() {
            self.register_x11(window);
        }
    }
}

smithay::delegate_xwayland_shell!(FocusState);
