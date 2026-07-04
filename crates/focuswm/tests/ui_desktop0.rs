//! Headless UI test: clicking the "Desktop 0" scratch row switches to it, and a
//! task row switches to that task.

use std::cell::Cell;
use std::rc::Rc;

use i_slint_backend_testing as testing;
use i_slint_core::items::PointerEventButton;
use slint::{ComponentHandle, ModelRc, VecModel};

slint::include_modules!();

#[test]
fn desktop0_and_task_rows_switch() {
    testing::init_no_event_loop();

    let ui = Desktop::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(1280, 800));
    let task = TaskItem { id: 3, name: "Docs".into(), category: "work".into(), ..Default::default() };
    ui.global::<AppData>()
        .set_tasks(ModelRc::from(Rc::new(VecModel::from(vec![task]))));

    let to_desktop0 = Rc::new(Cell::new(false));
    let switched = Rc::new(Cell::new(-1));
    {
        let d = to_desktop0.clone();
        ui.global::<Logic>().on_switch_to_desktop0(move || d.set(true));
    }
    {
        let s = switched.clone();
        ui.global::<Logic>().on_switch_task(move |id| s.set(id));
    }

    testing::ElementHandle::find_by_element_id(&ui, "Desktop0Row::touch")
        .next()
        .expect("the Desktop 0 row")
        .mock_single_click(PointerEventButton::Left);
    assert!(to_desktop0.get(), "clicking Desktop 0 switches to the scratch desktop");

    testing::ElementHandle::find_by_element_id(&ui, "TaskRow::touch")
        .next()
        .expect("a task row")
        .mock_single_click(PointerEventButton::Left);
    assert_eq!(switched.get(), 3, "clicking a task row switches to that task");
}
