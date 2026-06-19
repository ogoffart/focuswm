//! Headless test of the wizard's form validation (pure UI logic, no host
//! callbacks): the Create button is disabled until a task name is entered.

use i_slint_backend_testing as testing;
use i_slint_core::items::PointerEventButton;
use slint::ComponentHandle;

slint::include_modules!();

#[test]
fn create_button_enables_once_name_is_entered() {
    testing::init_no_event_loop();

    let ui = Desktop::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(1280, 800));

    // Open the wizard.
    testing::ElementHandle::find_by_element_id(&ui, "Sidebar::add")
        .next()
        .expect("the + button should exist")
        .mock_single_click(PointerEventButton::Left);
    assert!(ui.get_wizard_open());

    // With an empty name, Create is disabled.
    let create = || {
        testing::ElementHandle::find_by_accessible_label(&ui, "Create")
            .next()
            .expect("the Create button should exist")
    };
    assert_eq!(create().accessible_enabled(), Some(false));

    // Enter a name; Create becomes enabled.
    testing::ElementHandle::find_by_element_id(&ui, "Wizard::name-edit")
        .next()
        .expect("the name field should exist")
        .set_accessible_value("Fix login bug");
    assert_eq!(
        create().accessible_enabled(),
        Some(true),
        "Create should enable once a name is entered"
    );
}
