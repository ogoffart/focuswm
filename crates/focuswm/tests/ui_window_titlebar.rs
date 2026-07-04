//! Headless UI test: the server-side title-bar buttons (minimize / maximize /
//! close) forward to the host with the window's id.

use std::cell::Cell;
use std::rc::Rc;

use i_slint_backend_testing as testing;
use i_slint_core::items::PointerEventButton;
use slint::{ComponentHandle, ModelRc, VecModel};

slint::include_modules!();

#[test]
fn titlebar_buttons_forward_window_actions() {
    testing::init_no_event_loop();

    let ui = Desktop::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(1280, 800));

    // One decorated window so the server-side title bar (and its buttons) exist.
    let tile = WindowTile {
        id: 5,
        title: "term".into(),
        width: 500.0,
        height: 360.0,
        decorated: true,
        focused: true,
        ..Default::default()
    };
    ui.global::<AppData>()
        .set_windows(ModelRc::from(Rc::new(VecModel::from(vec![tile]))));

    let minimized = Rc::new(Cell::new(-1));
    let maximized = Rc::new(Cell::new(-1));
    let closed = Rc::new(Cell::new(-1));
    {
        let m = minimized.clone();
        ui.global::<Logic>().on_minimize_window(move |id| m.set(id));
    }
    {
        let m = maximized.clone();
        ui.global::<Logic>().on_maximize_window(move |id| m.set(id));
    }
    {
        let c = closed.clone();
        ui.global::<Logic>().on_close_window(move |id| c.set(id));
    }

    let click = |id: &str| {
        testing::ElementHandle::find_by_element_id(&ui, id)
            .next()
            .unwrap_or_else(|| panic!("{id} should exist"))
            .mock_single_click(PointerEventButton::Left);
    };
    click("WindowView::min-ta");
    click("WindowView::max-ta");
    click("WindowView::close-ta");

    assert_eq!(minimized.get(), 5, "minimize button targets the window");
    assert_eq!(maximized.get(), 5, "maximize button targets the window");
    assert_eq!(closed.get(), 5, "close button targets the window");
}
