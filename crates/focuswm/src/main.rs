//! focuswm — a task-focused Wayland desktop shell.
//!
//! Slint owns the main thread (UI + output + input + GL); the Smithay-based
//! Wayland protocol engine runs on its own thread and reports state changes back
//! over a channel which we drain on the UI thread.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use focuswm_shell::{TaskId, TaskList, WindowId};
use focuswm_wayland::{Command, Event};

mod config;
mod gl_bridge;
mod persist;

use config::SpawnEnv;
use gl_bridge::{Frame, GlBridge};

slint::include_modules!();

/// Default categories offered in the creation wizard.
const DEFAULT_CATEGORIES: &[&str] = &["work", "personal", "meeting", "learning", "other"];

/// Last-known metadata for a client window.
#[derive(Default, Clone)]
struct WinMeta {
    title: String,
    #[allow(dead_code)]
    app_id: String,
}

/// UI-thread state shared between the event pump and the rendering notifier.
#[derive(Default)]
struct Shared {
    /// Per-window metadata for labels.
    meta: HashMap<u64, WinMeta>,
    /// Latest frame per window awaiting GPU upload (drained in the notifier).
    pending: HashMap<u64, Frame>,
    /// Windows removed since the last frame, whose textures must be freed.
    closed: Vec<u64>,
    /// Last-known uploaded texture per window (so switching back to a task shows
    /// the last frame without waiting for a redraw).
    tiles: HashMap<u64, (f32, f32, slint::Image)>,
    /// window id -> its row index in the live `windows` model.
    rows: HashMap<u64, usize>,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // A panic on any thread brings the whole process down rather than leaving a
    // half-dead shell (e.g. a dead Wayland thread → "no windows show").
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        log::error!("fatal: a thread panicked — aborting focuswm");
        std::process::abort();
    }));

    // Spawn the Wayland engine on its own thread.
    let (tx, rx) = channel::<Event>();
    let (cmd_tx, cmd_rx) = focuswm_wayland::command_channel();
    std::thread::Builder::new()
        .name("focuswm-wayland".into())
        .spawn(move || {
            match focuswm_wayland::run(tx, cmd_rx) {
                Ok(()) => log::error!("wayland event loop ended unexpectedly"),
                Err(err) => log::error!("wayland thread exited: {err:?}"),
            }
            std::process::exit(1);
        })?;

    let ui = Desktop::new()?;
    let weak = ui.as_weak();

    // Shared state and models.
    let tasks = Rc::new(RefCell::new(persist::load()));
    let shared = Rc::new(RefCell::new(Shared::default()));
    let bridge = Rc::new(RefCell::new(GlBridge::default()));
    let spawn_env = Rc::new(RefCell::new(SpawnEnv::default()));
    let start = Instant::now();
    let now_secs = move || start.elapsed().as_secs();

    let tasks_model = Rc::new(VecModel::<TaskItem>::default());
    let windows_model = Rc::new(VecModel::<WindowTile>::default());
    ui.global::<AppData>()
        .set_tasks(ModelRc::from(tasks_model.clone()));
    ui.global::<AppData>()
        .set_windows(ModelRc::from(windows_model.clone()));
    ui.global::<AppData>().set_categories(ModelRc::from(Rc::new(
        VecModel::from(
            DEFAULT_CATEGORIES
                .iter()
                .map(|c| SharedString::from(*c))
                .collect::<Vec<_>>(),
        ),
    )));
    ui.global::<AppData>()
        .set_browser_name(config::browser_name().into());

    // --- Model refresh helpers -------------------------------------------------

    // Rebuild the sidebar task model from the TaskList.
    let refresh_tasks: Rc<dyn Fn()> = {
        let weak = weak.clone();
        let tasks = tasks.clone();
        let tasks_model = tasks_model.clone();
        Rc::new(move || {
            let Some(ui) = weak.upgrade() else { return };
            let list = tasks.borrow();
            let items: Vec<TaskItem> = list
                .tasks()
                .iter()
                .map(|t| TaskItem {
                    id: t.id.0 as i32,
                    name: t.name.clone().into(),
                    category: t.category.clone().into(),
                    minutes: (t.accumulated_secs / 60) as i32,
                    has_notification: t.has_notification,
                })
                .collect();
            tasks_model.set_vec(items);
            let active = list.active().map(|t| t.0 as i32).unwrap_or(-1);
            ui.global::<AppData>().set_active_task(active);
            let name = list
                .active()
                .and_then(|id| list.get(id))
                .map(|t| t.name.clone())
                .unwrap_or_default();
            ui.global::<AppData>().set_active_name(name.into());
            let history: Vec<SharedString> =
                list.repo_history().iter().map(|r| r.clone().into()).collect();
            ui.global::<AppData>()
                .set_repo_history(ModelRc::from(Rc::new(VecModel::from(history))));
        })
    };

    // Rebuild the windows model to show exactly the active task's windows.
    let rebuild_windows: Rc<dyn Fn()> = {
        let tasks = tasks.clone();
        let shared = shared.clone();
        let windows_model = windows_model.clone();
        Rc::new(move || {
            let list = tasks.borrow();
            let mut shared = shared.borrow_mut();
            let active_windows = list.active_windows();
            let mut tiles = Vec::new();
            let mut rows = HashMap::new();
            for (row, wid) in active_windows.iter().enumerate() {
                let id = wid.0;
                let title = shared
                    .meta
                    .get(&id)
                    .map(|m| m.title.clone())
                    .unwrap_or_default();
                let (w, h, texture) = shared
                    .tiles
                    .get(&id)
                    .cloned()
                    .unwrap_or((0.0, 0.0, slint::Image::default()));
                tiles.push(WindowTile {
                    id: id as i32,
                    title: title.into(),
                    texture,
                    width: w,
                    height: h,
                });
                rows.insert(id, row);
            }
            shared.rows = rows;
            windows_model.set_vec(tiles);
        })
    };

    // --- Rendering notifier: upload pending frames to GL textures ---------------
    ui.window()
        .set_rendering_notifier({
            let bridge = bridge.clone();
            let shared = shared.clone();
            let windows_model = windows_model.clone();
            move |state, graphics_api| match state {
                slint::RenderingState::RenderingSetup => {
                    if let slint::GraphicsAPI::NativeOpenGL { get_proc_address } = graphics_api {
                        bridge.borrow_mut().init(get_proc_address);
                    }
                }
                slint::RenderingState::BeforeRendering => {
                    let mut bridge = bridge.borrow_mut();
                    if !bridge.ready() {
                        return;
                    }
                    let mut shared = shared.borrow_mut();
                    for id in std::mem::take(&mut shared.closed) {
                        bridge.remove(id);
                    }
                    let frames: Vec<(u64, Frame)> = shared.pending.drain().collect();
                    for (id, frame) in frames {
                        let Some(image) = bridge.upload(id, &frame) else {
                            continue;
                        };
                        let (w, h) = (frame.width as f32, frame.height as f32);
                        shared.tiles.insert(id, (w, h, image.clone()));
                        if let Some(&row) = shared.rows.get(&id) {
                            if let Some(mut tile) = windows_model.row_data(row) {
                                tile.texture = image;
                                tile.width = w;
                                tile.height = h;
                                windows_model.set_row_data(row, tile);
                            }
                        }
                    }
                }
                _ => {}
            }
        })
        .unwrap_or_else(|err| log::error!("could not set rendering notifier: {err:?}"));

    // --- Logic callbacks -------------------------------------------------------

    // Create a task: add it, make it active, and open its terminal.
    ui.global::<Logic>().on_create_task({
        let tasks = tasks.clone();
        let refresh_tasks = refresh_tasks.clone();
        let rebuild_windows = rebuild_windows.clone();
        let spawn_env = spawn_env.clone();
        let now_secs = now_secs.clone();
        let weak = weak.clone();
        move |name, category, branch, repo| {
            {
                let mut list = tasks.borrow_mut();
                let id = list.add_task(name.to_string(), category.to_string());
                if let Some(task) = list.get_mut(id) {
                    if !branch.is_empty() {
                        task.branch = Some(branch.to_string());
                    }
                    if !repo.is_empty() {
                        task.repo = Some(repo.to_string());
                    }
                }
                list.record_repo(repo.as_str());
                list.set_active(id, now_secs());
            }
            refresh_tasks();
            rebuild_windows();
            persist::save(&tasks.borrow());
            // Auto-open a terminal in the new task.
            spawn_terminal(&spawn_env.borrow(), None, &weak);
        }
    });

    ui.global::<Logic>().on_switch_task({
        let tasks = tasks.clone();
        let refresh_tasks = refresh_tasks.clone();
        let rebuild_windows = rebuild_windows.clone();
        let now_secs = now_secs.clone();
        let cmd_tx = cmd_tx.clone();
        move |id| {
            {
                let mut list = tasks.borrow_mut();
                list.set_active(TaskId(id as u64), now_secs());
            }
            refresh_tasks();
            rebuild_windows();
            // Focus the active task's first window, if any.
            let first = tasks.borrow().active_windows().first().copied();
            if let Some(w) = first {
                let _ = cmd_tx.send(Command::FocusWindow(w));
            }
        }
    });

    ui.global::<Logic>().on_delete_task({
        let tasks = tasks.clone();
        let refresh_tasks = refresh_tasks.clone();
        let rebuild_windows = rebuild_windows.clone();
        let now_secs = now_secs.clone();
        let cmd_tx = cmd_tx.clone();
        move |id| {
            // Close that task's windows before forgetting them.
            let windows = tasks.borrow().windows_for(TaskId(id as u64));
            for w in windows {
                let _ = cmd_tx.send(Command::CloseWindow(w));
            }
            tasks.borrow_mut().remove_task(TaskId(id as u64), now_secs());
            refresh_tasks();
            rebuild_windows();
            persist::save(&tasks.borrow());
        }
    });

    ui.global::<Logic>().on_move_task({
        let tasks = tasks.clone();
        let refresh_tasks = refresh_tasks.clone();
        move |id, delta| {
            {
                let mut list = tasks.borrow_mut();
                if let Some(from) = list.tasks().iter().position(|t| t.id.0 == id as u64) {
                    let to = from as i32 + delta;
                    if to >= 0 {
                        list.reorder(from, to as usize);
                    }
                }
            }
            refresh_tasks();
            persist::save(&tasks.borrow());
        }
    });

    ui.global::<Logic>().on_open_terminal({
        let spawn_env = spawn_env.clone();
        let weak = weak.clone();
        move || spawn_terminal(&spawn_env.borrow(), None, &weak)
    });

    ui.global::<Logic>().on_open_browser({
        let spawn_env = spawn_env.clone();
        let weak = weak.clone();
        move || {
            let env = spawn_env.borrow();
            if let Err(err) = config::spawn(&config::browser_command(), &env, None) {
                log::warn!("{err}");
                notify(&weak, &err);
            }
        }
    });

    ui.global::<Logic>().on_close_window({
        let cmd_tx = cmd_tx.clone();
        move |id| {
            let _ = cmd_tx.send(Command::CloseWindow(WindowId(id as u64)));
        }
    });

    ui.global::<Logic>().on_focus_window({
        let cmd_tx = cmd_tx.clone();
        move |id| {
            let _ = cmd_tx.send(Command::FocusWindow(WindowId(id as u64)));
        }
    });

    ui.global::<Logic>().on_pointer_moved({
        let cmd_tx = cmd_tx.clone();
        move |id, x, y| {
            let _ = cmd_tx.send(Command::PointerMotion {
                id: WindowId(id as u64),
                x: x as f64,
                y: y as f64,
            });
        }
    });

    ui.global::<Logic>().on_pointer_button({
        let cmd_tx = cmd_tx.clone();
        move |id, x, y, btn, pressed| {
            let _ = cmd_tx.send(Command::PointerMotion {
                id: WindowId(id as u64),
                x: x as f64,
                y: y as f64,
            });
            let _ = cmd_tx.send(Command::PointerButton {
                id: WindowId(id as u64),
                button: evdev_button(btn),
                pressed,
            });
        }
    });

    ui.global::<Logic>().on_key_event({
        let cmd_tx = cmd_tx.clone();
        move |text, ctrl, alt, shift, pressed| {
            // Only act on press; emit a full modifier-wrapped tap (works for
            // typing; key-repeat arrives as further press events).
            if pressed {
                forward_key(&cmd_tx, text.as_str(), ctrl, alt, shift);
            }
        }
    });

    // Seed the UI from any persisted tasks.
    {
        let mut list = tasks.borrow_mut();
        if list.active().is_none() {
            if let Some(first) = list.tasks().first().map(|t| t.id) {
                list.set_active(first, now_secs());
            }
        }
    }
    refresh_tasks();
    rebuild_windows();

    // --- Event pump: drain compositor events at ~60Hz --------------------------
    let event_timer = slint::Timer::default();
    event_timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), {
        let tasks = tasks.clone();
        let shared = shared.clone();
        let spawn_env = spawn_env.clone();
        let rebuild_windows = rebuild_windows.clone();
        let cmd_tx = cmd_tx.clone();
        let weak = weak.clone();
        move || {
            let mut dirty_windows = false;
            while let Ok(event) = rx.try_recv() {
                match event {
                    Event::Ready {
                        socket_name,
                        runtime_dir,
                        ..
                    } => {
                        *spawn_env.borrow_mut() = SpawnEnv {
                            wayland_display: socket_name,
                            runtime_dir,
                        };
                        log::info!("compositor ready: {:?}", spawn_env.borrow());
                    }
                    Event::WindowAdded(id) => {
                        shared.borrow_mut().meta.entry(id.0).or_default();
                        tasks.borrow_mut().assign_window(id);
                        let _ = cmd_tx.send(Command::FocusWindow(id));
                        dirty_windows = true;
                    }
                    Event::WindowRemoved(id) => {
                        tasks.borrow_mut().remove_window(id);
                        let mut s = shared.borrow_mut();
                        s.meta.remove(&id.0);
                        s.tiles.remove(&id.0);
                        s.closed.push(id.0);
                        dirty_windows = true;
                    }
                    Event::WindowBuffer {
                        id,
                        width,
                        height,
                        pixels,
                        title,
                        app_id,
                    } => {
                        let mut s = shared.borrow_mut();
                        let meta = s.meta.entry(id.0).or_default();
                        if meta.title != title {
                            meta.title = title;
                            dirty_windows = true;
                        }
                        meta.app_id = app_id;
                        s.pending.insert(
                            id.0,
                            Frame {
                                width,
                                height,
                                pixels,
                            },
                        );
                        // First buffer for a window on the active task: ensure it
                        // has a row.
                        if tasks.borrow().is_visible(id) && !s.rows.contains_key(&id.0) {
                            dirty_windows = true;
                        }
                    }
                    Event::PopupBuffer { .. } | Event::PopupRemoved(_) => {
                        // Popups are a later milestone; ignore for now.
                    }
                    Event::OutputResized { .. } => {}
                }
            }
            if dirty_windows {
                rebuild_windows();
            }
            // Keep the compositor output sized to the host window's content area.
            if let Some(ui) = weak.upgrade() {
                sync_output_size(&ui, &cmd_tx, &tasks);
            }
        }
    });

    // --- Periodic time-tracking flush + persist --------------------------------
    let flush_timer = slint::Timer::default();
    flush_timer.start(slint::TimerMode::Repeated, Duration::from_secs(30), {
        let tasks = tasks.clone();
        let refresh_tasks = refresh_tasks.clone();
        let now_secs = now_secs.clone();
        move || {
            tasks.borrow_mut().flush(now_secs());
            refresh_tasks();
            persist::save(&tasks.borrow());
        }
    });

    ui.run()?;

    // Persist a final time snapshot on exit.
    tasks.borrow_mut().flush(now_secs());
    persist::save(&tasks.borrow());
    Ok(())
}

/// Track the host window size and resize the compositor output + active windows
/// to fill the content area (window region right of the sidebar, below header).
fn sync_output_size(
    ui: &Desktop,
    cmd_tx: &focuswm_wayland::CommandSender<Command>,
    tasks: &Rc<RefCell<TaskList>>,
) {
    thread_local! {
        static LAST: RefCell<(i32, i32)> = const { RefCell::new((0, 0)) };
    }
    let size = ui.window().size();
    let scale = ui.window().scale_factor().max(1.0);
    let logical_w = (size.width as f32 / scale) as i32;
    let logical_h = (size.height as f32 / scale) as i32;
    // Content area excludes only the sidebar (240px); there is no top panel.
    let content_w = (logical_w - 240).max(1);
    let content_h = logical_h.max(1);
    let changed = LAST.with(|l| {
        let mut l = l.borrow_mut();
        if *l != (content_w, content_h) {
            *l = (content_w, content_h);
            true
        } else {
            false
        }
    });
    if changed {
        let _ = cmd_tx.send(Command::ResizeOutput {
            width: content_w,
            height: content_h,
        });
        for w in tasks.borrow().active_windows() {
            let _ = cmd_tx.send(Command::ResizeWindow {
                id: w,
                width: content_w,
                height: content_h,
            });
        }
    }
}

/// Spawn the configured terminal, surfacing failures as a UI notification.
fn spawn_terminal(env: &SpawnEnv, cwd: Option<&str>, weak: &slint::Weak<Desktop>) {
    if env.wayland_display.is_empty() {
        log::warn!("terminal requested before the compositor was ready");
        return;
    }
    if let Err(err) = config::spawn(&config::terminal_command(), env, cwd) {
        log::warn!("{err}");
        notify(weak, &err);
    }
}

/// Log a user-facing message (a real on-screen toast is a later milestone).
fn notify(_weak: &slint::Weak<Desktop>, msg: &str) {
    log::warn!("notify: {msg}");
}

/// Map a Slint pointer-button index (1=left, 2=right, 3=middle) to an evdev code.
fn evdev_button(btn: i32) -> u32 {
    match btn {
        1 => 0x110, // BTN_LEFT
        2 => 0x111, // BTN_RIGHT
        3 => 0x112, // BTN_MIDDLE
        _ => 0x110,
    }
}

/// Forward a key tap to the focused client: press any modifiers, tap the key,
/// then release the modifiers.
fn forward_key(
    cmd_tx: &focuswm_wayland::CommandSender<Command>,
    text: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
) {
    let Some((keycode, needs_shift)) = evdev_keycode(text) else {
        return;
    };
    let want_shift = shift || needs_shift;
    let key = |kc: u32, pressed: bool| {
        let _ = cmd_tx.send(Command::Key {
            keycode: kc,
            pressed,
        });
    };
    if ctrl {
        key(29, true);
    } // LEFTCTRL
    if alt {
        key(56, true);
    } // LEFTALT
    if want_shift {
        key(42, true);
    } // LEFTSHIFT
    key(keycode, true);
    key(keycode, false);
    if want_shift {
        key(42, false);
    }
    if alt {
        key(56, false);
    }
    if ctrl {
        key(29, false);
    }
}

/// Best-effort mapping of Slint key text to a US-layout evdev keycode plus
/// whether shift is required to produce it.
fn evdev_keycode(text: &str) -> Option<(u32, bool)> {
    let ch = text.chars().next()?;
    // Control characters from special keys.
    match ch {
        '\u{0008}' => return Some((14, false)),  // Backspace
        '\u{0009}' => return Some((15, false)),  // Tab
        '\u{000a}' | '\u{000d}' => return Some((28, false)), // Return
        '\u{001b}' => return Some((1, false)),   // Escape
        ' ' => return Some((57, false)),         // Space
        _ => {}
    }
    // Letters.
    if ch.is_ascii_alphabetic() {
        let lower = ch.to_ascii_lowercase();
        const ROW: &[(char, u32)] = &[
            ('a', 30), ('b', 48), ('c', 46), ('d', 32), ('e', 18), ('f', 33), ('g', 34),
            ('h', 35), ('i', 23), ('j', 36), ('k', 37), ('l', 38), ('m', 50), ('n', 49),
            ('o', 24), ('p', 25), ('q', 16), ('r', 19), ('s', 31), ('t', 20), ('u', 22),
            ('v', 47), ('w', 17), ('x', 45), ('y', 21), ('z', 44),
        ];
        let kc = ROW.iter().find(|(c, _)| *c == lower).map(|(_, k)| *k)?;
        return Some((kc, ch.is_ascii_uppercase()));
    }
    // Digits and their shifted symbols.
    const DIGITS: &[(char, u32, char)] = &[
        ('1', 2, '!'), ('2', 3, '@'), ('3', 4, '#'), ('4', 5, '$'), ('5', 6, '%'),
        ('6', 7, '^'), ('7', 8, '&'), ('8', 9, '*'), ('9', 10, '('), ('0', 11, ')'),
    ];
    for (d, kc, sym) in DIGITS {
        if ch == *d {
            return Some((*kc, false));
        }
        if ch == *sym {
            return Some((*kc, true));
        }
    }
    // A handful of common punctuation keys.
    const PUNCT: &[(char, u32, bool)] = &[
        ('-', 12, false), ('_', 12, true), ('=', 13, false), ('+', 13, true),
        ('[', 26, false), ('{', 26, true), (']', 27, false), ('}', 27, true),
        (';', 39, false), (':', 39, true), ('\'', 40, false), ('"', 40, true),
        ('`', 41, false), ('~', 41, true), ('\\', 43, false), ('|', 43, true),
        (',', 51, false), ('<', 51, true), ('.', 52, false), ('>', 52, true),
        ('/', 53, false), ('?', 53, true),
    ];
    PUNCT
        .iter()
        .find(|(c, _, _)| *c == ch)
        .map(|(_, kc, sh)| (*kc, *sh))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evdev_letters_and_shift() {
        assert_eq!(evdev_keycode("a"), Some((30, false)));
        assert_eq!(evdev_keycode("A"), Some((30, true)));
        assert_eq!(evdev_keycode("q"), Some((16, false)));
    }

    #[test]
    fn evdev_digits_and_symbols() {
        assert_eq!(evdev_keycode("1"), Some((2, false)));
        assert_eq!(evdev_keycode("!"), Some((2, true)));
        assert_eq!(evdev_keycode("/"), Some((53, false)));
        assert_eq!(evdev_keycode("?"), Some((53, true)));
    }

    #[test]
    fn evdev_special_keys() {
        assert_eq!(evdev_keycode(" "), Some((57, false)));
        assert_eq!(evdev_keycode("\u{000a}"), Some((28, false)));
        assert_eq!(evdev_keycode("\u{0008}"), Some((14, false)));
    }

    #[test]
    fn buttons_map_to_evdev() {
        assert_eq!(evdev_button(1), 0x110);
        assert_eq!(evdev_button(2), 0x111);
        assert_eq!(evdev_button(3), 0x112);
    }
}
