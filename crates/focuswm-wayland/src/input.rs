//! Forwarding of input (from the Slint UI thread) to the focused Wayland client
//! via the seat, plus window-management commands.

use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::input::keyboard::{FilterResult, Keycode};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::utils::SERIAL_COUNTER;

use focuswm_shell::WindowId;

use crate::state::FocusState;
use crate::{Command, Event};

impl FocusState {
    /// Apply a command coming from the UI thread.
    pub fn handle_command(&mut self, command: Command) {
        match command {
            Command::CloseWindow(id) => {
                if let Some(entry) = self.windows.values().find(|e| e.id == id) {
                    entry.toplevel.send_close();
                } else if let Some(entry) = self.x11_windows.values().find(|e| e.id == id) {
                    let _ = entry.surface.close();
                }
            }
            Command::FocusWindow(id) => self.focus_window(id),
            Command::PointerMotion { id, x, y } => self.pointer_motion(id, x, y),
            Command::PointerButton {
                id,
                button,
                pressed,
            } => self.pointer_button(id, button, pressed),
            Command::PointerLeave => self.pointer_leave(),
            Command::PointerAxis { id, dx, dy } => self.pointer_axis(id, dx, dy),
            Command::Key { keycode, pressed } => self.key_input(keycode, pressed),
            Command::ResizeWindow { id, width, height } => self.resize_window(id, width, height),
            Command::ResizeOutput { width, height } => self.resize_output(width, height),
            Command::DismissPopups => {
                for entry in self.popups.values() {
                    entry.popup.send_popup_done();
                }
            }
        }
    }

    fn focus_window(&mut self, id: WindowId) {
        let Some(surface) = self.surface_for(id) else {
            return;
        };
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        self.set_selection_focus(&surface);
        keyboard.set_focus(self, Some(surface), serial);
    }

    fn pointer_motion(&mut self, id: WindowId, x: f64, y: f64) {
        let Some(surface) = self.surface_for(id) else {
            return;
        };
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.millis_since_start();
        // The displayed buffer is cropped to the window geometry, so its (0,0) is
        // at `geometry_offset` within the surface; shift coordinates back so the
        // client receives correct surface-local positions.
        let (ox, oy) = self
            .windows
            .values()
            .find(|e| e.id == id)
            .map(|e| e.geometry_offset)
            .unwrap_or((0, 0));
        pointer.motion(
            self,
            Some((surface, (0.0, 0.0).into())),
            &MotionEvent {
                location: (x + ox as f64, y + oy as f64).into(),
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    fn pointer_button(&mut self, id: WindowId, button: u32, pressed: bool) {
        let Some(surface) = self.surface_for(id) else {
            return;
        };
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.millis_since_start();

        // Clicking a window also gives it keyboard + selection focus.
        if pressed {
            self.set_selection_focus(&surface);
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, Some(surface), serial);
            }
        }

        pointer.button(
            self,
            &ButtonEvent {
                serial,
                time,
                button,
                state: if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
            },
        );
        pointer.frame(self);
    }

    fn pointer_leave(&mut self) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.millis_since_start();
        pointer.motion(
            self,
            None,
            &MotionEvent {
                location: (0.0, 0.0).into(),
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    fn pointer_axis(&mut self, _id: WindowId, dx: f64, dy: f64) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let time = self.millis_since_start();
        let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
        if dx != 0.0 {
            frame = frame.value(Axis::Horizontal, dx);
        }
        if dy != 0.0 {
            frame = frame.value(Axis::Vertical, dy);
        }
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    fn key_input(&mut self, keycode: u32, pressed: bool) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.millis_since_start();
        // evdev keycode -> xkb keycode is `+ 8`.
        keyboard.input::<(), _>(
            self,
            Keycode::new(keycode + 8),
            if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            },
            serial,
            time,
            |_, _, _| FilterResult::Forward,
        );
    }

    fn resize_window(&mut self, id: WindowId, width: i32, height: i32) {
        if let Some(entry) = self.windows.values().find(|e| e.id == id) {
            entry.toplevel.with_pending_state(|state| {
                state.size = Some((width.max(1), height.max(1)).into());
            });
            entry.toplevel.send_configure();
        } else if let Some(entry) = self.x11_windows.values().find(|e| e.id == id) {
            let mut geo = entry.surface.geometry();
            geo.size = (width.max(1), height.max(1)).into();
            let _ = entry.surface.configure(geo);
        }
    }

    /// Resize the output to `width`x`height` (logical px), so clients learn the
    /// new screen size when the host window is resized (nested).
    pub fn resize_output(&mut self, width: i32, height: i32) {
        let size = (width.max(1), height.max(1));
        if self.current_output_size == size {
            return;
        }
        self.current_output_size = size;
        let mode = smithay::output::Mode {
            size: size.into(),
            refresh: 60_000,
        };
        self.output.change_current_state(Some(mode), None, None, None);
        self.output.set_preferred(mode);
        let _ = self.events.send(Event::OutputResized {
            width: size.0,
            height: size.1,
        });
    }
}
