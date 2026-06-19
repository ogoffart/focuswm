//! Headless UI test: right-clicking a task row requests that task's settings
//! dialog (via the `open-task-settings` Logic callback the host handles).

use std::cell::Cell;
use std::rc::Rc;

use i_slint_backend_testing as testing;
use i_slint_core::items::PointerEventButton;
use slint::{ComponentHandle, ModelRc, VecModel};

slint::include_modules!();

#[test]
fn right_clicking_a_task_opens_its_settings() {
    testing::init_no_event_loop();

    let ui = Desktop::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(1280, 800));

    // Seed a single task in the sidebar.
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

    // Capture the id passed to the (host-handled) open-task-settings callback.
    let captured = Rc::new(Cell::new(-1));
    {
        let captured = captured.clone();
        ui.global::<Logic>()
            .on_open_task_settings(move |id| captured.set(id));
    }

    // A left click switches tasks; a right click opens the settings dialog.
    let row = testing::ElementHandle::find_by_element_id(&ui, "TaskRow::touch")
        .next()
        .expect("a task row should exist");
    row.mock_single_click(PointerEventButton::Right);

    assert_eq!(
        captured.get(),
        7,
        "right-clicking a task should request its settings dialog"
    );
}
