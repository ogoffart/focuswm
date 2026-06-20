//! Forwarding of input (from the Slint UI thread) to the focused Wayland client
//! via the seat, plus window-management commands.

use std::collections::HashMap;

use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::input::keyboard::{FilterResult, Keycode};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::utils::SERIAL_COUNTER;
use xkbcommon::xkb;

use focuswm_shell::WindowId;

use crate::state::FocusState;
use crate::{Command, Event};

/// Lets the focused client receive arbitrary composed Unicode characters —
/// accents, AltGr layers, dead-key results — independent of the host keyboard
/// layout. The host (the UI toolkit) already does the layout + composition work
/// and hands us the final text; we just need to deliver that exact character.
///
/// Approach (à la `wtype`): take the base US keymap and append one keycode
/// "slot" per character, mapping that slot to the character's Unicode keysym.
///
/// Crucially, the keymap is **primed once at startup** with a broad range of
/// common characters (ASCII + Latin-1 + Latin Extended-A + a few symbols) and
/// installed before any client connects, so clients receive it normally on
/// focus and the keymap never changes underneath them while they're focused —
/// swapping a focused client's keymap mid-stream wedges some toolkits (GTK)
/// until they're re-focused. Characters outside the primed set still fall back
/// to extending the keymap on demand, which is rare for Latin-script text.
#[derive(Default)]
pub struct TextInput {
    /// Base keymap (xkb text format) we append Unicode slots to; empty until
    /// primed (or if compiling the base layout failed).
    base: String,
    /// First xkb keycode used for a slot (just past the base keymap's maximum).
    slot_base: u32,
    /// Distinct characters seen so far; the index is the slot offset.
    chars: Vec<char>,
    /// Already-assigned `char -> xkb keycode`.
    by_char: HashMap<char, u32>,
    /// Set when `chars` grew and the keymap needs re-uploading.
    dirty: bool,
}

/// Maximum number of distinct characters we'll assign slots for (keeps keycode
/// names within xkb's 4-character limit: `Z000`..`ZFFF`).
const MAX_SLOTS: usize = 0xFFF;

/// Characters to pre-assign slots for at startup so typical (Latin-script) text
/// never forces a mid-session keymap change: printable ASCII, the Latin-1
/// Supplement and Latin Extended-A (European accents, œ/æ, …), plus the common
/// "smart" punctuation and the euro sign.
fn primed_chars() -> Vec<char> {
    let mut chars: Vec<char> = Vec::new();
    chars.extend((0x20u32..=0x7E).filter_map(char::from_u32)); // printable ASCII
    chars.extend((0xA0u32..=0xFF).filter_map(char::from_u32)); // Latin-1 Supplement
    chars.extend((0x100u32..=0x17F).filter_map(char::from_u32)); // Latin Extended-A
    chars.extend(
        [
            0x20AC, // €
            0x2013, 0x2014, // – —
            0x2018, 0x2019, // ‘ ’
            0x201C, 0x201D, // “ ”
            0x2026, // …
        ]
        .into_iter()
        .filter_map(char::from_u32),
    );
    chars
}

impl TextInput {
    /// Compile the base US keymap once. Returns false if it can't be compiled.
    fn ensure_base(&mut self) -> bool {
        if !self.base.is_empty() {
            return true;
        }
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        // Empty names == the system default layout, matching the seat's
        // `XkbConfig::default()`, so base keys (Enter, Ctrl+C, …) behave the
        // same once this keymap takes over.
        let Some(keymap) =
            xkb::Keymap::new_from_names(&ctx, "", "", "", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
        else {
            return false;
        };
        self.base = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        self.slot_base = parse_maximum(&self.base).unwrap_or(255) + 12;
        true
    }

    /// Build the startup keymap with the common characters pre-assigned, to be
    /// installed on the seat before any client connects. `None` if the base
    /// keymap can't be compiled.
    pub fn prime(&mut self) -> Option<String> {
        if !self.ensure_base() {
            return None;
        }
        for c in primed_chars() {
            let _ = self.keycode_for(c);
        }
        self.dirty = false; // the returned keymap already covers them
        Some(build_keymap(&self.base, &self.chars, self.slot_base))
    }

    /// The xkb keycode that types `c`, assigning a new slot if needed. Returns
    /// `None` only if the base keymap can't be compiled or we're out of slots.
    fn keycode_for(&mut self, c: char) -> Option<u32> {
        if let Some(&kc) = self.by_char.get(&c) {
            return Some(kc);
        }
        if !self.ensure_base() || self.chars.len() >= MAX_SLOTS {
            return None;
        }
        let kc = self.slot_base + self.chars.len() as u32;
        self.chars.push(c);
        self.by_char.insert(c, kc);
        self.dirty = true;
        Some(kc)
    }

    /// If a new character was just assigned, the keymap to upload before typing.
    fn take_keymap(&mut self) -> Option<String> {
        if std::mem::take(&mut self.dirty) {
            Some(build_keymap(&self.base, &self.chars, self.slot_base))
        } else {
            None
        }
    }
}

/// Byte range of the numeric value in `maximum = N`, plus the parsed value.
/// Tolerant of whitespace around `=` (xkbcommon's exact formatting may vary).
fn maximum_span(keymap: &str) -> Option<(std::ops::Range<usize>, u32)> {
    let kw = keymap.find("maximum")? + "maximum".len();
    let bytes = keymap.as_bytes();
    let mut i = kw;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'=') {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let value: u32 = keymap[start..i].parse().ok()?;
    Some((start..i, value))
}

/// Parse the `maximum` keycode value from a compiled keymap.
fn parse_maximum(keymap: &str) -> Option<u32> {
    maximum_span(keymap).map(|(_, v)| v)
}

/// Append Unicode keycode slots to a base keymap: declare each slot in the
/// keycodes section (raising `maximum`) and map it to its Unicode keysym in the
/// symbols section. Indentation is irrelevant to the xkb parser, so the inserts
/// are plain lines.
fn build_keymap(base: &str, chars: &[char], slot_base: u32) -> String {
    if chars.is_empty() {
        return base.to_string();
    }
    let new_max = slot_base + chars.len() as u32 + 1;

    // Raise `maximum` to cover the slots by replacing just its number.
    let mut out = base.to_string();
    let keycodes_at = if let Some((span, _)) = maximum_span(&out) {
        out.replace_range(span.clone(), &new_max.to_string());
        // Insert keycode declarations after the end of the maximum line.
        let line_end = out[span.start..]
            .find('\n')
            .map(|n| span.start + n + 1)
            .unwrap_or(span.start);
        Some(line_end)
    } else {
        None
    };

    // Keycode declarations, inserted inside the keycodes block.
    if let Some(at) = keycodes_at {
        let mut keycodes = String::new();
        for i in 0..chars.len() {
            keycodes.push_str(&format!("\t<Z{:03X}> = {};\n", i, slot_base + i as u32));
        }
        out.insert_str(at, &keycodes);
    }

    // Symbol mappings, inserted just before the close of the xkb_symbols block.
    let mut symbols = String::new();
    for (i, c) in chars.iter().enumerate() {
        symbols.push_str(&format!("\tkey <Z{:03X}> {{ [ U{:04X} ] }};\n", i, *c as u32));
    }
    if let Some(close) = symbols_block_close(&out) {
        out.insert_str(close, &symbols);
    }
    out
}

/// Byte index of the `}` that closes the `xkb_symbols` block, found by matching
/// braces from the block's opening `{` (its body contains nested `key { … }`).
fn symbols_block_close(keymap: &str) -> Option<usize> {
    let block = keymap.find("xkb_symbols")?;
    let open = block + keymap[block..].find('{')?;
    let mut depth = 0usize;
    for (i, b) in keymap[open..].bytes().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

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
            Command::TypeText(text) => self.type_text(text),
            Command::ResizeWindow { id, width, height } => self.resize_window(id, width, height),
            Command::SetMaximized { id, maximized } => self.set_window_maximized(id, maximized),
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

    /// Type already-composed Unicode `text` into the focused client: ensure each
    /// character has a keycode slot (re-uploading the keymap when a new one is
    /// introduced), then tap that keycode. This bypasses the host layout
    /// entirely, so accents / AltGr / dead-key results arrive verbatim.
    fn type_text(&mut self, text: String) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        for c in text.chars() {
            let Some(keycode) = self.text_input.keycode_for(c) else {
                continue;
            };
            // A newly seen character needs the extended keymap uploaded first;
            // Wayland delivers it before the key events that follow.
            if let Some(keymap) = self.text_input.take_keymap() {
                if keyboard.set_keymap_from_string(self, keymap).is_err() {
                    continue;
                }
            }
            for state in [KeyState::Pressed, KeyState::Released] {
                let serial = SERIAL_COUNTER.next_serial();
                let time = self.millis_since_start();
                keyboard.input::<(), _>(
                    self,
                    Keycode::new(keycode),
                    state,
                    serial,
                    time,
                    |_, _, _| FilterResult::Forward,
                );
            }
        }
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

    /// Tell a window whether it is maximized: set/clear the xdg `Maximized`
    /// state, sizing it to the output when maximized.
    fn set_window_maximized(&mut self, id: WindowId, maximized: bool) {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
        let size = self.current_output_size;
        if let Some(entry) = self.windows.values().find(|e| e.id == id) {
            let toplevel = entry.toplevel.clone();
            toplevel.with_pending_state(|state| {
                if maximized {
                    state.states.set(xdg_toplevel::State::Maximized);
                    state.size = Some((size.0.max(1), size.1.max(1)).into());
                } else {
                    state.states.unset(xdg_toplevel::State::Maximized);
                }
            });
            toplevel.send_configure();
        } else if let Some(entry) = self.x11_windows.values().find(|e| e.id == id) {
            let _ = entry.surface.set_maximized(maximized);
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

        // Re-configure layer surfaces that span the output (a 0 dimension means
        // "fill"), so bars/wallpapers track the new size.
        use smithay::wayland::compositor::with_states;
        use smithay::wayland::shell::wlr_layer::{LayerSurface, LayerSurfaceCachedState};
        let surfaces: Vec<LayerSurface> = self
            .layer_surfaces
            .values()
            .map(|e| e.surface.clone())
            .collect();
        for surface in surfaces {
            let desired = with_states(surface.wl_surface(), |states| {
                states.cached_state.get::<LayerSurfaceCachedState>().current().size
            });
            if desired.w > 0 && desired.h > 0 {
                continue;
            }
            let w = if desired.w > 0 { desired.w } else { size.0 };
            let h = if desired.h > 0 { desired.h } else { size.1 };
            surface.with_pending_state(|state| {
                state.size = Some((w, h).into());
            });
            surface.send_configure();
        }

        let _ = self.events.send(Event::OutputResized {
            width: size.0,
            height: size.1,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed keymap shaped like xkbcommon's text output, enough to exercise
    /// the keycodes/symbols splicing.
    const BASE: &str = "xkb_keymap {\n\
        xkb_keycodes \"(unnamed)\" {\n\
        \tminimum = 8;\n\
        \tmaximum = 708;\n\
        \t<ESC> = 9;\n\
        };\n\
        xkb_types \"(unnamed)\" {\n\
        };\n\
        xkb_compat \"(unnamed)\" {\n\
        };\n\
        xkb_symbols \"(unnamed)\" {\n\
        \tkey <ESC> { [ Escape ] };\n\
        };\n\
        };\n";

    #[test]
    fn maximum_parsed_tolerating_spacing() {
        assert_eq!(parse_maximum(BASE), Some(708));
        assert_eq!(parse_maximum("\tmaximum=42;\n"), Some(42));
        assert_eq!(parse_maximum("  maximum   =   7 ;"), Some(7));
        assert_eq!(parse_maximum("no maximum here"), None);
    }

    #[test]
    fn symbols_block_close_skips_nested_braces() {
        let close = symbols_block_close(BASE).expect("symbols block close");
        // The brace it finds must close xkb_symbols: everything after is just the
        // outer keymap close.
        assert_eq!(BASE[close..].trim_start_matches('}').trim(), "};");
        // And the inner `key { … }` braces must not have tripped it up.
        assert!(BASE[..close].contains("key <ESC>"));
    }

    #[test]
    fn build_keymap_appends_valid_looking_slots() {
        let km = build_keymap(BASE, &['a', 'é'], 720);
        // Maximum was raised to cover the two slots (720, 721).
        assert_eq!(parse_maximum(&km), Some(722));
        // Slot keycodes declared in the keycodes block (before xkb_types).
        let kc_a = km.find("<Z000> = 720;").expect("slot a keycode");
        let kc_e = km.find("<Z001> = 721;").expect("slot é keycode");
        assert!(kc_a < km.find("xkb_types").unwrap());
        assert!(kc_e < km.find("xkb_types").unwrap());
        // Symbols map the slots to the right Unicode keysyms, inside xkb_symbols.
        let sym_a = km.find("key <Z000> { [ U0061 ] };").expect("slot a symbol");
        let sym_e = km.find("key <Z001> { [ U00E9 ] };").expect("slot é symbol");
        let sym_block = km.find("xkb_symbols").unwrap();
        assert!(sym_a > sym_block && sym_e > sym_block);
    }

    #[test]
    fn build_keymap_noop_without_chars() {
        assert_eq!(build_keymap(BASE, &[], 720), BASE);
    }
}
