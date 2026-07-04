# Compositor status — missing & known-buggy features

A snapshot of the Wayland compositor engine (`crates/focuswm-wayland`) as a
nested Smithay-based shell. focuswm renders each client surface as a texture and
composites them itself; Slint/winit owns the real host output and pointer.

File/line citations are approximate and will drift; treat them as "look here".

## Supported today

`wl_compositor` + `wl_subcompositor`, `wl_shm`, `wl_seat` (keyboard + pointer),
`wl_output` + `xdg-output`, `xdg-shell` (toplevels + popups), `xdg-decoration`
(server-side by default), `xdg-activation`, `wlr-layer-shell`, `wl_data_device`
+ `primary-selection` (with an X11 selection bridge), `idle-inhibit`,
`viewporter` (advertised), `single-pixel-buffer` (advertised), `fractional-scale`
(advertised), `cursor-shape-v1` (named cursors), `linux-dmabuf` (best-effort),
`xwayland-shell` + a first-cut rootless XWayland. Pointer scroll, the Unicode
keymap-priming text path, and client-initiated (CSD) move are wired.

---

## Missing protocols / features

Grouped by user-visible impact. "Missing" = no global is created and there's no
code path.

### Input
- **Pointer lock / confinement** (`zwp_pointer_constraints_v1`) — FPS/3D/CAD apps
  and anything needing a captured cursor can't lock or confine it; the cursor
  escapes the window.
- **Relative pointer motion** (`wp_relative_pointer_manager_v1`) — only absolute
  motion is forwarded (`input.rs` `pointer_motion`), so mouselook / camera
  control in games gets no delta motion.
- **Touch input** — `TouchFocus` is declared but `seat.add_touch()` is never
  called and no touch is forwarded; touchscreens do nothing.
- **IME** (`text-input-v3` / `input-method-v2`) — text is delivered by
  synthesizing keycodes on a mutated xkb keymap, so CJK / compose / candidate
  windows are impossible and there's no pre-edit UI. (This is also why dead-key
  composition is limited.)
- **Keyboard-shortcuts-inhibit** (`zwp_keyboard_shortcuts_inhibit_v1`) — VMs,
  remote-desktop clients and nested sessions can't grab the shell's shortcuts.
- **Tablet** (`tablet_manager_v2`) — the handler is an explicit no-op; styluses
  (pressure/tilt) don't work.

### Output / display
- **Multi-output** — exactly one fixed virtual output (`current_output_size` is a
  single pair); clients never see a second monitor.
- **Output management / gamma / VRR** (`wlr-output-management`, `gamma-control`,
  `tearing-control`, adaptive-sync) — no display configuration, night-light, or
  tearing/VRR for games.
- **Presentation-time** (`wp_presentation`) — frame callbacks fire on a fixed
  16 ms timer with synthetic timestamps, not real vsync feedback; media/games
  can't sync to actual display timing.

### Desktop integration
- **Screencopy / screenshot** (`wlr-screencopy`, `ext-image-copy-capture`) — no
  screenshots or screen sharing/recording via standard protocols.
- **Foreign-toplevel listing** (`wlr-foreign-toplevel-management`,
  `ext-foreign-toplevel-list`) — external taskbars/docks/pagers can't enumerate
  or control windows.
- **Session lock** (`ext-session-lock-v1`) — standard lock screens (swaylock,
  etc.) can't function. (focuswm has its own idle lock UI, but not the protocol.)

---

## Partial / stubbed / buggy

Code path exists but is incomplete or a no-op.

- **Popup grab is a no-op** (`XdgShellHandler::grab`) — popups get no real input
  grab; dismissal relies on the UI sending `DismissPopups`, so click-outside /
  keyboard menu semantics can misbehave.
- **Popup reposition is a no-op** (`reposition_request`) — combo-box / menu
  re-anchoring (`xdg_positioner` reposition) is dropped.
- **Popup positioner constraints approximated** — the UI slides popups back
  inside the content area (`clamp_popup`), but the positioner's `flip`/`resize`
  adjustments aren't resolved, so a slid menu may cover its anchor.
- **Layer-shell `exclusive_zone` and `keyboard_interactivity` ignored** — bars /
  panels don't reserve space (windows overlap them), and launchers / on-screen
  keyboards can't take keyboard focus.
- **`wp_viewport` applied only on buffer commits** — crop/scale is applied when a
  new buffer is attached; a commit that changes only the viewport (no new
  buffer) doesn't re-run it until the next frame arrives.
- **Fractional scale is cosmetic** — always advertises integer scale and blits at
  native pixels, so 125% / 150% HiDPI clients aren't actually scaled.
- **dmabuf import is optimistic** — `dmabuf_imported` always reports success and
  advertises a guessed format set; the real import happens later on the UI
  thread and "may fall back", so some formats can yield blank/garbled windows.
  No dmabuf-feedback. (dmabuf surfaces also bypass the viewport path.)
- **Bitmap cursors fall back to the arrow** — `cursor_image` maps only *named*
  shapes (`cursor-shape-v1`); a client that sets a custom cursor *surface*
  (Blender, some games/editors, older GTK3) shows a plain arrow.
- **No damage tracking** — every commit re-reads the whole buffer, re-composites
  the full tree, and ships a full RGBA copy over the channel; buffer/surface
  damage is never consulted. Correctness is fine; it's CPU/bandwidth-heavy.
- **Scroll is wheel-flavoured only** — smooth values + value120 discrete steps
  are sent, but the source is always `Wheel` (no finger/continuous), with no
  axis-stop events, so touchpad kinetic scrolling doesn't coast.
- **Fullscreen/maximize only resize** — they set the state bit and size to the
  output but don't hide panels/layers, retarget an output, or restack, so a
  "fullscreen" video can still be overlapped.
- **XWayland is first-cut** — interactive resize and `configure_notify` are
  empty and X11 stacking/`Reorder` requests are dropped; XWayland is best-effort
  and may not even spawn if the `Xwayland` binary is absent.
- **`focus_changed` is a no-op** — the compositor emits no focus signal of its
  own; focus visuals/behaviour live entirely in the UI.

---

## Recently fixed (for context)

X11 startup panic (Tokio runtime), stale rendering between clicks, floating
move/resize, non-US / AZERTY typing (keymap priming + hidden-TextInput IME
path), terminal auto-detect fallback, drag-to-reorder + focus-follows-mouse,
sidebar-width off-by-12px, Super-chords leaking as text, minimize-shortcut
toggle, maximize honouring client max-size, `Instant` subtraction panic,
client cursor shapes (`cursor-shape-v1`), **subsurface z-order** (was painted
front-to-back — inverted, not just unordered), **single-pixel-buffer
rendering**, **`wp_viewport` crop/scale** (shm path), **client-initiated
interactive resize** (xdg + X11, driven by the UI like the resize grips),
**popup slide-clamping** into the content area, and **value120 discrete
scroll** steps.
