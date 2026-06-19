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
        },
        WindowTile {
            id: 11,
            title: "cargo test".into(),
            texture: solid_image(24, 24, 37),
            width: 800.0,
            height: 600.0,
        },
    ];
    ui.global::<AppData>()
        .set_windows(ModelRc::from(Rc::new(VecModel::from(windows))));
    ui.global::<AppData>().set_active_task(0);
    ui.global::<AppData>().set_active_name("Fix login bug".into());
    ui.global::<AppData>().set_categories(ModelRc::from(Rc::new(VecModel::from(
        ["work", "personal", "learning"]
            .iter()
            .map(|c| SharedString::from(*c))
            .collect::<Vec<_>>(),
    ))));

    save(&ui, &window, PhysicalSize::new(1280, 800), "shot_desktop.png");

    // Same scene with the creation wizard open.
    ui.set_wizard_open(true);
    save(&ui, &window, PhysicalSize::new(1280, 800), "shot_wizard.png");
}
