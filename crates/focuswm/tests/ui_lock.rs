//! Headless UI test: the lock screen's Unlock button reaches the host.

use std::cell::Cell;
use std::rc::Rc;

use i_slint_backend_testing as testing;
use slint::ComponentHandle;

slint::include_modules!();

#[test]
fn lock_screen_unlock_reaches_host() {
    testing::init_no_event_loop();

    let ui = Desktop::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(1280, 800));
    ui.set_locked(true);

    let unlocked = Rc::new(Cell::new(false));
    {
        let unlocked = unlocked.clone();
        ui.global::<Logic>().on_unlock(move || unlocked.set(true));
    }

    testing::ElementHandle::find_by_accessible_label(&ui, "Unlock")
        .next()
        .expect("Unlock button")
        .invoke_accessible_default_action();

    assert!(unlocked.get(), "clicking Unlock should ask the host to unlock");
}
