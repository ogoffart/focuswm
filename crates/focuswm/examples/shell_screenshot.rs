//! Headless render of the focuswm shell UI for visual verification.
//!
//! Uses Slint's software renderer (no GPU / no display needed) to rasterise the
//! `Desktop` scene into PNG files:
//!
//! ```sh
//! cargo run -p focuswm --example shell_screenshot
//! ```

use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, ModelRc, PhysicalSize, SharedString, VecModel};

slint::include_modules!();

struct TestPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

fn save(ui: &Desktop, window: &MinimalSoftwareWindow, size: PhysicalSize, name: &str) {
    window.set_size(size);
    slint::platform::update_timers_and_animations();
    let buffer = ui
        .window()
        .take_snapshot()
        .expect("software renderer should support take_snapshot");
    // take_snapshot leaves alpha at zero; drop it and write RGB.
    let rgb: Vec<u8> = buffer
        .as_bytes()
        .chunks_exact(4)
        .flat_map(|px| [px[0], px[1], px[2]])
        .collect();
    image::save_buffer(name, &rgb, buffer.width(), buffer.height(), image::ColorType::Rgb8)
        .expect("write png");
    println!("wrote {name} ({}x{})", buffer.width(), buffer.height());
}

/// A solid-colour image, standing in for a client window's texture.
fn solid_image(r: u8, g: u8, b: u8) -> slint::Image {
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(16, 16);
    for px in buf.make_mut_slice() {
        *px = slint::Rgba8Pixel { r, g, b, a: 255 };
    }
    slint::Image::from_rgba8(buf)
}

fn main() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint::platform::set_platform(Box::new(TestPlatform {
        window: window.clone(),
    }))
    .unwrap();

    let ui = Desktop::new().unwrap();

    // Seed a few sample tasks.
    let tasks = vec![
        TaskItem {
            id: 0,
            name: "Fix login bug".into(),
            category: "work".into(),
            minutes: 42,
            has_notification: false,
        },
        TaskItem {
            id: 1,
            name: "Review PR #128".into(),
            category: "work".into(),
            minutes: 15,
            has_notification: true,
        },
        TaskItem {
            id: 2,
            name: "Read Wayland book".into(),
            category: "learning".into(),
            minutes: 90,
            has_notification: false,
        },
    ];
    ui.global::<AppData>()
        .set_tasks(ModelRc::from(Rc::new(VecModel::from(tasks))));

    // A couple of sample windows for the active task, with solid-colour textures.
    let windows = vec![
        WindowTile {
            id: 10,
            title: "nvim — auth.rs".into(),
            texture: solid_image(40, 42, 54),
            width: 800.0,
            height: 600.0,
            decorated: false,
        },
        WindowTile {
            id: 11,
            title: "cargo test".into(),
            texture: solid_image(24, 24, 37),
            width: 800.0,
            height: 600.0,
            decorated: false,
        },
    ];
    ui.global::<AppData>()
        .set_windows(ModelRc::from(Rc::new(VecModel::from(windows))));
    ui.global::<AppData>().set_active_task(0);
    ui.global::<AppData>().set_active_name("Fix login bug".into());
    ui.global::<AppData>().set_clock_time("09:41".into());
    ui.global::<AppData>().set_clock_date("Friday, Jun 19".into());
    ui.global::<AppData>()
        .set_notifications(ModelRc::from(Rc::new(VecModel::from(vec![
            NotificationToast {
                id: 1,
                app: "Element".into(),
                summary: "New message from Alex".into(),
                body: "Can you review the worktree PR?".into(),
            },
            NotificationToast {
                id: 2,
                app: "focuswm".into(),
                summary: "Couldn't open application".into(),
                body: "failed to launch alacritty: No such file".into(),
            },
        ]))));
    ui.global::<AppData>().set_categories(ModelRc::from(Rc::new(VecModel::from(
        ["work", "personal", "learning"]
            .iter()
            .map(|c| SharedString::from(*c))
            .collect::<Vec<_>>(),
    ))));

    ui.global::<AppData>()
        .set_tray(ModelRc::from(Rc::new(VecModel::from(vec![
            TrayIcon { id: "1".into(), title: "Element".into(), icon: solid_image(137, 180, 250) },
            TrayIcon { id: "2".into(), title: "Network".into(), icon: solid_image(166, 227, 161) },
            TrayIcon { id: "3".into(), title: "Volume".into(), icon: solid_image(250, 179, 135) },
        ]))));

    save(&ui, &window, PhysicalSize::new(1280, 800), "shot_desktop.png");

    // Same scene with the creation wizard open.
    ui.set_wizard_open(true);
    save(&ui, &window, PhysicalSize::new(1280, 800), "shot_wizard.png");
    ui.set_wizard_open(false);

    // The time report, with sample figures.
    let cat_rows = vec![
        ReportRow { label: "work".into(), today: "1h 12m".into(), week: "6h 40m".into() },
        ReportRow { label: "learning".into(), today: "30m".into(), week: "3h 05m".into() },
        ReportRow { label: "personal".into(), today: "0m".into(), week: "45m".into() },
    ];
    let proj_rows = vec![
        ReportRow { label: "Fix login bug".into(), today: "42m".into(), week: "4h 10m".into() },
        ReportRow { label: "Review PR #128".into(), today: "30m".into(), week: "2h 30m".into() },
        ReportRow { label: "Read Wayland book".into(), today: "30m".into(), week: "3h 05m".into() },
    ];
    let bar = |label: &str, value: &str, fraction: f32, selected: bool| DayBar {
        label: label.into(),
        value: value.into(),
        fraction,
        selected,
    };
    let bars = vec![
        bar("Mon", "2h", 0.62, false),
        bar("Tue", "3h", 1.0, false),
        bar("Wed", "1h", 0.5, false),
        bar("Thu", "2h", 0.74, false),
        bar("Fri", "1h", 0.5, true),
        bar("Sat", "", 0.0, false),
        bar("Sun", "", 0.0, false),
    ];
    let rd = ui.global::<ReportData>();
    rd.set_day_label("Fri Jun 19".into());
    rd.set_week_label("Jun 15 – Jun 21".into());
    rd.set_day_total("1h 42m".into());
    rd.set_week_total("10h 30m".into());
    rd.set_can_forward(true);
    rd.set_week_bars(ModelRc::from(Rc::new(VecModel::from(bars))));
    rd.set_by_category(ModelRc::from(Rc::new(VecModel::from(cat_rows))));
    rd.set_by_project(ModelRc::from(Rc::new(VecModel::from(proj_rows))));
    ui.set_report_open(true);
    save(&ui, &window, PhysicalSize::new(1280, 800), "shot_report.png");
    ui.set_report_open(false);

    // The settings dialog.
    ui.global::<SettingsData>().set_terminal("alacritty".into());
    ui.global::<SettingsData>().set_browser("firefox".into());
    ui.global::<SettingsData>()
        .set_categories_csv("work, personal, meeting, learning, other".into());
    ui.global::<SettingsData>().set_idle_minutes("5".into());
    ui.set_settings_open(true);
    save(&ui, &window, PhysicalSize::new(1280, 800), "shot_settings.png");
    ui.set_settings_open(false);

    // The lock screen.
    ui.set_locked(true);
    save(&ui, &window, PhysicalSize::new(1280, 800), "shot_lock.png");
}
