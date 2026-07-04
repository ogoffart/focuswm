//! Escape closes whichever dialog overlay is open.

use i_slint_backend_testing as testing;
use slint::ComponentHandle;

slint::include_modules!();

fn press_escape(ui: &Desktop) {
    let esc = slint::SharedString::from("\u{001B}");
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: esc.clone() });
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::KeyReleased { text: esc });
}

#[test]
fn escape_closes_open_dialogs() {
    testing::init_no_event_loop();

    let ui = Desktop::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(1280, 800));

    // Settings dialog.
    ui.set_settings_open(true);
    press_escape(&ui);
    assert!(!ui.get_settings_open(), "Esc should close the settings dialog");

    // Task-settings dialog.
    ui.set_task_settings_open(true);
    press_escape(&ui);
    assert!(!ui.get_task_settings_open(), "Esc should close the task settings");

    // Report.
    ui.set_report_open(true);
    press_escape(&ui);
    assert!(!ui.get_report_open(), "Esc should close the report");

    // Wizard.
    ui.set_wizard_open(true);
    press_escape(&ui);
    assert!(!ui.get_wizard_open(), "Esc should close the wizard");

    // Launcher — its LineEdit grabs focus when shown, so this also covers the
    // capture path where focus sits inside the dialog.
    ui.set_launcher_open(true);
    press_escape(&ui);
    assert!(!ui.get_launcher_open(), "Esc should close the launcher");
}
