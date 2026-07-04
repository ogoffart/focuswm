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
mod github;
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

/// Height of the server-side decoration title bar, in logical px. Must match the
/// title-bar height in `main.slint`'s `WindowView`.
const TITLE_BAR_H: f32 = 30.0;

/// Width of the sidebar, in logical px. The client content area is everything to
/// its right, so the compositor output is sized to `window_width - SIDEBAR_W`.
/// Must match `sidebar-width` in `theme.slint`.
const SIDEBAR_W: i32 = 252;

/// Smallest a floating window frame may be shrunk to, in logical px.
const MIN_WIN_W: f32 = 200.0;
const MIN_WIN_H: f32 = 120.0;

/// Last-known metadata for a client window.
#[derive(Default, Clone)]
struct WinMeta {
    title: String,
    #[allow(dead_code)]
    app_id: String,
    /// Whether the compositor should draw a server-side decoration title bar.
    decorated: bool,
    /// Floating frame geometry in content-area logical px: top-left `(x, y)` and
    /// whole-frame size `(w, h)` (the title bar is the top `TITLE_BAR_H` of it).
    geom: WinGeom,
    /// Geometry to restore to when unmaximizing/unsnapping (the frame from just
    /// before the last maximize/snap), if any.
    restore: Option<WinGeom>,
    /// Client size hints for the content surface, in logical px (0 = unset).
    /// Resizing clamps the frame so the content stays within these.
    min_w: i32,
    min_h: i32,
    max_w: i32,
    max_h: i32,
}

impl WinMeta {
    /// Frame-size bounds `(min_w, min_h, max_w, max_h)` in logical px from the
    /// client's content hints plus the title bar; `max` is `f32::INFINITY` when
    /// the client set no maximum. Never below the global minimum, and `max` is
    /// kept ≥ `min` so callers can `clamp` safely.
    fn frame_bounds(&self) -> (f32, f32, f32, f32) {
        let bar = if self.decorated { TITLE_BAR_H } else { 0.0 };
        let min_w = (self.min_w as f32).max(MIN_WIN_W);
        let min_h = (if self.min_h > 0 { self.min_h as f32 + bar } else { 0.0 }).max(MIN_WIN_H);
        let max_w = if self.max_w > 0 { (self.max_w as f32).max(min_w) } else { f32::INFINITY };
        let max_h = if self.max_h > 0 { (self.max_h as f32 + bar).max(min_h) } else { f32::INFINITY };
        (min_w, min_h, max_w, max_h)
    }
}

/// A floating window's frame rectangle, in content-area logical px.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
struct WinGeom {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl WinGeom {
    /// The client surface size below the title bar: the content the Wayland
    /// client should render into, in logical px.
    fn content_size(&self, decorated: bool) -> (i32, i32) {
        let bar = if decorated { TITLE_BAR_H } else { 0.0 };
        (self.w.round() as i32, (self.h - bar).round() as i32)
    }

    /// Clamp the top-left so the title bar stays grab-able within a `cw`×`ch`
    /// content area (keep at least `KEEP` px of the frame on each axis on-screen).
    fn clamp_pos(&mut self, cw: f32, ch: f32) {
        const KEEP: f32 = 80.0;
        self.x = self.x.clamp(KEEP - self.w, (cw - KEEP).max(0.0));
        self.y = self.y.clamp(0.0, (ch - TITLE_BAR_H).max(0.0));
    }

    /// Resize the frame by dragging `edges` (bitmask 1=left 2=right 4=top
    /// 8=bottom) by `(dx, dy)`, clamped to `(min_w, min_h, max_w, max_h)`.
    /// Dragging the left/top edge keeps the opposite edge anchored by moving the
    /// origin to match the clamped size.
    fn resize_by(&mut self, edges: i32, dx: f32, dy: f32, bounds: (f32, f32, f32, f32)) {
        let (min_w, min_h, max_w, max_h) = bounds;
        if edges & 2 != 0 {
            self.w = (self.w + dx).clamp(min_w, max_w);
        }
        if edges & 1 != 0 {
            let new_w = (self.w - dx).clamp(min_w, max_w);
            self.x += self.w - new_w;
            self.w = new_w;
        }
        if edges & 8 != 0 {
            self.h = (self.h + dy).clamp(min_h, max_h);
        }
        if edges & 4 != 0 {
            let new_h = (self.h - dy).clamp(min_h, max_h);
            self.y += self.h - new_h;
            self.h = new_h;
        }
    }
}

/// The frame for snapping a window into `zone` of a `cw`×`ch` content area:
/// 1=left half, 2=right half, 4..7=corner quarters, anything else=maximize
/// (the whole area). The result is not yet clamped to the client's size hints.
fn snap_geom(zone: i32, cw: f32, ch: f32) -> WinGeom {
    let (hw, hh) = (cw / 2.0, ch / 2.0);
    match zone {
        1 => WinGeom { x: 0.0, y: 0.0, w: hw, h: ch },  // left half
        2 => WinGeom { x: hw, y: 0.0, w: hw, h: ch },   // right half
        4 => WinGeom { x: 0.0, y: 0.0, w: hw, h: hh },  // top-left
        5 => WinGeom { x: hw, y: 0.0, w: hw, h: hh },   // top-right
        6 => WinGeom { x: 0.0, y: hh, w: hw, h: hh },   // bottom-left
        7 => WinGeom { x: hw, y: hh, w: hw, h: hh },    // bottom-right
        _ => WinGeom { x: 0.0, y: 0.0, w: cw, h: ch },  // maximize
    }
}

/// Clamp a frame's size to the client's own min/max *content*-size hints (0 =
/// unset on that axis), adding the title bar to the height bounds when
/// decorated; the position is left unchanged. Unlike [`WinMeta::frame_bounds`]
/// this applies no global floor, so a client that declares a small fixed size is
/// honored (its frame shrinks to match what it draws rather than being forced to
/// the minimum floating size).
fn clamp_to_client_hints(
    mut geom: WinGeom,
    decorated: bool,
    min_w: i32,
    min_h: i32,
    max_w: i32,
    max_h: i32,
) -> WinGeom {
    let bar = if decorated { TITLE_BAR_H } else { 0.0 };
    let cmin_w = if min_w > 0 { min_w as f32 } else { 0.0 };
    let cmin_h = if min_h > 0 { min_h as f32 + bar } else { 0.0 };
    let cmax_w = if max_w > 0 { (max_w as f32).max(cmin_w) } else { f32::INFINITY };
    let cmax_h = if max_h > 0 { (max_h as f32 + bar).max(cmin_h) } else { f32::INFINITY };
    geom.w = geom.w.clamp(cmin_w, cmax_w);
    geom.h = geom.h.clamp(cmin_h, cmax_h);
    geom
}

/// If `(gx, gy)` (content-area coords) falls within a window's *content* region
/// — the frame minus the title bar when decorated — the surface-local
/// coordinates within it, else `None`. Used to hit-test drag-and-drop targets.
fn content_hit(geom: WinGeom, decorated: bool, gx: f32, gy: f32) -> Option<(f32, f32)> {
    let bar = if decorated { TITLE_BAR_H } else { 0.0 };
    let (cx, cy, cw, ch) = (geom.x, geom.y + bar, geom.w, geom.h - bar);
    if gx >= cx && gx < cx + cw && gy >= cy && gy < cy + ch {
        Some((gx - cx, gy - cy))
    } else {
        None
    }
}

/// The frame for a maximized window in a `cw`×`ch` content area, capped at the
/// client's `max_w`/`max_h` (so a non-resizable client doesn't get a frame
/// bigger than it renders into) and centred within the area.
fn maximized_geom(cw: f32, ch: f32, max_w: f32, max_h: f32) -> WinGeom {
    let w = cw.min(max_w);
    let h = ch.min(max_h);
    WinGeom { x: (cw - w) / 2.0, y: (ch - h) / 2.0, w, h }
}

/// A default frame for a freshly mapped window: ~72% of the content area, nudged
/// by a small per-window cascade so successive windows don't perfectly overlap.
fn default_geom(id: u64, cw: f32, ch: f32) -> WinGeom {
    let w = (cw * 0.72).clamp(MIN_WIN_W, (cw - 20.0).max(MIN_WIN_W));
    let h = (ch * 0.72).clamp(MIN_WIN_H, (ch - 20.0).max(MIN_WIN_H));
    let off = (id % 6) as f32 * 28.0;
    let mut geom = WinGeom { x: 20.0 + off, y: 16.0 + off, w, h };
    geom.clamp_pos(cw, ch);
    geom
}

/// Number of slots to move a dragged sidebar task, from its vertical drag
/// distance `dy` (logical px). Must use the same pitch as `row-pitch` in
/// sidebar.slint (row height + gap).
fn reorder_delta(dy: f32) -> i32 {
    const ROW_PITCH: f32 = 64.0;
    (dy / ROW_PITCH).round() as i32
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
    /// Current content-area size (logical px), tracked from the host window so
    /// new floating windows can be placed and positions clamped against it.
    content: (f32, f32),
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // A compositor juggles a lot of file descriptors (a socket per client, every
    // dmabuf plane, XWayland, epoll/eventfds, …), so the default soft limit
    // (often 1024) is easily exhausted — especially with fd-hungry clients like
    // Firefox — which then crashes the event loop with "Too many open files".
    // Raise the soft limit to the hard limit, as other compositors do.
    raise_open_file_limit();

    // A panic on any thread brings the whole process down rather than leaving a
    // half-dead shell (e.g. a dead Wayland thread → "no windows show").
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        log::error!("fatal: a thread panicked — aborting focuswm");
        std::process::abort();
    }));

    // `zbus` is compiled with its `tokio` feature: the `system-tray` crate pulls
    // it in, and Cargo unifies features across the build, so *every* zbus user is
    // now tokio-flavoured — including Slint's winit backend, which reaches the
    // desktop portal over zbus to detect the colour scheme when it opens an X11
    // window. That call runs on this (main) thread, where zbus expects an ambient
    // Tokio runtime; without one it panics with "there is no reactor running".
    // Enter a multi-threaded runtime for the whole UI lifetime so those zbus
    // calls find a live reactor (its I/O driver runs on the runtime's own
    // threads, so it keeps working while Slint owns the main thread).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("focuswm-zbus")
        .enable_all()
        .build()?;
    let _runtime_guard = runtime.enter();

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
    // Seed the content size (output minus the sidebar) so the first window
    // placed before the host reports its size still lands somewhere sensible.
    shared.borrow_mut().content =
        ((focuswm_wayland::OUTPUT_W - SIDEBAR_W) as f32, focuswm_wayland::OUTPUT_H as f32);
    let bridge = Rc::new(RefCell::new(GlBridge::default()));
    let spawn_env = Rc::new(RefCell::new(SpawnEnv::default()));
    // Holds focuswm's private D-Bus daemon (started once the compositor is
    // ready) alive for the lifetime of the session.
    let dbus_daemon: Rc<RefCell<Option<std::process::Child>>> = Rc::new(RefCell::new(None));
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

    // GitHub integration (only when GITHUB_TOKEN is set): wizard issue search and
    // background polling of linked issues/PRs for new activity.
    let github = github::spawn();
    ui.global::<AppData>().set_github_enabled(github.is_some());
    let issue_results_model = Rc::new(VecModel::<IssueResult>::default());
    ui.global::<AppData>()
        .set_issue_results(ModelRc::from(issue_results_model.clone()));
    // The issue the user picked in the wizard to link to the next created task.
    let pending_link: Rc<RefCell<Option<focuswm_shell::GithubLink>>> = Rc::new(RefCell::new(None));

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
            let focused_win = *focused.borrow();
            let mut active_windows = list.active_windows();
            // Render the focused window last so it stacks on top.
            if let Some(f) = focused_win {
                if let Some(pos) = active_windows.iter().position(|w| *w == f) {
                    let w = active_windows.remove(pos);
                    active_windows.push(w);
                }
            }
            let (cw, ch) = shared.content;
            let mut tiles = Vec::new();
            let mut rows = HashMap::new();
            for (row, wid) in active_windows.iter().enumerate() {
                let id = wid.0;
                let (title, decorated, mut geom) = shared
                    .meta
                    .get(&id)
                    .map(|m| (m.title.clone(), m.decorated, m.geom))
                    .unwrap_or_default();
                // Keep the title bar reachable if the content area shrank.
                geom.clamp_pos(cw, ch);
                if let Some(m) = shared.meta.get_mut(&id) {
                    m.geom = geom;
                }
                let texture = shared
                    .tiles
                    .get(&id)
                    .map(|(_, _, img)| img.clone())
                    .unwrap_or_default();
                tiles.push(WindowTile {
                    id: id as i32,
                    title: title.into(),
                    texture,
                    x: geom.x,
                    y: geom.y,
                    width: geom.w,
                    height: geom.h,
                    decorated,
                    minimized: list.is_minimized(*wid),
                    maximized: list.is_maximized(*wid),
                    focused: focused_win == Some(*wid),
                });
                rows.insert(id, row);
            }
            shared.rows = rows;
            windows_model.set_vec(tiles);
        })
    };

    // Update just the `focused` highlight on each window row, without rebuilding
    // or restacking. Used when focus-follows-mouse moves focus on hover (where a
    // full rebuild would churn the z-order as the pointer travels).
    let refresh_focus_highlight: Rc<dyn Fn()> = {
        let windows_model = windows_model.clone();
        let focused = focused.clone();
        Rc::new(move || {
            let f = focused.borrow().map(|w| w.0 as i32);
            for row in 0..windows_model.row_count() {
                if let Some(mut t) = windows_model.row_data(row) {
                    let want = f == Some(t.id);
                    if t.focused != want {
                        t.focused = want;
                        windows_model.set_row_data(row, t);
                    }
                }
            }
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
        let pending_link = pending_link.clone();
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
                    // Attach the issue/PR picked in the wizard, if any.
                    task.github = pending_link.borrow_mut().take();
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

    // Wizard: run a GitHub issue/PR search (results arrive async via GhEvent).
    ui.global::<Logic>().on_search_issues({
        let requests = github.as_ref().map(|g| g.requests.clone());
        let weak = weak.clone();
        let toast_tx = toast_tx.clone();
        move |query| {
            let query = query.trim().to_string();
            if query.is_empty() {
                return;
            }
            let Some(requests) = &requests else {
                internal_toast(&toast_tx, "focuswm", "GitHub disabled", "Set GITHUB_TOKEN to search issues.");
                return;
            };
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppData>().set_issue_searching(true);
            }
            let _ = requests.send(github::Request::Search { query });
        }
    });

    // Wizard: select (or clear, with an empty slug) the issue to link to the
    // task being created, and reflect the selection in the results list.
    ui.global::<Logic>().on_link_issue({
        let pending_link = pending_link.clone();
        let issue_results_model = issue_results_model.clone();
        let weak = weak.clone();
        move |slug, number, title, url| {
            if slug.is_empty() {
                *pending_link.borrow_mut() = None;
                issue_results_model.set_vec(Vec::new());
                // Reset the search state too, so a search left in flight when the
                // wizard was closed doesn't leave the button stuck on "…".
                if let Some(ui) = weak.upgrade() {
                    ui.global::<AppData>().set_issue_searching(false);
                }
                return;
            }
            *pending_link.borrow_mut() = Some(focuswm_shell::GithubLink {
                slug: slug.to_string(),
                number: number as u64,
                title: title.to_string(),
                url: url.to_string(),
                last_seen: None,
            });
            // Mark the chosen row selected, the rest not.
            for i in 0..issue_results_model.row_count() {
                if let Some(mut r) = issue_results_model.row_data(i) {
                    r.selected = r.number == number && r.slug == slug;
                    issue_results_model.set_row_data(i, r);
                }
            }
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

    // Drag-and-drop reorder: convert the dragged vertical distance into a number
    // of slots (the row pitch matches `row-pitch` in sidebar.slint) and move the
    // task by that many positions.
    ui.global::<Logic>().on_reorder_task({
        let tasks = tasks.clone();
        let refresh_tasks = refresh_tasks.clone();
        let mark_active = mark_active.clone();
        move |id, dy| {
            mark_active();
            let delta = reorder_delta(dy);
            if delta != 0 {
                let mut list = tasks.borrow_mut();
                if let Some(from) = list.tasks().iter().position(|t| t.id.0 == id as u64) {
                    let to = (from as i32 + delta).max(0) as usize;
                    list.reorder(from, to);
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
            ui.global::<SettingsData>()
                .set_focus_follows_mouse(s.focus_follows_mouse);
            ui.set_settings_open(true);
        }
    });

    ui.global::<Logic>().on_save_settings({
        let tasks = tasks.clone();
        let apply_categories = apply_categories.clone();
        let mark_active = mark_active.clone();
        let weak = weak.clone();
        move |terminal, browser, categories_csv, idle_minutes, focus_follows_mouse| {
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
                focus_follows_mouse,
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
        let tasks = tasks.clone();
        let rebuild_windows = rebuild_windows.clone();
        let mark_active = mark_active.clone();
        move |id| {
            mark_active();
            let wid = WindowId(id as u64);
            // Focusing a minimized window restores it.
            if tasks.borrow().is_minimized(wid) {
                tasks.borrow_mut().set_minimized(wid, false);
            }
            *focused.borrow_mut() = Some(wid);
            let _ = cmd_tx.send(Command::FocusWindow(wid));
            rebuild_windows();
        }
    });

    // Drag a floating window's title bar: add the delta to its frame position.
    // Updating just this row (not a full rebuild) keeps the TouchArea's pointer
    // grab — and so the live drag — intact.
    ui.global::<Logic>().on_move_window({
        let shared = shared.clone();
        let windows_model = windows_model.clone();
        let mark_active = mark_active.clone();
        move |id, dx, dy| {
            mark_active();
            let id = id as u64;
            let mut s = shared.borrow_mut();
            let (cw, ch) = s.content;
            let geom = {
                let Some(meta) = s.meta.get_mut(&id) else { return };
                meta.geom.x += dx;
                meta.geom.y += dy;
                meta.geom.clamp_pos(cw, ch);
                meta.geom
            };
            if let Some(row) = s.rows.get(&id).copied() {
                if let Some(mut t) = windows_model.row_data(row) {
                    t.x = geom.x;
                    t.y = geom.y;
                    windows_model.set_row_data(row, t);
                }
            }
        }
    });

    // Drag a resize grip: apply the delta to the dragged edges (bitmask
    // 1=left 2=right 4=top 8=bottom), clamp to a minimum, and re-size the client.
    ui.global::<Logic>().on_resize_window({
        let shared = shared.clone();
        let windows_model = windows_model.clone();
        let cmd_tx = cmd_tx.clone();
        let tasks = tasks.clone();
        let mark_active = mark_active.clone();
        move |id, edges, dx, dy| {
            mark_active();
            let id = id as u64;
            let wid = WindowId(id);
            // A manual resize leaves the maximized state: clear it so the client
            // drops its maximized look and the ▣ button maximizes again instead
            // of "restoring" a stale pre-maximize frame. Patch the row in place —
            // a rebuild here would recreate the view and break the drag grab.
            if tasks.borrow().is_maximized(wid) {
                tasks.borrow_mut().set_maximized(wid, false);
                let _ = cmd_tx.send(Command::SetMaximized { id: wid, maximized: false });
                let s = shared.borrow();
                if let Some(row) = s.rows.get(&id).copied() {
                    if let Some(mut t) = windows_model.row_data(row) {
                        t.maximized = false;
                        windows_model.set_row_data(row, t);
                    }
                }
            }
            let mut s = shared.borrow_mut();
            let (g, decorated) = {
                let Some(meta) = s.meta.get_mut(&id) else { return };
                // The frame is no longer "maximized"; forget the stale restore
                // target so the next maximize saves this user-chosen frame.
                meta.restore = None;
                // Clamp to the client's min/max size hints (plus the global floor).
                let bounds = meta.frame_bounds();
                let mut g = meta.geom;
                g.resize_by(edges, dx, dy, bounds);
                meta.geom = g;
                (g, meta.decorated)
            };
            let (cwid, chei) = g.content_size(decorated);
            let _ = cmd_tx.send(Command::ResizeWindow {
                id: wid,
                width: cwid,
                height: chei,
            });
            if let Some(row) = s.rows.get(&id).copied() {
                if let Some(mut t) = windows_model.row_data(row) {
                    t.x = g.x;
                    t.y = g.y;
                    t.width = g.w;
                    t.height = g.h;
                    windows_model.set_row_data(row, t);
                }
            }
        }
    });

    // Toggle a window's minimized state (hidden from the content area, still
    // listed in the sidebar). Restoring also raises/focuses it.
    ui.global::<Logic>().on_minimize_window({
        let cmd_tx = cmd_tx.clone();
        let tasks = tasks.clone();
        let focused = focused.clone();
        let rebuild_windows = rebuild_windows.clone();
        let mark_active = mark_active.clone();
        move |id| {
            mark_active();
            let wid = WindowId(id as u64);
            let now_minimized = !tasks.borrow().is_minimized(wid);
            tasks.borrow_mut().set_minimized(wid, now_minimized);
            if now_minimized {
                // If we just minimized the focused window, move focus to another
                // visible window — otherwise the "minimize" shortcut would simply
                // toggle this same window back on the next press.
                if *focused.borrow() == Some(wid) {
                    let next = {
                        let list = tasks.borrow();
                        list.active_windows()
                            .into_iter()
                            .find(|w| *w != wid && !list.is_minimized(*w))
                    };
                    *focused.borrow_mut() = next;
                    if let Some(w) = next {
                        let _ = cmd_tx.send(Command::FocusWindow(w));
                    }
                }
            } else {
                *focused.borrow_mut() = Some(wid);
                let _ = cmd_tx.send(Command::FocusWindow(wid));
            }
            rebuild_windows();
        }
    });

    // Move a window to another desktop (task id, or -1 for desktop 0). The
    // window leaves the current view; keyboard focus moves to whatever remains.
    ui.global::<Logic>().on_move_window_to_desktop({
        let tasks = tasks.clone();
        let rebuild_windows = rebuild_windows.clone();
        let refresh_tasks = refresh_tasks.clone();
        let cmd_tx = cmd_tx.clone();
        let focused = focused.clone();
        let mark_active = mark_active.clone();
        move |id, task_id| {
            mark_active();
            let wid = WindowId(id as u64);
            let target = (task_id >= 0).then(|| focuswm_shell::TaskId(task_id as u64));
            {
                let mut list = tasks.borrow_mut();
                list.move_window_to(wid, target);
                // If the moved window was focused, focus another one still here.
                if *focused.borrow() == Some(wid) {
                    let next = list
                        .active_windows()
                        .into_iter()
                        .find(|w| !list.is_minimized(*w));
                    *focused.borrow_mut() = next;
                    if let Some(w) = next {
                        let _ = cmd_tx.send(Command::FocusWindow(w));
                    }
                }
            }
            rebuild_windows();
            refresh_tasks();
        }
    });

    // Toggle a window's maximized state: fill the content area (saving the
    // current frame to restore later), or restore the saved frame.
    ui.global::<Logic>().on_maximize_window({
        let cmd_tx = cmd_tx.clone();
        let tasks = tasks.clone();
        let shared = shared.clone();
        let rebuild_windows = rebuild_windows.clone();
        let mark_active = mark_active.clone();
        move |id| {
            mark_active();
            let wid = WindowId(id as u64);
            let maximized = !tasks.borrow().is_maximized(wid);
            tasks.borrow_mut().set_maximized(wid, maximized);
            {
                let mut s = shared.borrow_mut();
                let (cw, ch) = s.content;
                if let Some(meta) = s.meta.get_mut(&wid.0) {
                    if maximized {
                        meta.restore = Some(meta.geom);
                        // Honour the client's max size: a non-resizable window
                        // (e.g. a fixed X11 dialog) can't fill the area, so size
                        // it to its cap and centre it instead of leaving the
                        // frame larger than what the client renders.
                        let (_, _, max_w, max_h) = meta.frame_bounds();
                        meta.geom = maximized_geom(cw, ch, max_w, max_h);
                    } else {
                        meta.geom = meta.restore.take().unwrap_or(meta.geom);
                    }
                    let (w, h) = meta.geom.content_size(meta.decorated);
                    let _ = cmd_tx.send(Command::ResizeWindow { id: wid, width: w, height: h });
                }
            }
            let _ = cmd_tx.send(Command::SetMaximized { id: wid, maximized });
            rebuild_windows();
        }
    });

    // Snap a window to a screen region (1=left half, 2=right half, 3=maximize).
    ui.global::<Logic>().on_snap_window({
        let cmd_tx = cmd_tx.clone();
        let tasks = tasks.clone();
        let shared = shared.clone();
        let rebuild_windows = rebuild_windows.clone();
        let mark_active = mark_active.clone();
        move |id, zone| {
            mark_active();
            let wid = WindowId(id as u64);
            {
                let mut s = shared.borrow_mut();
                let (cw, ch) = s.content;
                if let Some(meta) = s.meta.get_mut(&wid.0) {
                    // Remember the pre-snap frame so a later restore brings it back.
                    if meta.restore.is_none() {
                        meta.restore = Some(meta.geom);
                    }
                    meta.geom = snap_geom(zone, cw, ch);
                    // Don't ask a non-resizable client for more than its max size.
                    let (_, _, max_w, max_h) = meta.frame_bounds();
                    meta.geom.w = meta.geom.w.min(max_w);
                    meta.geom.h = meta.geom.h.min(max_h);
                    let (w, h) = meta.geom.content_size(meta.decorated);
                    let _ = cmd_tx.send(Command::ResizeWindow { id: wid, width: w, height: h });
                }
            }
            // Only the top-edge snap is a true maximize; halves/quarters aren't.
            let maximized = zone == 3;
            tasks.borrow_mut().set_maximized(wid, maximized);
            let _ = cmd_tx.send(Command::SetMaximized { id: wid, maximized });
            rebuild_windows();
        }
    });

    // Keyboard window management (Super+arrows): apply snap/maximize/minimize to
    // the currently focused window by delegating to the id-based callbacks above,
    // so the restore-geometry bookkeeping stays in one place.
    // NB: read `focused` into a local and drop the borrow *before* invoking the
    // id-based callback — that callback re-enters the same `focused` RefCell
    // (minimize moves focus), so holding a borrow across the invoke would panic.
    ui.global::<Logic>().on_snap_focused({
        let weak = weak.clone();
        let focused = focused.clone();
        move |zone| {
            let wid = *focused.borrow();
            if let (Some(ui), Some(wid)) = (weak.upgrade(), wid) {
                ui.global::<Logic>().invoke_snap_window(wid.0 as i32, zone);
            }
        }
    });
    ui.global::<Logic>().on_maximize_focused({
        let weak = weak.clone();
        let focused = focused.clone();
        move || {
            let wid = *focused.borrow();
            if let (Some(ui), Some(wid)) = (weak.upgrade(), wid) {
                ui.global::<Logic>().invoke_maximize_window(wid.0 as i32);
            }
        }
    });
    ui.global::<Logic>().on_minimize_focused({
        let weak = weak.clone();
        let focused = focused.clone();
        move || {
            let wid = *focused.borrow();
            if let (Some(ui), Some(wid)) = (weak.upgrade(), wid) {
                ui.global::<Logic>().invoke_minimize_window(wid.0 as i32);
            }
        }
    });

    // The topmost non-minimized window whose *content* area (below the title
    // bar) contains a point in content coordinates, with the surface-local
    // coordinates within it. Used to re-target drag-and-drop across windows.
    let window_under: Rc<dyn Fn(f32, f32) -> Option<(WindowId, f32, f32)>> = {
        let tasks = tasks.clone();
        let shared = shared.clone();
        let focused = focused.clone();
        Rc::new(move |gx, gy| {
            let list = tasks.borrow();
            // Same stacking as `rebuild_windows`: active order, focused on top.
            let mut order = list.active_windows();
            if let Some(f) = *focused.borrow() {
                if let Some(pos) = order.iter().position(|w| *w == f) {
                    let w = order.remove(pos);
                    order.push(w);
                }
            }
            let s = shared.borrow();
            for wid in order.iter().rev() {
                if list.is_minimized(*wid) {
                    continue;
                }
                let Some(m) = s.meta.get(&wid.0) else { continue };
                if let Some((lx, ly)) = content_hit(m.geom, m.decorated, gx, gy) {
                    return Some((*wid, lx, ly));
                }
            }
            None
        })
    };

    // Drag-and-drop motion: position the drag icon at the cursor and forward
    // surface-local motion to the window under it, so the Wayland DnD grab sends
    // its offer to that surface (this is what lets a drag cross applications).
    ui.global::<Logic>().on_dnd_motion({
        let cmd_tx = cmd_tx.clone();
        let weak = weak.clone();
        let window_under = window_under.clone();
        let mark_active = mark_active.clone();
        move |gx, gy| {
            mark_active();
            if let Some(ui) = weak.upgrade() {
                let ad = ui.global::<AppData>();
                ad.set_dnd_x(gx);
                ad.set_dnd_y(gy);
            }
            if let Some((target, lx, ly)) = window_under(gx, gy) {
                let _ = cmd_tx.send(Command::PointerMotion {
                    id: target,
                    x: lx as f64,
                    y: ly as f64,
                });
            }
        }
    });

    // Drag-and-drop release: do a final motion to the drop target, then release
    // the button so the grab performs the drop. With no window under the cursor,
    // release on the origin window to end the grab (the drop is cancelled).
    ui.global::<Logic>().on_dnd_drop({
        let cmd_tx = cmd_tx.clone();
        let window_under = window_under.clone();
        let mark_active = mark_active.clone();
        move |origin, gx, gy, btn| {
            mark_active();
            let button = evdev_button(btn);
            if let Some((target, lx, ly)) = window_under(gx, gy) {
                let _ = cmd_tx.send(Command::PointerMotion {
                    id: target,
                    x: lx as f64,
                    y: ly as f64,
                });
                let _ = cmd_tx.send(Command::PointerButton {
                    id: target,
                    button,
                    pressed: false,
                });
            } else {
                let _ = cmd_tx.send(Command::PointerButton {
                    id: WindowId(origin as u64),
                    button,
                    pressed: false,
                });
            }
        }
    });

    ui.global::<Logic>().on_pointer_moved({
        let cmd_tx = cmd_tx.clone();
        let mark_active = mark_active.clone();
        let tasks = tasks.clone();
        let shared = shared.clone();
        let focused = focused.clone();
        let refresh_focus_highlight = refresh_focus_highlight.clone();
        move |id, x, y| {
            mark_active();
            let wid = WindowId(id as u64);
            // Focus-follows-mouse: hovering a *window* (not a popup/layer) gives
            // it keyboard focus. We don't rebuild/raise here, so the stack doesn't
            // churn as the pointer travels — only the keyboard focus follows.
            if tasks.borrow().settings().focus_follows_mouse
                && *focused.borrow() != Some(wid)
                && shared.borrow().rows.contains_key(&wid.0)
            {
                *focused.borrow_mut() = Some(wid);
                let _ = cmd_tx.send(Command::FocusWindow(wid));
                refresh_focus_highlight();
            }
            let _ = cmd_tx.send(Command::PointerMotion {
                id: wid,
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

    // Composed/typed text from the focused window's input method: type it
    // verbatim into the client (handles accents, AltGr layers and dead-key
    // results that only surface through the platform input method).
    ui.global::<Logic>().on_text_input({
        let cmd_tx = cmd_tx.clone();
        let mark_active = mark_active.clone();
        move |text| {
            mark_active();
            if !text.is_empty() {
                let _ = cmd_tx.send(Command::TypeText(text.to_string()));
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
        let dbus_daemon = dbus_daemon.clone();
        let rebuild_windows = rebuild_windows.clone();
        let refresh_tasks = refresh_tasks.clone();
        let windows_model = windows_model.clone();
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
        // When client damage (a new buffer) last arrived; we keep compositing for
        // a short tail afterwards, then let the window idle. Initialised to a
        // second ago so the desktop idles immediately — via `checked_sub` so it
        // can't panic when the monotonic clock is still within 1s of its epoch.
        let last_damage = std::cell::Cell::new(
            Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
        );
        let github_events = github.as_ref().map(|g| g.events.clone());
        let issue_results_model = issue_results_model.clone();
        let pending_link = pending_link.clone();
        let toast_tx = toast_tx.clone();
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
                        // Start a private session bus so clients don't remote
                        // into the parent session's app servers.
                        if dbus_daemon.borrow().is_none() {
                            if let Some((child, addr)) =
                                config::start_private_dbus(&env.wayland_display, &env.runtime_dir)
                            {
                                env.dbus_address = Some(addr);
                                *dbus_daemon.borrow_mut() = Some(child);
                            }
                        }
                        log::info!("compositor ready: {env:?}");
                    }
                    Event::WindowAdded(id) => {
                        // Give the new window a floating frame and size its client
                        // surface to that frame's content area.
                        let (cw, ch) = shared.borrow().content;
                        let geom = default_geom(id.0, cw, ch);
                        let decorated = {
                            let mut s = shared.borrow_mut();
                            let meta = s.meta.entry(id.0).or_default();
                            meta.geom = geom;
                            meta.decorated
                        };
                        let (w, h) = geom.content_size(decorated);
                        let _ = cmd_tx.send(Command::ResizeWindow { id, width: w, height: h });
                        tasks.borrow_mut().assign_window(id);
                        *focused.borrow_mut() = Some(id);
                        let _ = cmd_tx.send(Command::FocusWindow(id));
                        dirty_windows = true;
                    }
                    Event::MoveRequested(id) => {
                        // The client dragged its own (client-side) title bar.
                        // Flag the window so the view drives the floating move
                        // from the in-flight pointer drag.
                        if let Some(ui) = weak.upgrade() {
                            ui.global::<AppData>().set_moving_window(id.0 as i32);
                        }
                    }
                    Event::ResizeRequested { id, edges } => {
                        // The client grabbed one of its own (client-side) edges.
                        // Flag the window + edges so the view drives the resize
                        // from the in-flight pointer drag, like its own grips.
                        if let Some(ui) = weak.upgrade() {
                            let ad = ui.global::<AppData>();
                            ad.set_resizing_window(id.0 as i32);
                            ad.set_resizing_edges(edges as i32);
                        }
                    }
                    Event::MinimizeRequested(id) => {
                        // A client-side-decorated window (e.g. GNOME Terminal's
                        // header-bar button) asked to be minimized. Apply the same
                        // logic as the server-side minimize button. The window is
                        // visible when its button is clicked, so the toggle
                        // minimizes it.
                        if let Some(ui) = weak.upgrade() {
                            ui.global::<Logic>().invoke_minimize_window(id.0 as i32);
                        }
                    }
                    Event::MaximizeRequested { id, maximized } => {
                        // A client asked to be (un)maximized — a CSD window's own
                        // maximize button, or an X11 _NET_WM_STATE toggle. Reuse
                        // the toggle handler (frame geometry + SetMaximized +
                        // ResizeWindow) but only when the state actually changes,
                        // so a repeated request can't flip it the wrong way.
                        if tasks.borrow().is_maximized(id) != maximized {
                            if let Some(ui) = weak.upgrade() {
                                ui.global::<Logic>().invoke_maximize_window(id.0 as i32);
                            }
                        }
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
                        let geom = {
                            let mut s = shared.borrow_mut();
                            let meta = s.meta.entry(id.0).or_default();
                            if meta.decorated == decorated {
                                None
                            } else {
                                meta.decorated = decorated;
                                Some(meta.geom)
                            }
                        };
                        // Adding/removing the title bar changes the content height;
                        // re-size the client to match.
                        if let Some(geom) = geom {
                            let (w, h) = geom.content_size(decorated);
                            let _ = cmd_tx.send(Command::ResizeWindow { id, width: w, height: h });
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
                        min_w,
                        min_h,
                        max_w,
                        max_h,
                    } => {
                        let mut s = shared.borrow_mut();
                        let (cw, ch) = s.content;
                        let meta = s.meta.entry(id.0).or_default();
                        meta.min_w = min_w;
                        meta.min_h = min_h;
                        meta.max_w = max_w;
                        meta.max_h = max_h;
                        let mut need_resize = false;
                        if meta.decorated != decorated {
                            meta.decorated = decorated;
                            need_resize = true;
                            dirty_windows = true;
                        }
                        // Honour the client's (possibly just-changed) min/max size
                        // hints: some clients raise their minimum size — e.g. a
                        // drag-and-drop target expanding on drop — and then render
                        // at that minimum, which would overflow the smaller
                        // host-driven frame (the contents look mysteriously
                        // resized). Re-clamp the frame to the client's own hint
                        // bounds (not the global floor, so small dialogs aren't
                        // force-grown) so the window tracks what the client draws.
                        let clamped =
                            clamp_to_client_hints(meta.geom, decorated, min_w, min_h, max_w, max_h);
                        if (clamped.w - meta.geom.w).abs() > 0.5
                            || (clamped.h - meta.geom.h).abs() > 0.5
                        {
                            meta.geom.w = clamped.w;
                            meta.geom.h = clamped.h;
                            meta.geom.clamp_pos(cw, ch);
                            need_resize = true;
                        }
                        // A title-only change must NOT rebuild the window model:
                        // `set_vec` recreates every WindowView (and its focused
                        // FocusScope), so keyboard focus would be lost on every
                        // title update — e.g. each shell prompt re-sets the
                        // terminal title, stealing focus after every command.
                        // Patch the affected row in place instead.
                        let title_changed = meta.title != title;
                        if title_changed {
                            meta.title = title.clone();
                        }
                        meta.app_id = app_id;
                        let new_geom = meta.geom;
                        s.pending.insert(
                            id.0,
                            Frame {
                                width,
                                height,
                                pixels,
                            },
                        );
                        // Reconfigure the client to the frame's content area when
                        // the decoration mode was first learned or the frame was
                        // re-clamped to the size hints above.
                        if need_resize {
                            let (w, h) = new_geom.content_size(decorated);
                            let _ = cmd_tx.send(Command::ResizeWindow { id, width: w, height: h });
                        }
                        // First buffer for a window on the active task: ensure it
                        // has a row.
                        if tasks.borrow().is_visible(id) && !s.rows.contains_key(&id.0) {
                            dirty_windows = true;
                        }
                        // Apply title / geometry changes in place (unless a rebuild
                        // is already happening this tick for another reason).
                        if !dirty_windows && (title_changed || need_resize) {
                            if let Some(&row) = s.rows.get(&id.0) {
                                if let Some(mut t) = windows_model.row_data(row) {
                                    if title_changed {
                                        t.title = title.into();
                                    }
                                    // Only touch geometry when we actually
                                    // re-clamped, so a plain title update can't
                                    // fight an in-progress move/resize.
                                    if need_resize {
                                        t.x = new_geom.x;
                                        t.y = new_geom.y;
                                        t.width = new_geom.w;
                                        t.height = new_geom.h;
                                    }
                                    windows_model.set_row_data(row, t);
                                }
                            }
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
                        // Slide the popup back inside the content area if the
                        // positioner would put it off-screen (a poor man's
                        // xdg_positioner constraint_adjustment).
                        let (x, y) = clamp_popup(
                            px + ox as f32,
                            py + oy as f32,
                            width as f32,
                            height as f32,
                            s.content,
                        );
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
                    Event::DragStarted => {
                        if let Some(ui) = weak.upgrade() {
                            ui.global::<AppData>().set_dnd_active(true);
                        }
                    }
                    Event::DragEnded => {
                        if let Some(ui) = weak.upgrade() {
                            let ad = ui.global::<AppData>();
                            ad.set_dnd_active(false);
                            ad.set_dnd_w(0.0);
                            ad.set_dnd_h(0.0);
                            ad.set_dnd_icon(slint::Image::default());
                        }
                    }
                    Event::DragIcon {
                        width,
                        height,
                        pixels,
                        ..
                    } => {
                        if let Some(ui) = weak.upgrade() {
                            let img = rgba_to_image(width, height, &pixels);
                            let ad = ui.global::<AppData>();
                            ad.set_dnd_icon(img);
                            ad.set_dnd_w(width as f32);
                            ad.set_dnd_h(height as f32);
                        }
                    }
                    Event::CursorShape(code) => {
                        // The focused client requested a cursor shape; publish it
                        // so the hovered window shows it (the UI owns the pointer).
                        if let Some(ui) = weak.upgrade() {
                            ui.global::<AppData>().set_client_cursor(code as i32);
                        }
                    }
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

            // GitHub results: search hits populate the wizard; activity on a
            // linked issue/PR raises the task's dot and posts a toast.
            if let Some(events) = &github_events {
                let mut tasks_dirty = false;
                while let Ok(ev) = events.try_recv() {
                    match ev {
                        github::GhEvent::SearchResults(hits) => {
                            let selected = pending_link.borrow().clone();
                            let rows: Vec<IssueResult> = hits
                                .into_iter()
                                .map(|h| IssueResult {
                                    selected: selected
                                        .as_ref()
                                        .is_some_and(|s| s.slug == h.slug && s.number == h.number),
                                    slug: h.slug.into(),
                                    number: h.number as i32,
                                    title: h.title.into(),
                                    url: h.url.into(),
                                })
                                .collect();
                            issue_results_model.set_vec(rows);
                            if let Some(ui) = weak.upgrade() {
                                ui.global::<AppData>().set_issue_searching(false);
                            }
                        }
                        github::GhEvent::Activity { task_id, updated_at, url, title } => {
                            let tid = focuswm_shell::TaskId(task_id);
                            // Set on genuinely new activity: (slug, number) to toast.
                            let mut alert: Option<(String, u64)> = None;
                            {
                                let mut list = tasks.borrow_mut();
                                if let Some(task) = list.get_mut(tid) {
                                    if let Some(link) = task.github.as_mut() {
                                        link.url = url;
                                        link.title = title.clone();
                                        match link.last_seen {
                                            // First observation: record a baseline,
                                            // don't alert for pre-existing activity.
                                            None => link.last_seen = Some(updated_at),
                                            Some(prev) if updated_at > prev => {
                                                link.last_seen = Some(updated_at);
                                                alert = Some((link.slug.clone(), link.number));
                                            }
                                            Some(_) => {}
                                        }
                                    }
                                }
                                if alert.is_some() {
                                    list.notify(tid);
                                }
                            }
                            if let Some((slug, number)) = alert {
                                tasks_dirty = true;
                                internal_toast(
                                    &toast_tx,
                                    "GitHub",
                                    &format!("New activity on {slug}#{number}"),
                                    &title,
                                );
                            }
                        }
                        github::GhEvent::Error(msg) => {
                            if let Some(ui) = weak.upgrade() {
                                ui.global::<AppData>().set_issue_searching(false);
                            }
                            internal_toast(&toast_tx, "GitHub", "Request failed", &msg);
                        }
                    }
                }
                if tasks_dirty {
                    persist::save(&tasks.borrow());
                    refresh_tasks();
                }
            }

            // Keep the compositor output sized to the host window's content area.
            if let Some(ui) = weak.upgrade() {
                // Floating frames keep their size; re-clamp them when the content
                // area changes so a title bar can't end up off-screen.
                if sync_output_size(&ui, &cmd_tx, &shared) {
                    rebuild_windows();
                }
                // New client frames are uploaded to GL textures in the rendering
                // notifier's `BeforeRendering` hook, which only runs when the
                // window repaints. Rather than composite every frame forever
                // while a window is shown, only repaint when there's *recent*
                // client damage (a buffer waiting to upload, plus a short tail to
                // absorb redraw-scheduling lag and brief client follow-ups) or a
                // UI animation is in flight — so a static desktop truly idles.
                let damage = {
                    let s = shared.borrow();
                    !s.pending.is_empty() || !s.pending_dmabuf.is_empty()
                };
                if damage {
                    last_damage.set(Instant::now());
                }
                let in_tail = last_damage.get().elapsed() < Duration::from_millis(150);
                if damage || in_tail || ui.window().has_active_animations() {
                    ui.window().request_redraw();
                }
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

    // Poll linked GitHub issues/PRs for new activity: shortly after start and
    // then every couple of minutes. Results arrive as GhEvent::Activity.
    let github_poll_timer = slint::Timer::default();
    if let Some(requests) = github.as_ref().map(|g| g.requests.clone()) {
        let poll: Rc<dyn Fn()> = {
            let tasks = tasks.clone();
            Rc::new(move || {
                for t in tasks.borrow().tasks() {
                    if let Some(link) = &t.github {
                        let _ = requests.send(github::Request::Poll {
                            task_id: t.id.0,
                            slug: link.slug.clone(),
                            number: link.number,
                        });
                    }
                }
            })
        };
        poll();
        github_poll_timer.start(slint::TimerMode::Repeated, Duration::from_secs(150), {
            let poll = poll.clone();
            move || poll()
        });
    }

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

/// Slide a popup's top-left so a `w`×`h` popup stays inside the `(cw, ch)`
/// content area (approximating `xdg_positioner`'s slide constraint adjustment,
/// which we don't resolve compositor-side because only the UI knows window
/// positions). Oversized popups pin to the top/left edge.
fn clamp_popup(x: f32, y: f32, w: f32, h: f32, (cw, ch): (f32, f32)) -> (f32, f32) {
    (
        x.min(cw - w).max(0.0),
        y.min(ch - h).max(0.0),
    )
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
        // Anchor to the parent window's floating frame: its top-left, dropped
        // below the title bar when the frame is decorated.
        if let Some(meta) = shared.meta.get(&parent.0) {
            let bar = if meta.decorated { TITLE_BAR_H } else { 0.0 };
            return (meta.geom.x, meta.geom.y + bar);
        }
        return (0.0, 0.0);
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
            // Only swap the texture: a window's `width`/`height` are its floating
            // frame size (host-driven), not the buffer's pixel size.
            t.texture = image;
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

/// Raise the open-file soft limit (`RLIMIT_NOFILE`) to the hard limit. Best
/// effort: logs and carries on if the limits can't be read or set.
fn raise_open_file_limit() {
    // SAFETY: `getrlimit`/`setrlimit` only read/write the local `rlimit` struct.
    unsafe {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            log::warn!("could not read RLIMIT_NOFILE: {}", std::io::Error::last_os_error());
            return;
        }
        if lim.rlim_cur >= lim.rlim_max {
            return;
        }
        lim.rlim_cur = lim.rlim_max;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
            log::warn!("could not raise RLIMIT_NOFILE: {}", std::io::Error::last_os_error());
        } else {
            log::info!("raised open-file limit to {}", lim.rlim_max);
        }
    }
}

/// The current local calendar day, "YYYY-MM-DD".
fn today() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// Format a duration in seconds as "Xh Ym Zs" (dropping leading zero units), so
/// reports show the exact tracked time down to the second — nothing is lost to
/// whole-minute rounding.
fn fmt_dur(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// Short duration for the bar-graph labels: "2h", "30m", "45s", or "" for zero.
fn fmt_short(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 0 {
        format!("{h}h")
    } else if m > 0 {
        format!("{m}m")
    } else if secs > 0 {
        format!("{secs}s")
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

/// Track the host window size and resize the compositor output to the content
/// area (the window region right of the sidebar). Floating windows keep their
/// own geometry; only the virtual output follows the host. Returns `true` when
/// the content size changed (so the caller can re-clamp/rebuild window frames).
fn sync_output_size(
    ui: &Desktop,
    cmd_tx: &focuswm_wayland::CommandSender<Command>,
    shared: &Rc<RefCell<Shared>>,
) -> bool {
    thread_local! {
        static LAST: RefCell<(i32, i32)> = const { RefCell::new((0, 0)) };
    }
    let size = ui.window().size();
    let scale = ui.window().scale_factor().max(1.0);
    let logical_w = (size.width as f32 / scale) as i32;
    let logical_h = (size.height as f32 / scale) as i32;
    // Content area excludes only the sidebar; there is no top panel.
    let content_w = (logical_w - SIDEBAR_W).max(1);
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
        shared.borrow_mut().content = (content_w as f32, content_h as f32);
        let _ = cmd_tx.send(Command::ResizeOutput {
            width: content_w,
            height: content_h,
        });
    }
    changed
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

/// Forward a key press to the focused client. Three paths:
///
/// 1. Layout-independent special/navigation keys (Enter, Tab, Backspace, arrows,
///    F-keys, …) → a keycode tap on the compositor's base layout.
/// 2. Ctrl[+Alt] shortcuts (Ctrl+C, Ctrl+Shift+V, …) → the base key resolved on
///    the US layout, tapped with the held modifiers.
/// 3. Everything else is text the host already composed — letters, digits,
///    symbols, accents, AltGr layers, dead-key results — so type the Unicode
///    verbatim. This is what makes non-US layouts (e.g. AZERTY) work: the host
///    did the layout/composition, and we deliver the exact character rather than
///    trying to re-derive a US keycode for it.
fn forward_key(
    cmd_tx: &focuswm_wayland::CommandSender<Command>,
    text: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
) {
    if let Some(keycode) = special_keycode(text) {
        send_key_tap(cmd_tx, keycode, false, ctrl, alt, shift);
        return;
    }
    if ctrl {
        if let Some((keycode, needs_shift)) = evdev_keycode(text) {
            send_key_tap(cmd_tx, keycode, needs_shift, ctrl, alt, shift);
        }
        return;
    }
    let typed: String = text.chars().filter(|c| !c.is_control()).collect();
    if !typed.is_empty() {
        let _ = cmd_tx.send(Command::TypeText(typed));
    }
}

/// The keycode for a layout-independent special/navigation key (control codes
/// and Slint's private-use key encodings, plus space), or `None` for printable
/// text that should be typed as Unicode instead.
fn special_keycode(text: &str) -> Option<u32> {
    let ch = text.chars().next()?;
    let is_special =
        ch == ' ' || ch.is_control() || ('\u{F700}'..='\u{F8FF}').contains(&ch);
    if !is_special {
        return None;
    }
    evdev_keycode(text).map(|(keycode, _)| keycode)
}

/// Tap a key: press any held modifiers, press+release the key, release modifiers.
fn send_key_tap(
    cmd_tx: &focuswm_wayland::CommandSender<Command>,
    keycode: u32,
    needs_shift: bool,
    ctrl: bool,
    alt: bool,
    shift: bool,
) {
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
    fn fmt_dur_shows_exact_seconds() {
        assert_eq!(fmt_dur(0), "0s");
        assert_eq!(fmt_dur(45), "45s");
        assert_eq!(fmt_dur(90), "1m 30s");
        assert_eq!(fmt_dur(3600), "1h 0m 0s");
        assert_eq!(fmt_dur(3661), "1h 1m 1s");
        assert_eq!(fmt_short(0), "");
        assert_eq!(fmt_short(45), "45s");
        assert_eq!(fmt_short(120), "2m");
        assert_eq!(fmt_short(7200), "2h");
    }

    #[test]
    fn buttons_map_to_evdev() {
        assert_eq!(evdev_button(1), 0x110);
        assert_eq!(evdev_button(2), 0x111);
        assert_eq!(evdev_button(3), 0x112);
    }

    #[test]
    fn special_keys_route_to_keycodes_text_does_not() {
        // Navigation / control keys keep the keycode path.
        assert_eq!(special_keycode("\u{000a}"), Some(28)); // Return
        assert_eq!(special_keycode("\u{0008}"), Some(14)); // Backspace
        assert_eq!(special_keycode(" "), Some(57)); // Space
        assert_eq!(special_keycode("\u{F700}"), Some(103)); // Up arrow
        assert_eq!(special_keycode("\u{F704}"), Some(59)); // F1
        // Printable text (incl. non-US / accented) is typed as Unicode instead.
        assert_eq!(special_keycode("a"), None);
        assert_eq!(special_keycode("~"), None);
        assert_eq!(special_keycode("é"), None);
        assert_eq!(special_keycode("1"), None);
    }

    #[test]
    fn popup_clamped_inside_content() {
        let area = (1000.0, 700.0);
        // Fits: untouched.
        assert_eq!(clamp_popup(100.0, 100.0, 200.0, 150.0, area), (100.0, 100.0));
        // Overflows right/bottom: slides back in.
        assert_eq!(clamp_popup(950.0, 650.0, 200.0, 150.0, area), (800.0, 550.0));
        // Negative: pins to the origin.
        assert_eq!(clamp_popup(-30.0, -10.0, 200.0, 150.0, area), (0.0, 0.0));
        // Bigger than the area: pins to the top-left rather than going negative.
        assert_eq!(clamp_popup(50.0, 50.0, 1200.0, 800.0, area), (0.0, 0.0));
    }

    #[test]
    fn reorder_delta_rounds_to_nearest_slot() {
        assert_eq!(reorder_delta(0.0), 0);
        assert_eq!(reorder_delta(20.0), 0); // less than half a row → stay
        assert_eq!(reorder_delta(40.0), 1); // past the midpoint → down one
        assert_eq!(reorder_delta(64.0), 1);
        assert_eq!(reorder_delta(130.0), 2);
        assert_eq!(reorder_delta(-40.0), -1); // up one
        assert_eq!(reorder_delta(-130.0), -2);
    }

    #[test]
    fn win_geom_content_size_accounts_for_title_bar() {
        let g = WinGeom { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
        // Decorated: the client area is the frame minus the title bar.
        assert_eq!(g.content_size(true), (800, 600 - TITLE_BAR_H as i32));
        // Undecorated: the whole frame is client area.
        assert_eq!(g.content_size(false), (800, 600));
    }

    #[test]
    fn win_geom_clamp_keeps_title_bar_reachable() {
        let (cw, ch) = (1000.0, 700.0);
        // Dragged far off the bottom-right: clamped back into reach.
        let mut g = WinGeom { x: 5000.0, y: 5000.0, w: 400.0, h: 300.0 };
        g.clamp_pos(cw, ch);
        assert!(g.x <= cw && g.y >= 0.0 && g.y <= ch - TITLE_BAR_H);
        // Dragged far off the top-left: still keeps part of the frame on-screen.
        let mut g = WinGeom { x: -5000.0, y: -5000.0, w: 400.0, h: 300.0 };
        g.clamp_pos(cw, ch);
        assert!(g.x + g.w >= 0.0 && g.y >= 0.0);
    }

    #[test]
    fn default_geom_fits_inside_content_and_cascades() {
        let (cw, ch) = (1040.0, 800.0);
        let a = default_geom(0, cw, ch);
        let b = default_geom(1, cw, ch);
        // Inside the content area and at least the minimum size.
        assert!(a.w >= MIN_WIN_W && a.h >= MIN_WIN_H);
        assert!(a.x >= 0.0 && a.y >= 0.0 && a.x + a.w <= cw && a.y + a.h <= ch);
        // Successive windows cascade so they don't perfectly overlap.
        assert!(b.x > a.x && b.y > a.y);
    }

    // A WinMeta with the given content size hints (0 = unset), decorated.
    fn meta_with_hints(min_w: i32, min_h: i32, max_w: i32, max_h: i32) -> WinMeta {
        WinMeta { decorated: true, min_w, min_h, max_w, max_h, ..Default::default() }
    }

    #[test]
    fn frame_bounds_unset_hints_are_floor_and_infinite() {
        // No client hints: min is the global floor, max is unbounded.
        let (min_w, min_h, max_w, max_h) = meta_with_hints(0, 0, 0, 0).frame_bounds();
        assert_eq!((min_w, min_h), (MIN_WIN_W, MIN_WIN_H));
        assert!(max_w.is_infinite() && max_h.is_infinite());
    }

    #[test]
    fn frame_bounds_adds_title_bar_to_height_only() {
        // A 400×300 content min/max becomes a frame min/max with the title bar
        // added to the *height* axis only (width has no horizontal decoration).
        let (min_w, min_h, max_w, max_h) = meta_with_hints(400, 300, 400, 300).frame_bounds();
        assert_eq!(min_w, 400.0);
        assert_eq!(max_w, 400.0);
        assert_eq!(min_h, 300.0 + TITLE_BAR_H);
        assert_eq!(max_h, 300.0 + TITLE_BAR_H);
    }

    #[test]
    fn frame_bounds_undecorated_has_no_title_bar() {
        let mut m = meta_with_hints(400, 300, 0, 0);
        m.decorated = false;
        let (_, min_h, _, _) = m.frame_bounds();
        assert_eq!(min_h, 300.0); // no title bar added
    }

    #[test]
    fn frame_bounds_keeps_max_at_least_min() {
        // A client whose max is below the global floor still yields max ≥ min,
        // so callers can clamp(min, max) without panicking.
        let (min_w, min_h, max_w, max_h) = meta_with_hints(0, 0, 50, 40).frame_bounds();
        assert_eq!((max_w, max_h), (min_w, min_h));
        assert!(max_w >= min_w && max_h >= min_h);
    }

    #[test]
    fn resize_right_and_bottom_keep_origin() {
        let bounds = (MIN_WIN_W, MIN_WIN_H, f32::INFINITY, f32::INFINITY);
        let mut g = WinGeom { x: 100.0, y: 50.0, w: 400.0, h: 300.0 };
        g.resize_by(2 | 8, 60.0, 40.0, bounds); // drag right + bottom edges out
        assert_eq!((g.x, g.y), (100.0, 50.0)); // origin unmoved
        assert_eq!((g.w, g.h), (460.0, 340.0));
    }

    #[test]
    fn resize_left_and_top_anchor_opposite_edge() {
        let bounds = (MIN_WIN_W, MIN_WIN_H, f32::INFINITY, f32::INFINITY);
        let mut g = WinGeom { x: 100.0, y: 50.0, w: 400.0, h: 300.0 };
        let (right, bottom) = (g.x + g.w, g.y + g.h);
        g.resize_by(1 | 4, 40.0, 30.0, bounds); // drag left + top edges inward
        assert_eq!((g.w, g.h), (360.0, 270.0));
        // The right/bottom edges stay put; the origin moved.
        assert!((g.x + g.w - right).abs() < 0.01);
        assert!((g.y + g.h - bottom).abs() < 0.01);
    }

    #[test]
    fn resize_clamps_to_min_and_max_hints() {
        let bounds = (200.0, 150.0, 600.0, 500.0);
        // Shrinking past the minimum is clamped to the minimum.
        let mut g = WinGeom { x: 0.0, y: 0.0, w: 300.0, h: 250.0 };
        g.resize_by(2 | 8, -1000.0, -1000.0, bounds);
        assert_eq!((g.w, g.h), (200.0, 150.0));
        // Growing past the maximum is clamped to the maximum.
        let mut g = WinGeom { x: 0.0, y: 0.0, w: 300.0, h: 250.0 };
        g.resize_by(2 | 8, 1000.0, 1000.0, bounds);
        assert_eq!((g.w, g.h), (600.0, 500.0));
    }

    #[test]
    fn snap_geom_covers_halves_quarters_and_maximize() {
        let (cw, ch) = (1000.0, 800.0);
        assert_eq!(snap_geom(1, cw, ch), WinGeom { x: 0.0, y: 0.0, w: 500.0, h: 800.0 });
        assert_eq!(snap_geom(2, cw, ch), WinGeom { x: 500.0, y: 0.0, w: 500.0, h: 800.0 });
        assert_eq!(snap_geom(7, cw, ch), WinGeom { x: 500.0, y: 400.0, w: 500.0, h: 400.0 });
        // Zone 3 (and any unknown zone) maximizes to the full area.
        assert_eq!(snap_geom(3, cw, ch), WinGeom { x: 0.0, y: 0.0, w: cw, h: ch });
    }

    #[test]
    fn clamp_to_client_hints_grows_shrinks_and_ignores_unset() {
        let g = WinGeom { x: 10.0, y: 20.0, w: 300.0, h: 300.0 };
        // No hints: unchanged (no global floor applied here).
        assert_eq!(clamp_to_client_hints(g, true, 0, 0, 0, 0), g);
        // A raised minimum grows the frame; the title bar is added to height.
        let grown = clamp_to_client_hints(g, true, 500, 400, 0, 0);
        assert_eq!(grown.w, 500.0);
        assert_eq!(grown.h, 400.0 + TITLE_BAR_H);
        // Position is left untouched.
        assert_eq!((grown.x, grown.y), (10.0, 20.0));
        // A maximum below the current size shrinks the frame (fixed-size dialog).
        let shrunk = clamp_to_client_hints(g, true, 0, 0, 150, 150);
        assert_eq!(shrunk.w, 150.0);
        assert_eq!(shrunk.h, 150.0 + TITLE_BAR_H);
        // Undecorated: no title bar added to the height bound.
        let u = clamp_to_client_hints(g, false, 0, 400, 0, 0);
        assert_eq!(u.h, 400.0);
    }

    #[test]
    fn content_hit_excludes_title_bar_and_returns_local_coords() {
        let g = WinGeom { x: 100.0, y: 50.0, w: 400.0, h: 300.0 };
        // Decorated: the title bar (top TITLE_BAR_H) is not part of the content.
        assert_eq!(content_hit(g, true, 110.0, 55.0), None); // over the title bar
        // Just below the title bar maps to surface-local (10, 0).
        assert_eq!(content_hit(g, true, 110.0, 50.0 + TITLE_BAR_H), Some((10.0, 0.0)));
        // A point well inside the content maps relative to the content origin.
        assert_eq!(
            content_hit(g, true, 300.0, 200.0),
            Some((200.0, 200.0 - 50.0 - TITLE_BAR_H))
        );
        // Outside the frame entirely.
        assert_eq!(content_hit(g, true, 600.0, 200.0), None);
        // Undecorated: the whole frame is content, so the top-left is (0, 0).
        assert_eq!(content_hit(g, false, 100.0, 50.0), Some((0.0, 0.0)));
    }

    #[test]
    fn maximized_geom_fills_or_centres_within_max() {
        let (cw, ch) = (1000.0, 800.0);
        // A resizable client fills the whole area, top-left anchored.
        let g = maximized_geom(cw, ch, f32::INFINITY, f32::INFINITY);
        assert_eq!(g, WinGeom { x: 0.0, y: 0.0, w: cw, h: ch });
        // A capped client is sized to its max and centred.
        let g = maximized_geom(cw, ch, 600.0, 400.0);
        assert_eq!(g, WinGeom { x: 200.0, y: 200.0, w: 600.0, h: 400.0 });
    }
}
