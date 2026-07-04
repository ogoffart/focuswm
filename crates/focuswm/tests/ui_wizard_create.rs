//! Headless UI test: filling the wizard and clicking Create forwards the
//! entered fields (name, category, branch) to the host's `create-task`.

use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing as testing;
use i_slint_core::items::PointerEventButton;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

slint::include_modules!();

#[test]
fn wizard_create_forwards_fields_to_host() {
    testing::init_no_event_loop();

    let ui = Desktop::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(1280, 800));
    ui.global::<AppData>().set_categories(ModelRc::from(Rc::new(VecModel::from(
        ["work", "personal"]
            .iter()
            .map(|c| SharedString::from(*c))
            .collect::<Vec<_>>(),
    ))));

    let got = Rc::new(RefCell::new(None));
    {
        let got = got.clone();
        ui.global::<Logic>().on_create_task(move |name, cat, branch, repo| {
            *got.borrow_mut() =
                Some((name.to_string(), cat.to_string(), branch.to_string(), repo.to_string()));
        });
    }

    // Open the wizard.
    testing::ElementHandle::find_by_element_id(&ui, "Sidebar::add")
        .next()
        .expect("+ button")
        .mock_single_click(PointerEventButton::Left);

    // Fill name + branch (category defaults to the first entry).
    testing::ElementHandle::find_by_element_id(&ui, "Wizard::name-edit")
        .next()
        .expect("name field")
        .set_accessible_value("Fix login bug");
    testing::ElementHandle::find_by_element_id(&ui, "Wizard::branch-edit")
        .next()
        .expect("branch field")
        .set_accessible_value("feature/login");

    // Create.
    testing::ElementHandle::find_by_accessible_label(&ui, "Create")
        .next()
        .expect("Create button")
        .invoke_accessible_default_action();

    let got = got.borrow();
    let (name, cat, branch, _repo) = got.as_ref().expect("create-task should fire");
    assert_eq!(name, "Fix login bug");
    assert_eq!(cat, "work", "the default category is the first in the list");
    assert_eq!(branch, "feature/login");
}
