//! Headless UI test: the settings dialog seeds its fields from `SettingsData`
//! and Save forwards the (possibly edited) values to the host.

use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing as testing;
use slint::ComponentHandle;

slint::include_modules!();

#[test]
fn settings_save_forwards_fields() {
    testing::init_no_event_loop();

    let ui = Desktop::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(1280, 800));

    // Seed the current settings and open the dialog (fields bind to these).
    ui.global::<SettingsData>().set_terminal("foot".into());
    ui.global::<SettingsData>().set_browser("firefox".into());
    ui.global::<SettingsData>().set_categories_csv("work, play".into());
    ui.global::<SettingsData>().set_idle_minutes("7".into());
    ui.set_settings_open(true);

    let got = Rc::new(RefCell::new(None));
    {
        let got = got.clone();
        ui.global::<Logic>().on_save_settings(move |term, brow, cats, idle, _ffm| {
            *got.borrow_mut() = Some((
                term.to_string(),
                brow.to_string(),
                cats.to_string(),
                idle.to_string(),
            ));
        });
    }

    // Click the settings dialog's Save (distinguish from the task-settings Save).
    testing::ElementHandle::find_by_accessible_label(&ui, "Save")
        .find(|e| e.id().is_some_and(|id| id.starts_with("Settings::")))
        .expect("settings Save button")
        .invoke_accessible_default_action();

    assert_eq!(
        got.take(),
        Some(("foot".into(), "firefox".into(), "work, play".into(), "7".into())),
    );
}
