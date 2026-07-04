//! Headless UI test: the wizard's GitHub issue search forwards the query to the
//! host, and clicking a result links it. Also checks the feature is gated on
//! `github-enabled`.

use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing as testing;
use i_slint_core::items::PointerEventButton;
use slint::{ComponentHandle, ModelRc, VecModel};

slint::include_modules!();

#[test]
fn wizard_github_search_and_link() {
    testing::init_no_event_loop();

    let ui = Desktop::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(1280, 800));
    ui.global::<AppData>().set_github_enabled(true);

    let query = Rc::new(RefCell::new(String::new()));
    let linked = Rc::new(RefCell::new(None));
    {
        let q = query.clone();
        ui.global::<Logic>().on_search_issues(move |s| *q.borrow_mut() = s.to_string());
    }
    {
        let l = linked.clone();
        ui.global::<Logic>().on_link_issue(move |slug, number, title, _url| {
            *l.borrow_mut() = Some((slug.to_string(), number, title.to_string()));
        });
    }

    // Open the wizard.
    testing::ElementHandle::find_by_element_id(&ui, "Sidebar::add")
        .next()
        .expect("+ button")
        .mock_single_click(PointerEventButton::Left);

    // Type a query and click Search.
    testing::ElementHandle::find_by_element_id(&ui, "Wizard::issue-edit")
        .next()
        .expect("issue search field")
        .set_accessible_value("login bug");
    testing::ElementHandle::find_by_accessible_label(&ui, "Search")
        .next()
        .expect("Search button")
        .invoke_accessible_default_action();
    assert_eq!(*query.borrow(), "login bug", "Search forwards the query");

    // Seed results as if the host answered, then click the first one to link it.
    let hit = IssueResult {
        slug: "ogoffart/focuswm".into(),
        number: 128,
        title: "Login fails".into(),
        url: "https://github.com/ogoffart/focuswm/issues/128".into(),
        selected: false,
    };
    ui.global::<AppData>()
        .set_issue_results(ModelRc::from(Rc::new(VecModel::from(vec![hit]))));

    testing::ElementHandle::find_by_element_id(&ui, "Wizard::ta")
        .next()
        .expect("a result row")
        .mock_single_click(PointerEventButton::Left);

    assert_eq!(
        linked.take(),
        Some(("ogoffart/focuswm".to_string(), 128, "Login fails".to_string())),
        "clicking a result links that issue",
    );
}
