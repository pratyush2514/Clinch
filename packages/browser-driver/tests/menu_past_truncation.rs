//! A revealed menu renders at the end of the document (React portal), past
//! the 300-element head truncation the navigator slice keeps. This test
//! pins the behavior that caught the live Reddit miss: the truncated
//! `interactive_elements` view reports "no new controls" for a menu that
//! plainly opened (`actionable 300 → 300`), while `interactive_elements_all`
//! — the account-home worker's post-click re-snapshot — sees it.

use browser_driver::{AxNode, interactive_elements, interactive_elements_all};

fn ax_value(text: &str) -> browser_driver::AxValue {
    let mut value = browser_driver::AxValue::new(browser_driver::AxValueType::String);
    value.value = Some(serde_json::Value::String(text.to_owned()));
    value
}

fn node(id: &str, role: &str, name: &str, backend: i64) -> AxNode {
    let mut node = AxNode::new(id.to_owned(), false);
    node.role = Some(ax_value(role));
    node.name = Some(ax_value(name));
    node.backend_dom_node_id = Some(browser_driver::BackendNodeId::new(backend));
    node
}

fn backend_ids(elements: &[browser_driver::AxElement]) -> Vec<i64> {
    elements.iter().map(|el| el.backend_node_id).collect()
}

/// A full page: 300 head controls plus a 3-item user menu appended at
/// document end, exactly where a React portal puts it.
fn page_with_menu_at_end() -> Vec<AxNode> {
    let mut nodes: Vec<AxNode> = (0..300)
        .map(|i| node(&format!("head-{i}"), "button", "head", 1000 + i))
        .collect();
    nodes.push(node("menu-1", "menuitem", "Profile", 2001));
    nodes.push(node("menu-2", "menuitem", "Settings", 2002));
    nodes.push(node("menu-3", "menuitem", "Log out", 2003));
    nodes
}

#[test]
fn truncated_slice_is_blind_to_a_menu_past_the_head() {
    let nodes = page_with_menu_at_end();
    let elements = interactive_elements(&nodes);
    assert_eq!(elements.len(), 300);
    // The menu items never appear in the truncated view.
    assert!(!backend_ids(&elements).contains(&2001));
}

#[test]
fn full_snapshot_sees_the_revealed_menu() {
    let nodes = page_with_menu_at_end();
    let elements = interactive_elements_all(&nodes);
    assert_eq!(elements.len(), 303);
    let Some(profile) = elements.iter().find(|el| el.backend_node_id == 2001) else {
        panic!("menu item past the truncation head must be visible");
    };
    assert_eq!(profile.role, "menuitem");
    assert_eq!(profile.name, "Profile");
}

#[test]
fn full_snapshot_keeps_document_order() {
    let nodes = page_with_menu_at_end();
    let elements = interactive_elements_all(&nodes);
    assert_eq!(elements[0].backend_node_id, 1000);
    assert_eq!(elements[299].backend_node_id, 1299);
    assert_eq!(elements[300].backend_node_id, 2001);
}
