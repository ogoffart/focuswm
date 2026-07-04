//! Headless UI test: the report's day/week navigation arrows reach the host.

use std::cell::Cell;
use std::rc::Rc;

use i_slint_backend_testing as testing;
use slint::ComponentHandle;

slint::include_modules!();

#[test]
fn report_arrows_navigate_day_and_week() {
    testing::init_no_event_loop();

    let ui = Desktop::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(1280, 800));
    ui.set_report_open(true);

    let prev_day = Rc::new(Cell::new(0));
    let prev_week = Rc::new(Cell::new(0));
    {
        let prev_day = prev_day.clone();
        ui.global::<Logic>().on_report_prev_day(move || prev_day.set(prev_day.get() + 1));
    }
    {
        let prev_week = prev_week.clone();
        ui.global::<Logic>().on_report_prev_week(move || prev_week.set(prev_week.get() + 1));
    }

    // Two "‹" back-arrows exist: the first steps the day, the second the week.
    let backs: Vec<_> = testing::ElementHandle::find_by_accessible_label(&ui, "‹").collect();
    assert_eq!(backs.len(), 2, "a day and a week back-arrow");
    backs[0].invoke_accessible_default_action();
    backs[1].invoke_accessible_default_action();

    assert_eq!(prev_day.get(), 1, "first ‹ steps the day back");
    assert_eq!(prev_week.get(), 1, "second ‹ steps the week back");
}
