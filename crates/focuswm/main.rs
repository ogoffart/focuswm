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

use chrono::{Datelike, Duration as ChronoDuration, Local};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use focuswm_shell::{Settings, TaskId, TaskList, WindowId};
use focuswm_wayland::{Command, Event};

mod config;
mod gl_bridge;
mod notify;
mod persist;
mod tray;

use notify::NotifyEvent;
use tray::{TrayCommand, TrayUpdate};

use config::SpawnEnv;
use gl_bridge::{DmabufFrame, Frame, GlBridge};

slint::include_modules!();

/// A live on-screen notification toast.
struct ToastState {
    id: u32,
    app: String,
    summary: String,
    body: String,
    /// When to auto-remove it; `None` = sticky.
    deadline: Option<Instant>,
}

/// Last-known metadata for a client window.
#[derive(Default, Clone)]
struct WinMeta {
    title: String,
    #[allow(dead_code)]
    app_id: String,
    /// Whether the compositor should draw a server-side decoration title bar.
    decorated: bool,
}

/// UI-thread state shared between the event pump and the rendering notifier.
#[derive(Default)]
struct Shared {
    /// Per-window metadata for labels.
    meta: HashMap<u64, WinMeta>,
    /// Latest shm frame per window awaiting GPU upload (drained in the notifier).
    pending: HashMap<u64, Frame>,
    /// Latest dmabuf (GPU) frame per window awaiting EGLImage import.
    pending_dmabuf: HashMap<u64, DmabufFrame>,
    /// Windows removed since the last frame, whose textures must be freed.
    closed: Vec<u64>,
    /// Last-known uploaded texture per window (so switching back to a task shows
    /// the last frame without waiting for a redraw).
    tiles: HashMap<u64, (f32, f32, slint::Image)>,
    /// window id -> its row index in the live `windows` model.
    rows: HashMap<u64, usize>,
    /// popup id -> its row index in the live `popups` model.
    popup_rows: HashMap<u64, usize>,
    /// layer-surface id -> its row index in the live `layers` model.
    layer_rows: HashMap<u64, usize>,
    /// A client is inhibiting idle (e.g. a video player); suppress the idle lock.
    idle_inhibited: bool,
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

    // Idle detection: any user input refreshes `last_activity`; the tracking
    // timer pauses accrual after the configured idle timeout.
    let last_activity = Rc::new(RefCell::new(Instant::now()));
    let mark_active: Rc<dyn Fn()> = {
        let last_activity = last_activity.clone();
        Rc::new(move || *last_activity.borrow_mut() = Instant::now())
    };

    // The day the report is focused on (the week follows it). Defaults to today.
    let report_anchor = Rc::new(RefCell::new(Local::now().date_naive()));
    // The window with keyboard focus (drives Alt+Tab and "close window").
    let focused: Rc<RefCell<Option<WindowId>>> = Rc::new(RefCell::new(None));

    let tasks_model = Rc::new(VecModel::<TaskItem>::default());
    let windows_model = Rc::new(VecModel::<WindowTile>::default());
    let popups_model = Rc::new(VecModel::<PopupTile>::default());
    let layers_model = Rc::new(VecModel::<LayerTile>::default());
    ui.global::<AppData>()
        .set_tasks(ModelRc::from(tasks_model.clone()));
    ui.global::<AppData>()
        .set_windows(ModelRc::from(windows_model.clone()));
    ui.global::<AppData>()
        .set_popups(ModelRc::from(popups_model.clone()));
    ui.global::<AppData>()
        .set_layers(ModelRc::from(layers_model.clone()));
    let notifications_model = Rc::new(VecModel::<NotificationToast>::default());
    ui.global::<AppData>()
        .set_notifications(ModelRc::from(notifications_model.clone()));

    // Notification toasts: the freedesktop daemon and internal messages feed one
    // channel; toasts auto-expire. `toasts` is the source of truth.
    let (toast_tx, toast_rx) = async_channel::unbounded::<NotifyEvent>();
    notify::spawn(toast_tx.clone());
    // System tray: host SNI on its own thread; icons feed a model.
    let tray_model = Rc::new(VecModel::<TrayIcon>::default());
    ui.global::<AppData>()
        .set_tray(ModelRc::from(tray_model.clone()));
    let (tray_rx, tray_cmd_tx) = match tray::run() {
        Some((rx, tx)) => (Some(rx), Some(tx)),
        None => (None, None),
    };
    let tray_items: Rc<RefCell<Vec<TrayIcon>>> = Rc::new(RefCell::new(Vec::new()));

    let toasts: Rc<RefCell<Vec<ToastState>>> = Rc::new(RefCell::new(Vec::new()));
    let refresh_toasts: Rc<dyn Fn()> = {
        let toasts = toasts.clone();
        let notifications_model = notifications_model.clone();
        Rc::new(move || {
            let items: Vec<NotificationToast> = toasts
                .borrow()
                .iter()
                .map(|t| NotificationToast {
                    id: t.id as i32,
                    app: t.app.clone().into(),
                    summary: t.summary.clone().into(),
                    body: t.body.clone().into(),
                })
                .collect();
            notifications_model.set_vec(items);
        })
    };
    ui.global::<AppData>()
        .set_browser_name(config::browser_name().into());

    // Publish the colour palette to the task-settings dialog (static; the chosen
    // swatch is identified by its index).
    let palette: Vec<slint::Color> = focuswm_shell::task_palette()
        .iter()
        .enumerate()
        .map(|(i, c)| task_tint(c, i))
        .collect();
    ui.global::<TaskSettingsData>()
        .set_palette(ModelRc::from(Rc::new(VecModel::from(palette))));

    // Publish the configured category list to the wizard.
    let apply_categories: Rc<dyn Fn()> = {
        let weak = weak.clone();
        let tasks = tasks.clone();
        Rc::new(move || {
            let Some(ui) = weak.upgrade() else { return };
            let cats: Vec<SharedString> = tasks
                .borrow()
                .settings()
                .categories
                .iter()
                .map(|c| c.clone().into())
                .collect();
            ui.global::<AppData>()
                .set_categories(ModelRc::from(Rc::new(VecModel::from(cats))));
        })
    };
    apply_categories();

    // Effective terminal/browser commands: the configured one, or auto-detect.
    let terminal_cmd: Rc<dyn Fn() -> Vec<String>> = {
        let tasks = tasks.clone();
        Rc::new(move || {
            let t = tasks.borrow().settings().terminal.trim().to_string();
            if t.is_empty() {
                config::terminal_command()
            } else {
                config::split_command(&t)
            }
        })
    };
    let browser_cmd: Rc<dyn Fn() -> Vec<String>> = {
        let tasks = tasks.clone();
        Rc::new(move || {
            let b = tasks.borrow().settings().browser.trim().to_string();
            if b.is_empty() {
                config::browser_command()
            } else {
                config::split_command(&b)
            }
        })
    };

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
                .enumerate()
                .map(|(i, t)| TaskItem {
                    id: t.id.0 as i32,
                    name: t.name.clone().into(),
                    category: t.category.clone().into(),
                    minutes: (t.accumulated_secs / 60) as i32,
                    has_notification: t.has_notification,
                    tint: task_tint(&t.color, i),
                })
                .collect();
            tasks_model.set_vec(items);
            let active = list.active().map(|t| t.0 as i32).unwrap_or(-1);
            ui.global::<AppData>().set_active_task(active);
            let name = list
                .active()
                .and_then(|id| list.get(id))
                .map(|t| t.name.clone())
                .unwrap_or_else(|| "Desktop 0".to_string());
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
        let focused = focused.clone();
        Rc::new(move || {
            let list = tasks.borrow();
            let mut shared = shared.borrow_mut();
            let mut active_windows = list.active_windows();
            // Render the focused window last so it stacks on top.
            if let Some(f) = *focused.borrow() {
                if let Some(pos) = active_windows.iter().position(|w| *w == f) {
                    let w = active_windows.remove(pos);
                    active_windows.push(w);
                }
            }
            let mut tiles = Vec::new();
            let mut rows = HashMap::new();
            for (row, wid) in active_windows.iter().enumerate() {
                let id = wid.0;
                let (title, decorated) = shared
                    .meta
                    .get(&id)
                    .map(|m| (m.title.clone(), m.decorated))
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
                    decorated,
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
            let popups_model = popups_model.clone();
            let layers_model = layers_model.clone();
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
                    // GPU (dmabuf) frames first: import as EGLImage textures.
                    let dmabufs: Vec<(u64, DmabufFrame)> =
                        shared.pending_dmabuf.drain().collect();
                    for (id, frame) in dmabufs {
                        let Some(image) = bridge.import_dmabuf(id, &frame) else {
                            continue;
                        };
                        let (w, h) = (frame.width as f32, frame.height as f32);
                        place_texture(&mut shared, &windows_model, &popups_model, &layers_model, id, image, w, h);
                    }
                    // shm frames.
                    let frames: Vec<(u64, Frame)> = shared.pending.drain().collect();
                    for (id, frame) in frames {
                        let Some(image) = bridge.upload(id, &frame) else {
                            continue;
                        };
                        let (w, h) = (frame.width as f32, frame.height as f32);
                        place_texture(&mut shared, &windows_model, &popups_model, &layers_model, id, image, w, h);
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
        let terminal_cmd = terminal_cmd.clone();
        let mark_active = mark_active.clone();
        let toast_tx = toast_tx.clone();
        move |name, category, branch, repo| {
            mark_active();
            {
                let mut list = tasks.borrow_mut();
                list.set_date(&today());
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
            spawn_client(&terminal_cmd(), &spawn_env.borrow(), None, &toast_tx);
        }
    });

    ui.global::<Logic>().on_switch_task({
        let tasks = tasks.clone();
        let refresh_tasks = refresh_tasks.clone();
        let rebuild_windows = rebuild_windows.clone();
        let now_secs = now_secs.clone();
        let cmd_tx = cmd_tx.clone();
        let focused = focused.clone();
        let mark_active = mark_active.clone();
        move |id| {
            mark_active();
            {
                let mut list = tasks.borrow_mut();
                list.set_date(&today());
                list.set_active(TaskId(id as u64), now_secs());
            }
            refresh_tasks();
            // Focus the active task's first window, if any.
            let first = tasks.borrow().active_windows().first().copied();
            *focused.borrow_mut() = first;
            rebuild_windows();
            if let Some(w) = first {
                let _ = cmd_tx.send(Command::FocusWindow(w));
            }
        }
    });

    // Switch to desktop 0 (the scratch desktop, not tied to any task).
    ui.global::<Logic>().on_switch_to_desktop0({
        let tasks = tasks.clone();
        let refresh_tasks = refresh_tasks.clone();
        let rebuild_windows = rebuild_windows.clone();
        let now_secs = now_secs.clone();
        let cmd_tx = cmd_tx.clone();
        let mark_active = mark_active.clone();
        move || {
            mark_active();
            {
                let mut list = tasks.borrow_mut();
                list.set_date(&today());
                list.set_scratch_active(now_secs());
            }
            refresh_tasks();
            rebuild_windows();
            // Focus desktop 0's first window, if any.
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
        let mark_active = mark_active.clone();
        move |id| {
            mark_active();
            // Close that task's windows before forgetting them.
            let windows = tasks.borrow().windows_for(TaskId(id as u64));
            for w in windows {
                let _ = cmd_tx.send(Command::CloseWindow(w));
            }
            {
                let mut list = tasks.borrow_mut();
                list.set_date(&today());
                list.remove_task(TaskId(id as u64), now_secs());
            }
            refresh_tasks();
            rebuild_windows();
            persist::save(&tasks.borrow());
        }
    });

    ui.global::<Logic>().on_move_task({
        let tasks = tasks.clone();
        let refresh_tasks = refresh_tasks.clone();
        let mark_active = mark_active.clone();
        move |id, delta| {
            mark_active();
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

    // Task settings dialog: populate the current task's values and open.
    ui.global::<Logic>().on_open_task_settings({
        let tasks = tasks.clone();
        let mark_active = mark_active.clone();
        let weak = weak.clone();
        move |id| {
            mark_active();
            let Some(ui) = weak.upgrade() else { return };
            let list = tasks.borrow();
            let task_id = TaskId(id as u64);
            let Some(pos) = list.tasks().iter().position(|t| t.id == task_id) else {
                return;
            };
            let task = &list.tasks()[pos];
            let categories = &list.settings().categories;
            let palette = focuswm_shell::task_palette();
            let tsd = ui.global::<TaskSettingsData>();
            tsd.set_id(id);
            tsd.set_name(task.name.clone().into());
            tsd.set_category_index(
                categories.iter().position(|c| *c == task.category).unwrap_or(0) as i32,
            );
            // Highlight the swatch matching the task's colour (falling back to the
            // palette colour for its position when unset/custom).
            let color_index = palette
                .iter()
                .position(|c| c.eq_ignore_ascii_case(&task.color))
                .unwrap_or(pos % palette.len());
            tsd.set_selected_index(color_index as i32);
            ui.set_task_settings_open(true);
        }
    });

    ui.global::<Logic>().on_save_task_settings({
        let tasks = tasks.clone();
        let refresh_tasks = refresh_tasks.clone();
        let mark_active = mark_active.clone();
        move |id, name, category, color_index| {
            mark_active();
            let palette = focuswm_shell::task_palette();
            let color = palette
                .get(color_index as usize)
                .cloned()
                .unwrap_or_else(|| palette[0].clone());
            tasks.borrow_mut().set_task_props(
                TaskId(id as u64),
                name.to_string(),
                category.to_string(),
                color,
            );
            persist::save(&tasks.borrow());
            refresh_tasks();
        }
    });

    ui.global::<Logic>().on_open_terminal({
        let spawn_env = spawn_env.clone();
        let terminal_cmd = terminal_cmd.clone();
        let mark_active = mark_active.clone();
        let toast_tx = toast_tx.clone();
        move || {
            mark_active();
            spawn_client(&terminal_cmd(), &spawn_env.borrow(), None, &toast_tx);
        }
    });

    ui.global::<Logic>().on_open_browser({
        let spawn_env = spawn_env.clone();
        let browser_cmd = browser_cmd.clone();
        let mark_active = mark_active.clone();
        let toast_tx = toast_tx.clone();
        move || {
            mark_active();
            spawn_client(&browser_cmd(), &spawn_env.borrow(), None, &toast_tx);
        }
    });

    // Run-command launcher (Alt+F2 / sidebar ▶): spawn an arbitrary command.
    ui.global::<Logic>().on_run_command({
        let spawn_env = spawn_env.clone();
        let mark_active = mark_active.clone();
        let toast_tx = toast_tx.clone();
        move |cmd| {
            mark_active();
            let parts = config::split_command(cmd.as_str());
            if !parts.is_empty() {
                spawn_client(&parts, &spawn_env.borrow(), None, &toast_tx);
            }
        }
    });

    // Settings dialog: populate fields and open.
    ui.global::<Logic>().on_open_settings({
        let tasks = tasks.clone();
        let mark_active = mark_active.clone();
        let weak = weak.clone();
        move || {
            mark_active();
            let Some(ui) = weak.upgrade() else { return };
            let list = tasks.borrow();
            let s = list.settings();
            ui.global::<SettingsData>().set_terminal(s.terminal.clone().into());
            ui.global::<SettingsData>().set_browser(s.browser.clone().into());
            ui.global::<SettingsData>()
                .set_categories_csv(s.categories.join(", ").into());
            ui.global::<SettingsData>()
                .set_idle_minutes(s.idle_minutes.to_string().into());
            ui.set_settings_open(true);
        }
    });

    ui.global::<Logic>().on_save_settings({
        let tasks = tasks.clone();
        let apply_categories = apply_categories.clone();
        let mark_active = mark_active.clone();
        let weak = weak.clone();
        move |terminal, browser, categories_csv, idle_minutes| {
            mark_active();
            let mut cats: Vec<String> = categories_csv
                .split(',')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect();
            if cats.is_empty() {
                cats = focuswm_shell::default_categories();
            }
            let idle = idle_minutes
                .trim()
                .parse::<u64>()
                .unwrap_or_else(|_| focuswm_shell::default_idle_minutes());
            tasks.borrow_mut().set_settings(Settings {
                terminal: terminal.to_string(),
                browser: browser.to_string(),
                categories: cats,
                idle_minutes: idle,
            });
            persist::save(&tasks.borrow());
            apply_categories();
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppData>()
                    .set_browser_name(browser_label(&tasks.borrow()).into());
            }
        }
    });

    // Time report: flush the running interval, reset to today, compute, and open.
    ui.global::<Logic>().on_open_report({
        let tasks = tasks.clone();
        let now_secs = now_secs.clone();
        let report_anchor = report_anchor.clone();
        let mark_active = mark_active.clone();
        let weak = weak.clone();
        move || {
            mark_active();
            {
                let mut list = tasks.borrow_mut();
                list.set_date(&today());
                list.flush(now_secs());
            }
            persist::save(&tasks.borrow());
            *report_anchor.borrow_mut() = Local::now().date_naive();
            if let Some(ui) = weak.upgrade() {
                build_report(&tasks.borrow(), &ui, *report_anchor.borrow());
                ui.set_report_open(true);
            }
        }
    });

    // Report navigation: shift the anchor day/week and rebuild (clamped to today).
    {
        let make_nav = |delta_days: i64| {
            let tasks = tasks.clone();
            let report_anchor = report_anchor.clone();
            let mark_active = mark_active.clone();
            let weak = weak.clone();
            move || {
                mark_active();
                let today_date = Local::now().date_naive();
                let mut anchor = report_anchor.borrow_mut();
                let next = *anchor + ChronoDuration::days(delta_days);
                // Don't navigate into the future.
                *anchor = next.min(today_date);
                if let Some(ui) = weak.upgrade() {
                    build_report(&tasks.borrow(), &ui, *anchor);
                }
            }
        };
        ui.global::<Logic>().on_report_prev_day(make_nav(-1));
        ui.global::<Logic>().on_report_next_day(make_nav(1));
        ui.global::<Logic>().on_report_prev_week(make_nav(-7));
        ui.global::<Logic>().on_report_next_week(make_nav(7));
    }

    // Lock: pause time tracking and raise the lock screen.
    ui.global::<Logic>().on_lock({
        let tasks = tasks.clone();
        let now_secs = now_secs.clone();
        let weak = weak.clone();
        move || {
            {
                let mut list = tasks.borrow_mut();
                list.set_date(&today());
                list.flush(now_secs());
                list.pause();
            }
            persist::save(&tasks.borrow());
            if let Some(ui) = weak.upgrade() {
                ui.set_locked(true);
            }
        }
    });

    // Activate a system-tray item.
    ui.global::<Logic>().on_tray_activate({
        let tray_cmd_tx = tray_cmd_tx.clone();
        move |id| {
            if let Some(tx) = &tray_cmd_tx {
                let _ = tx.send(TrayCommand::Activate(id.to_string()));
            }
        }
    });

    // Alt+Tab: cycle the active task's windows (focused one stacks on top).
    ui.global::<Logic>().on_cycle_window({
        let tasks = tasks.clone();
        let focused = focused.clone();
        let rebuild_windows = rebuild_windows.clone();
        let cmd_tx = cmd_tx.clone();
        let mark_active = mark_active.clone();
        move |forward| {
            mark_active();
            let wins = tasks.borrow().active_windows();
            if wins.is_empty() {
                return;
            }
            let cur = *focused.borrow();
            let idx = cur
                .and_then(|c| wins.iter().position(|w| *w == c))
                .unwrap_or(0);
            let n = wins.len();
            let next = if forward { (idx + 1) % n } else { (idx + n - 1) % n };
            let target = wins[next];
            *focused.borrow_mut() = Some(target);
            let _ = cmd_tx.send(Command::FocusWindow(target));
            rebuild_windows();
        }
    });

    // Super+N: switch to the task at the given index.
    ui.global::<Logic>().on_switch_task_index({
        let tasks = tasks.clone();
        let focused = focused.clone();
        let refresh_tasks = refresh_tasks.clone();
        let rebuild_windows = rebuild_windows.clone();
        let now_secs = now_secs.clone();
        let cmd_tx = cmd_tx.clone();
        let mark_active = mark_active.clone();
        move |i| {
            mark_active();
            let id = tasks.borrow().tasks().get(i as usize).map(|t| t.id);
            if let Some(id) = id {
                {
                    let mut list = tasks.borrow_mut();
                    list.set_date(&today());
                    list.set_active(id, now_secs());
                }
                refresh_tasks();
                let first = tasks.borrow().active_windows().first().copied();
                *focused.borrow_mut() = first;
                rebuild_windows();
                if let Some(w) = first {
                    let _ = cmd_tx.send(Command::FocusWindow(w));
                }
            }
        }
    });

    // Super+W: close the focused window.
    ui.global::<Logic>().on_close_active_window({
        let focused = focused.clone();
        let cmd_tx = cmd_tx.clone();
        let mark_active = mark_active.clone();
        move || {
            mark_active();
            if let Some(w) = *focused.borrow() {
                let _ = cmd_tx.send(Command::CloseWindow(w));
            }
        }
    });

    // Dismiss a notification toast.
    ui.global::<Logic>().on_dismiss_notification({
        let toasts = toasts.clone();
        let refresh_toasts = refresh_toasts.clone();
        move |id| {
            toasts.borrow_mut().retain(|t| t.id != id as u32);
            refresh_toasts();
        }
    });

    // Unlock: count this as activity, resume tracking, and hide the lock screen.
    ui.global::<Logic>().on_unlock({
        let tasks = tasks.clone();
        let now_secs = now_secs.clone();
        let mark_active = mark_active.clone();
        let weak = weak.clone();
        move || {
            mark_active();
            tasks.borrow_mut().resume(now_secs());
            if let Some(ui) = weak.upgrade() {
                ui.set_locked(false);
            }
        }
    });

    ui.global::<Logic>().on_close_window({
        let cmd_tx = cmd_tx.clone();
        let mark_active = mark_active.clone();
        move |id| {
            mark_active();
            let _ = cmd_tx.send(Command::CloseWindow(WindowId(id as u64)));
        }
    });

    ui.global::<Logic>().on_focus_window({
        let cmd_tx = cmd_tx.clone();
        let focused = focused.clone();
        let rebuild_windows = rebuild_windows.clone();
        let mark_active = mark_active.clone();
        move |id| {
            mark_active();
            *focused.borrow_mut() = Some(WindowId(id as u64));
            let _ = cmd_tx.send(Command::FocusWindow(WindowId(id as u64)));
            rebuild_windows();
        }
    });

    ui.global::<Logic>().on_pointer_moved({
        let cmd_tx = cmd_tx.clone();
        let mark_active = mark_active.clone();
        move |id, x, y| {
            mark_active();
            let _ = cmd_tx.send(Command::PointerMotion {
                id: WindowId(id as u64),
                x: x as f64,
                y: y as f64,
            });
        }
    });

    ui.global::<Logic>().on_pointer_button({
        let cmd_tx = cmd_tx.clone();
        let focused = focused.clone();
        let mark_active = mark_active.clone();
        move |id, x, y, btn, pressed| {
            mark_active();
            if pressed {
                *focused.borrow_mut() = Some(WindowId(id as u64));
            }
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
        let mark_active = mark_active.clone();
        move |text, ctrl, alt, shift, pressed| {
            mark_active();
            // Only act on press; emit a full modifier-wrapped tap (works for
            // typing; key-repeat arrives as further press events).
            if pressed {
                forward_key(&cmd_tx, text.as_str(), ctrl, alt, shift);
            }
        }
    });

    // Start on desktop 0 (the scratch desktop); the user picks a task to begin
    // time tracking.
    tasks.borrow_mut().set_date(&today());
    refresh_tasks();
    rebuild_windows();

    // --- Event pump: drain compositor events at ~60Hz --------------------------
    let event_timer = slint::Timer::default();
    event_timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), {
        let tasks = tasks.clone();
        let shared = shared.clone();
        let spawn_env = spawn_env.clone();
        let rebuild_windows = rebuild_windows.clone();
        let refresh_tasks = refresh_tasks.clone();
        let popups_model = popups_model.clone();
        let layers_model = layers_model.clone();
        let toasts = toasts.clone();
        let toast_rx = toast_rx.clone();
        let refresh_toasts = refresh_toasts.clone();
        let tray_model = tray_model.clone();
        let tray_items = tray_items.clone();
        let focused = focused.clone();
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
                        let mut env = spawn_env.borrow_mut();
                        env.wayland_display = socket_name;
                        env.runtime_dir = runtime_dir;
                        log::info!("compositor ready: {env:?}");
                    }
                    Event::WindowAdded(id) => {
                        shared.borrow_mut().meta.entry(id.0).or_default();
                        tasks.borrow_mut().assign_window(id);
                        *focused.borrow_mut() = Some(id);
                        let _ = cmd_tx.send(Command::FocusWindow(id));
                        dirty_windows = true;
                    }
                    Event::WindowRemoved(id) => {
                        tasks.borrow_mut().remove_window(id);
                        let mut s = shared.borrow_mut();
                        s.meta.remove(&id.0);
                        s.tiles.remove(&id.0);
                        s.closed.push(id.0);
                        drop(s);
                        // If the focused window closed, focus another in the task.
                        if *focused.borrow() == Some(id) {
                            let next = tasks.borrow().active_windows().last().copied();
                            *focused.borrow_mut() = next;
                            if let Some(w) = next {
                                let _ = cmd_tx.send(Command::FocusWindow(w));
                            }
                        }
                        dirty_windows = true;
                    }
                    Event::WindowDecorated { id, decorated } => {
                        let mut s = shared.borrow_mut();
                        let meta = s.meta.entry(id.0).or_default();
                        if meta.decorated != decorated {
                            meta.decorated = decorated;
                            dirty_windows = true;
                        }
                    }
                    Event::WindowBuffer {
                        id,
                        width,
                        height,
                        pixels,
                        title,
                        app_id,
                        decorated,
                    } => {
                        let mut s = shared.borrow_mut();
                        let meta = s.meta.entry(id.0).or_default();
                        if meta.title != title || meta.decorated != decorated {
                            meta.title = title;
                            meta.decorated = decorated;
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
                    Event::WindowDmabuf {
                        id,
                        width,
                        height,
                        fourcc,
                        modifier,
                        mut planes,
                    } => {
                        // Single-plane import for now.
                        if let Some(plane) = planes.drain(..).next() {
                            let mut s = shared.borrow_mut();
                            s.pending_dmabuf.insert(
                                id.0,
                                DmabufFrame {
                                    width,
                                    height,
                                    fourcc,
                                    modifier,
                                    fd: plane.fd,
                                    offset: plane.offset,
                                    stride: plane.stride,
                                },
                            );
                            if tasks.borrow().is_visible(id) && !s.rows.contains_key(&id.0) {
                                dirty_windows = true;
                            }
                        }
                    }
                    Event::LayerBuffer {
                        id,
                        layer,
                        x,
                        y,
                        width,
                        height,
                        pixels,
                    } => {
                        let mut s = shared.borrow_mut();
                        if let Some(&row) = s.layer_rows.get(&id.0) {
                            if let Some(mut t) = layers_model.row_data(row) {
                                t.x = x as f32;
                                t.y = y as f32;
                                t.width = width as f32;
                                t.height = height as f32;
                                layers_model.set_row_data(row, t);
                            }
                        } else {
                            let row = layers_model.row_count();
                            layers_model.push(LayerTile {
                                id: id.0 as i32,
                                texture: slint::Image::default(),
                                layer: layer as i32,
                                x: x as f32,
                                y: y as f32,
                                width: width as f32,
                                height: height as f32,
                            });
                            s.layer_rows.insert(id.0, row);
                        }
                        s.pending.insert(id.0, Frame { width, height, pixels });
                    }
                    Event::LayerRemoved(id) => {
                        let mut s = shared.borrow_mut();
                        s.pending.remove(&id.0);
                        if let Some(removed) = s.layer_rows.remove(&id.0) {
                            layers_model.remove(removed);
                            for row in s.layer_rows.values_mut() {
                                if *row > removed {
                                    *row -= 1;
                                }
                            }
                            s.closed.push(id.0);
                        }
                    }
                    Event::ActivationRequested(id) => {
                        // An app wants attention: flag its task with a notification.
                        let task = tasks.borrow().task_of_window(id);
                        if let Some(task) = task {
                            tasks.borrow_mut().notify(task);
                            refresh_tasks();
                        }
                    }
                    Event::IdleInhibited(on) => {
                        shared.borrow_mut().idle_inhibited = on;
                    }
                    Event::XwaylandReady { display } => {
                        spawn_env.borrow_mut().x_display = Some(display);
                        log::info!("xwayland ready: DISPLAY=:{display}");
                    }
                    Event::PopupBuffer {
                        id,
                        parent,
                        ox,
                        oy,
                        width,
                        height,
                        pixels,
                    } => {
                        let mut s = shared.borrow_mut();
                        let (px, py) = popup_origin(&s, &popups_model, parent);
                        let (x, y) = (px + ox as f32, py + oy as f32);
                        if let Some(&row) = s.popup_rows.get(&id.0) {
                            if let Some(mut t) = popups_model.row_data(row) {
                                t.x = x;
                                t.y = y;
                                t.width = width as f32;
                                t.height = height as f32;
                                popups_model.set_row_data(row, t);
                            }
                        } else {
                            let row = popups_model.row_count();
                            popups_model.push(PopupTile {
                                id: id.0 as i32,
                                texture: slint::Image::default(),
                                x,
                                y,
                                width: width as f32,
                                height: height as f32,
                            });
                            s.popup_rows.insert(id.0, row);
                        }
                        s.pending.insert(id.0, Frame { width, height, pixels });
                    }
                    Event::PopupRemoved(id) => {
                        let mut s = shared.borrow_mut();
                        s.pending.remove(&id.0);
                        if let Some(removed) = s.popup_rows.remove(&id.0) {
                            popups_model.remove(removed);
                            for row in s.popup_rows.values_mut() {
                                if *row > removed {
                                    *row -= 1;
                                }
                            }
                            s.closed.push(id.0);
                        }
                    }
                    Event::OutputResized { .. } => {}
                }
            }
            if dirty_windows {
                rebuild_windows();
            }

            // Notification toasts: ingest daemon/internal events, then expire.
            let mut toasts_dirty = false;
            while let Ok(ev) = toast_rx.try_recv() {
                toasts_dirty = true;
                let mut t = toasts.borrow_mut();
                match ev {
                    NotifyEvent::Add {
                        id,
                        app_name,
                        summary,
                        body,
                        timeout_ms,
                    } => {
                        let deadline = match timeout_ms {
                            0 => None, // sticky
                            ms if ms < 0 => Some(Instant::now() + Duration::from_secs(5)),
                            ms => Some(Instant::now() + Duration::from_millis(ms as u64)),
                        };
                        t.retain(|x| x.id != id);
                        t.push(ToastState { id, app: app_name, summary, body, deadline });
                        // Keep the on-screen stack bounded.
                        let overflow = t.len().saturating_sub(5);
                        if overflow > 0 {
                            t.drain(0..overflow);
                        }
                    }
                    NotifyEvent::Close { id } => t.retain(|x| x.id != id),
                }
            }
            {
                let now = Instant::now();
                let mut t = toasts.borrow_mut();
                let before = t.len();
                t.retain(|x| x.deadline.is_none_or(|d| d > now));
                if t.len() != before {
                    toasts_dirty = true;
                }
            }
            if toasts_dirty {
                refresh_toasts();
            }

            // System-tray updates: upsert/remove icons by id.
            if let Some(rx) = &tray_rx {
                let mut tray_dirty = false;
                while let Ok(update) = rx.try_recv() {
                    tray_dirty = true;
                    let mut items = tray_items.borrow_mut();
                    match update {
                        TrayUpdate::Add { id, title, pixmap } => {
                            let icon = pixmap
                                .map(|(w, h, rgba)| rgba_to_image(w, h, &rgba))
                                .unwrap_or_default();
                            let entry = TrayIcon {
                                id: id.clone().into(),
                                title: title.into(),
                                icon,
                            };
                            if let Some(e) = items.iter_mut().find(|e| e.id.as_str() == id.as_str())
                            {
                                *e = entry;
                            } else {
                                items.push(entry);
                            }
                        }
                        TrayUpdate::Remove { id } => {
                            items.retain(|e| e.id.as_str() != id.as_str())
                        }
                    }
                }
                if tray_dirty {
                    tray_model.set_vec(tray_items.borrow().clone());
                }
            }

            // Keep the compositor output sized to the host window's content area.
            if let Some(ui) = weak.upgrade() {
                sync_output_size(&ui, &cmd_tx, &tasks, &shared);
            }
        }
    });

    // --- Live clock ------------------------------------------------------------
    let update_clock: Rc<dyn Fn()> = {
        let weak = weak.clone();
        Rc::new(move || {
            if let Some(ui) = weak.upgrade() {
                let now = Local::now();
                ui.global::<AppData>()
                    .set_clock_time(now.format("%H:%M").to_string().into());
                ui.global::<AppData>()
                    .set_clock_date(now.format("%A, %b %d").to_string().into());
            }
        })
    };
    update_clock();
    let clock_timer = slint::Timer::default();
    clock_timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), {
        let update_clock = update_clock.clone();
        move || update_clock()
    });

    // --- Periodic time-tracking flush + idle detection -------------------------
    let flush_timer = slint::Timer::default();
    flush_timer.start(slint::TimerMode::Repeated, Duration::from_secs(10), {
        let tasks = tasks.clone();
        let shared = shared.clone();
        let refresh_tasks = refresh_tasks.clone();
        let now_secs = now_secs.clone();
        let last_activity = last_activity.clone();
        let weak = weak.clone();
        move || {
            let now = now_secs();
            let idle_secs = last_activity.borrow().elapsed().as_secs();
            let threshold = tasks.borrow().settings().idle_minutes.saturating_mul(60);
            // A client (e.g. a video player) may inhibit idle.
            let inhibited = shared.borrow().idle_inhibited;
            let became_idle = {
                let mut list = tasks.borrow_mut();
                list.set_date(&today());
                if threshold > 0 && idle_secs >= threshold && !inhibited {
                    // Count up to the moment activity stopped, then pause.
                    list.flush(now.saturating_sub(idle_secs));
                    let was_running = !list.is_paused();
                    list.pause();
                    was_running
                } else {
                    list.resume(now);
                    list.flush(now);
                    false
                }
            };
            refresh_tasks();
            persist::save(&tasks.borrow());
            // Raise the lock screen when we first go idle.
            if became_idle {
                if let Some(ui) = weak.upgrade() {
                    ui.set_locked(true);
                }
            }
        }
    });

    ui.run()?;

    // Persist a final time snapshot on exit.
    {
        let mut list = tasks.borrow_mut();
        list.set_date(&today());
        list.flush(now_secs());
    }
    persist::save(&tasks.borrow());
    Ok(())
}

/// Where a popup's top-left sits in the content area: its parent's origin plus
/// the popup's own offset. A window parent sits at the content origin (offset by
/// its title bar when decorated); a popup parent uses its own position.
fn popup_origin(
    shared: &Shared,
    popups_model: &VecModel<PopupTile>,
    parent: Option<WindowId>,
) -> (f32, f32) {
    let Some(parent) = parent else {
        return (0.0, 0.0);
    };
    if shared.rows.contains_key(&parent.0) {
        let decorated = shared.meta.get(&parent.0).map(|m| m.decorated).unwrap_or(false);
        return (0.0, if decorated { 30.0 } else { 0.0 });
    }
    if let Some(&row) = shared.popup_rows.get(&parent.0) {
        if let Some(t) = popups_model.row_data(row) {
            return (t.x, t.y);
        }
    }
    (0.0, 0.0)
}

/// Cache the imported texture and update whichever live model row (window,
/// popup or layer) the surface `id` belongs to.
fn place_texture(
    shared: &mut Shared,
    windows_model: &VecModel<WindowTile>,
    popups_model: &VecModel<PopupTile>,
    layers_model: &VecModel<LayerTile>,
    id: u64,
    image: slint::Image,
    w: f32,
    h: f32,
) {
    shared.tiles.insert(id, (w, h, image.clone()));
    if let Some(row) = shared.rows.get(&id).copied() {
        if let Some(mut t) = windows_model.row_data(row) {
            t.texture = image;
            t.width = w;
            t.height = h;
            windows_model.set_row_data(row, t);
        }
    } else if let Some(row) = shared.popup_rows.get(&id).copied() {
        if let Some(mut t) = popups_model.row_data(row) {
            t.texture = image;
            t.width = w;
            t.height = h;
            popups_model.set_row_data(row, t);
        }
    } else if let Some(row) = shared.layer_rows.get(&id).copied() {
        if let Some(mut t) = layers_model.row_data(row) {
            t.texture = image;
            t.width = w;
            t.height = h;
            layers_model.set_row_data(row, t);
        }
    }
}

/// Resolve a task's accent colour for the UI: its stored "#rrggbb" hex, or — for
/// tasks persisted before colours existed — a palette colour chosen by position.
fn task_tint(color: &str, index: usize) -> slint::Color {
    if let Some(c) = parse_hex_color(color) {
        return c;
    }
    let palette = focuswm_shell::task_palette();
    parse_hex_color(&palette[index % palette.len()])
        .unwrap_or_else(|| slint::Color::from_rgb_u8(0x89, 0xb4, 0xfa))
}

/// Parse a "#rrggbb" hex string into a Slint colour, or `None` if malformed.
fn parse_hex_color(hex: &str) -> Option<slint::Color> {
    let h = hex.strip_prefix('#').unwrap_or(hex);
    if h.len() != 6 {
        return None;
    }
    let rgb = u32::from_str_radix(h, 16).ok()?;
    Some(slint::Color::from_rgb_u8(
        (rgb >> 16) as u8,
        (rgb >> 8) as u8,
        rgb as u8,
    ))
}

/// The current local calendar day, "YYYY-MM-DD".
fn today() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// Format a duration in seconds as "Xh Ym" or "Ym".
fn fmt_dur(secs: u64) -> String {
    let m = secs / 60;
    if m >= 60 {
        format!("{}h {}m", m / 60, m % 60)
    } else {
        format!("{m}m")
    }
}

/// Short duration for the bar-graph labels: "2h", "30m", or "" for zero.
fn fmt_short(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 0 {
        format!("{h}h")
    } else if m > 0 {
        format!("{m}m")
    } else {
        String::new()
    }
}

/// The program name of the effective browser command, for the UI label.
fn browser_label(list: &TaskList) -> String {
    let b = list.settings().browser.trim().to_string();
    if b.is_empty() {
        config::browser_name()
    } else {
        config::split_command(&b).first().cloned().unwrap_or_default()
    }
}

/// Compute and publish the time-report figures for the selected `anchor` day:
/// the day total, the containing week's total + per-day bar graph, and the
/// by-category / by-project breakdowns (day + week columns).
fn build_report(list: &TaskList, ui: &Desktop, anchor: chrono::NaiveDate) {
    let fmt = |d: chrono::NaiveDate| d.format("%Y-%m-%d").to_string();
    let day = fmt(anchor);
    let monday = anchor - ChronoDuration::days(anchor.weekday().num_days_from_monday() as i64);
    let sunday = monday + ChronoDuration::days(6);
    let log = list.time_log();
    let day_agg = log.aggregate(&day, &day);
    let week_agg = log.aggregate(&fmt(monday), &fmt(sunday));

    // Rows ordered by the week totals, with the matching day value alongside.
    let rows_of = |week_rows: &[(String, u64)], day_rows: &[(String, u64)]| -> Vec<ReportRow> {
        week_rows
            .iter()
            .map(|(label, wsecs)| {
                let dsecs = day_rows
                    .iter()
                    .find(|(l, _)| l == label)
                    .map(|(_, s)| *s)
                    .unwrap_or(0);
                ReportRow {
                    label: label.clone().into(),
                    today: fmt_dur(dsecs).into(),
                    week: fmt_dur(*wsecs).into(),
                }
            })
            .collect()
    };
    let cats = rows_of(&week_agg.by_category, &day_agg.by_category);
    let projects = rows_of(&week_agg.by_project, &day_agg.by_project);

    // Weekly bar graph (Mon..Sun), scaled to the busiest day.
    let day_secs: Vec<u64> = (0..7)
        .map(|i| {
            let d = fmt(monday + ChronoDuration::days(i));
            log.aggregate(&d, &d).total
        })
        .collect();
    let max = day_secs.iter().copied().max().unwrap_or(0).max(1);
    let bars: Vec<DayBar> = (0..7)
        .map(|i| {
            let d = monday + ChronoDuration::days(i);
            let secs = day_secs[i as usize];
            DayBar {
                label: d.format("%a").to_string().into(),
                value: fmt_short(secs).into(),
                fraction: secs as f32 / max as f32,
                selected: d == anchor,
            }
        })
        .collect();

    let rd = ui.global::<ReportData>();
    rd.set_day_label(anchor.format("%a %b %d").to_string().into());
    rd.set_week_label(
        format!("{} – {}", monday.format("%b %d"), sunday.format("%b %d")).into(),
    );
    rd.set_day_total(fmt_dur(day_agg.total).into());
    rd.set_week_total(fmt_dur(week_agg.total).into());
    rd.set_week_bars(ModelRc::from(Rc::new(VecModel::from(bars))));
    rd.set_by_category(ModelRc::from(Rc::new(VecModel::from(cats))));
    rd.set_by_project(ModelRc::from(Rc::new(VecModel::from(projects))));
    rd.set_can_forward(anchor < Local::now().date_naive());
}

/// Track the host window size and resize the compositor output + active windows
/// to fill the content area (window region right of the sidebar, below header).
fn sync_output_size(
    ui: &Desktop,
    cmd_tx: &focuswm_wayland::CommandSender<Command>,
    tasks: &Rc<RefCell<TaskList>>,
    shared: &Rc<RefCell<Shared>>,
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
        let shared = shared.borrow();
        for w in tasks.borrow().active_windows() {
            // Decorated windows leave room for the 30px server-side title bar.
            let decorated = shared.meta.get(&w.0).map(|m| m.decorated).unwrap_or(false);
            let h = if decorated { (content_h - 30).max(1) } else { content_h };
            let _ = cmd_tx.send(Command::ResizeWindow {
                id: w,
                width: content_w,
                height: h,
            });
        }
    }
}

/// Spawn a client program into the compositor, surfacing failures as a toast.
fn spawn_client(
    cmd: &[String],
    env: &SpawnEnv,
    cwd: Option<&str>,
    toast_tx: &async_channel::Sender<NotifyEvent>,
) {
    if env.wayland_display.is_empty() {
        log::warn!("client requested before the compositor was ready");
        return;
    }
    if let Err(err) = config::spawn(cmd, env, cwd) {
        log::warn!("{err}");
        internal_toast(toast_tx, "focuswm", "Couldn't open application", &err);
    }
}

/// Build a Slint CPU image from tightly-packed RGBA8 pixels.
fn rgba_to_image(w: u32, h: u32, rgba: &[u8]) -> slint::Image {
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
    let bytes = buf.make_mut_bytes();
    let n = bytes.len().min(rgba.len());
    bytes[..n].copy_from_slice(&rgba[..n]);
    slint::Image::from_rgba8(buf)
}

/// Show an internally generated toast (id space well above the daemon's).
fn internal_toast(tx: &async_channel::Sender<NotifyEvent>, app: &str, summary: &str, body: &str) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1_000_000);
    let _ = tx.try_send(NotifyEvent::Add {
        id: NEXT.fetch_add(1, Ordering::Relaxed),
        app_name: app.to_string(),
        summary: summary.to_string(),
        body: body.to_string(),
        timeout_ms: 5000,
    });
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
    // Control + special keys (Slint encodes these as specific code points).
    match ch {
        '\u{0008}' => return Some((14, false)),             // Backspace
        '\u{0009}' => return Some((15, false)),             // Tab
        '\u{000a}' | '\u{000d}' => return Some((28, false)), // Return
        '\u{001b}' => return Some((1, false)),              // Escape
        '\u{007f}' => return Some((111, false)),            // Delete
        ' ' => return Some((57, false)),                    // Space
        '\u{F700}' => return Some((103, false)),            // UpArrow
        '\u{F701}' => return Some((108, false)),            // DownArrow
        '\u{F702}' => return Some((105, false)),            // LeftArrow
        '\u{F703}' => return Some((106, false)),            // RightArrow
        '\u{F727}' => return Some((110, false)),            // Insert
        '\u{F729}' => return Some((102, false)),            // Home
        '\u{F72B}' => return Some((107, false)),            // End
        '\u{F72C}' => return Some((104, false)),            // PageUp
        '\u{F72D}' => return Some((109, false)),            // PageDown
        _ => {}
    }
    // Function keys F1..F12 (Slint: F1 = U+F704).
    if ('\u{F704}'..='\u{F70F}').contains(&ch) {
        // evdev: F1=59..F10=68, F11=87, F12=88.
        let n = ch as u32 - 0xF704; // 0-based
        let kc = match n {
            0..=9 => 59 + n,   // F1..F10
            10 => 87,          // F11
            11 => 88,          // F12
            _ => return None,
        };
        return Some((kc, false));
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
    fn evdev_arrows_and_nav() {
        assert_eq!(evdev_keycode("\u{F700}"), Some((103, false))); // Up
        assert_eq!(evdev_keycode("\u{F703}"), Some((106, false))); // Right
        assert_eq!(evdev_keycode("\u{F729}"), Some((102, false))); // Home
        assert_eq!(evdev_keycode("\u{007f}"), Some((111, false))); // Delete
    }

    #[test]
    fn evdev_function_keys() {
        assert_eq!(evdev_keycode("\u{F704}"), Some((59, false))); // F1
        assert_eq!(evdev_keycode("\u{F70D}"), Some((68, false))); // F10
        assert_eq!(evdev_keycode("\u{F70E}"), Some((87, false))); // F11
        assert_eq!(evdev_keycode("\u{F70F}"), Some((88, false))); // F12
    }

    #[test]
    fn fmt_dur_formats_hours_and_minutes() {
        assert_eq!(fmt_dur(0), "0m");
        assert_eq!(fmt_dur(90), "1m");
        assert_eq!(fmt_dur(3600), "1h 0m");
        assert_eq!(fmt_dur(3660), "1h 1m");
        assert_eq!(fmt_short(0), "");
        assert_eq!(fmt_short(120), "2m");
        assert_eq!(fmt_short(7200), "2h");
    }

    #[test]
    fn buttons_map_to_evdev() {
        assert_eq!(evdev_button(1), 0x110);
        assert_eq!(evdev_button(2), 0x111);
        assert_eq!(evdev_button(3), 0x112);
    }
}
