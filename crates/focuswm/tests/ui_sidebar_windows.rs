//! Headless UI test: the sidebar's window list forwards a left-click as
//! focus-window and the row's ✕ as close-window.

use std::cell::Cell;
use std::rc::Rc;

use i_slint_backend_testing as testing;
use i_slint_core::items::PointerEventButton;
use slint::{ComponentHandle, ModelRc, VecModel};

slint::include_modules!();

#[test]
fn sidebar_window_row_activates_and_closes() {
    testing::init_no_event_loop();

    let ui = Desktop::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(1280, 800));

    // An active task with one window makes the sidebar "WINDOWS" list appear.
    ui.global::<AppData>().set_active_task(1);
    let tile = WindowTile { id: 9, title: "nvim".into(), ..Default::default() };
    ui.global::<AppData>()
        .set_windows(ModelRc::from(Rc::new(VecModel::from(vec![tile]))));

    let focused = Rc::new(Cell::new(-1));
    let closed = Rc::new(Cell::new(-1));
    {
        let f = focused.clone();
        ui.global::<Logic>().on_focus_window(move |id| f.set(id));
    }
    {
        let c = closed.clone();
        ui.global::<Logic>().on_close_window(move |id| c.set(id));
    }

    // Left-click the row body -> focus.
    testing::ElementHandle::find_by_element_id(&ui, "WindowRow::touch")
        .next()
        .expect("a window row")
        .mock_single_click(PointerEventButton::Left);
    assert_eq!(focused.get(), 9, "clicking a window row focuses that window");

    // Click the row's ✕ -> close.
    testing::ElementHandle::find_by_element_id(&ui, "WindowRow::close-ta")
        .next()
        .expect("the row close button")
        .mock_single_click(PointerEventButton::Left);
    assert_eq!(closed.get(), 9, "the ✕ closes that window");
}
