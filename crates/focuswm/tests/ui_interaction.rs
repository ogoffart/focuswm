//! Headless UI interaction tests driven by Slint's testing backend.
//!
//! These exercise the real generated UI (no mocking of the toolkit): they
//! simulate clicks on actual elements and assert on the resulting state. The
//! testing backend can only be initialised once per process, so each scenario
//! lives in its own integration-test file with a single `#[test]`.

use i_slint_backend_testing as testing;
use i_slint_core::items::PointerEventButton;
use slint::ComponentHandle;

slint::include_modules!();

#[test]
fn clicking_add_opens_wizard_and_cancel_closes_it() {
    testing::init_no_event_loop();

    let ui = Desktop::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(1280, 800));

    // The wizard starts closed.
    assert!(!ui.get_wizard_open());

    // Click the sidebar "+" button (a TouchArea) -> the wizard opens.
    let add = testing::ElementHandle::find_by_element_id(&ui, "Sidebar::add")
        .next()
        .expect("the + button should exist");
    add.mock_single_click(PointerEventButton::Left);
    assert!(ui.get_wizard_open(), "clicking + should open the creation wizard");

    // Click the wizard's "Cancel" button -> the wizard closes.
    let cancel = testing::ElementHandle::find_by_accessible_label(&ui, "Cancel")
        .next()
        .expect("the Cancel button should exist");
    cancel.invoke_accessible_default_action();
    assert!(!ui.get_wizard_open(), "Cancel should close the wizard");
}
