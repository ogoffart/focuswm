//! Headless UI test: the run-command launcher forwards the typed command to the
//! host and the Run button is gated on non-empty input.

use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing as testing;
use slint::ComponentHandle;

slint::include_modules!();

#[test]
fn launcher_run_forwards_command() {
    testing::init_no_event_loop();

    let ui = Desktop::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(1280, 800));

    let got = Rc::new(RefCell::new(String::new()));
    {
        let got = got.clone();
        ui.global::<Logic>().on_run_command(move |cmd| *got.borrow_mut() = cmd.to_string());
    }

    ui.set_launcher_open(true);

    let run = || {
        testing::ElementHandle::find_by_accessible_label(&ui, "Run")
            .next()
            .expect("Run button")
    };
    // Empty input -> Run disabled.
    assert_eq!(run().accessible_enabled(), Some(false));

    testing::ElementHandle::find_by_element_id(&ui, "Launcher::edit")
        .next()
        .expect("command field")
        .set_accessible_value("alacritty -e htop");
    assert_eq!(run().accessible_enabled(), Some(true));

    run().invoke_accessible_default_action();
    assert_eq!(*got.borrow(), "alacritty -e htop");
}
