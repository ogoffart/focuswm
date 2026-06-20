//! Headless UI test: a task row's per-task settings still flow through to the
//! host. The right-click context menu opens as a native popup that the headless
//! testing backend can't drive, so this drives the settings *dialog* directly
//! (which the menu's "Settings…" entry opens) and asserts Save reaches the host
//! callback. It also guards that wrapping the row in a `ContextMenuArea` left
//! ordinary left-click task-switching working.

use std::cell::Cell;
use std::rc::Rc;

use i_slint_backend_testing as testing;
use i_slint_core::items::PointerEventButton;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

slint::include_modules!();

#[test]
fn task_row_clicks_and_settings_save_reach_the_host() {
    testing::init_no_event_loop();

    let ui = Desktop::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(1280, 800));

    // Seed a single task and the category list.
    let tasks = vec![TaskItem {
        id: 7,
        name: "Demo".into(),
        category: "work".into(),
        minutes: 0,
        has_notification: false,
        tint: slint::Color::from_rgb_u8(0x89, 0xb4, 0xfa),
    }];
    ui.global::<AppData>()
        .set_tasks(ModelRc::from(Rc::new(VecModel::from(tasks))));
    ui.global::<AppData>().set_active_task(7);
    ui.global::<AppData>().set_categories(ModelRc::from(Rc::new(VecModel::from(
        ["work", "personal"]
            .iter()
            .map(|c| SharedString::from(*c))
            .collect::<Vec<_>>(),
    ))));

    // Left-clicking the row still switches tasks (the row is now wrapped in a
    // ContextMenuArea — make sure that didn't swallow the click).
    let switched = Rc::new(Cell::new(-1));
    {
        let switched = switched.clone();
        ui.global::<Logic>()
            .on_switch_task(move |id| switched.set(id));
    }
    testing::ElementHandle::find_by_element_id(&ui, "TaskRow::touch")
        .next()
        .expect("a task row should exist")
        .mock_single_click(PointerEventButton::Left);
    assert_eq!(switched.get(), 7, "left-clicking a task should switch to it");

    // The settings dialog (opened by the menu's "Settings…" entry) saves back to
    // the host with the task id and edited fields.
    let saved = Rc::new(Cell::new(None));
    {
        let saved = saved.clone();
        ui.global::<Logic>().on_save_task_settings(move |id, name, _cat, color| {
            saved.set(Some((id, name.to_string(), color)));
        });
    }
    ui.global::<TaskSettingsData>().set_id(7);
    ui.global::<TaskSettingsData>().set_name("Renamed".into());
    ui.global::<TaskSettingsData>().set_selected_index(2);
    ui.set_task_settings_open(true);

    testing::ElementHandle::find_by_accessible_label(&ui, "Save")
        .find(|e| e.id().is_some_and(|id| id.starts_with("TaskSettings::")))
        .expect("the task settings dialog should have a Save button")
        .invoke_accessible_default_action();

    assert_eq!(
        saved.take(),
        Some((7, "Renamed".to_string(), 2)),
        "Save should send the edited task back to the host"
    );
}

