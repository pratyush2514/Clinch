#![deny(unsafe_code)]
//! In-page goal selection: pure, browser-free.
//!
//! `select_page_control` and `select_menu_button` choose from a fabricated
//! AX snapshot the same way the live pursuit loop does. The multi-step
//! loop itself (`pursue_page_goal`) needs a real browser and is covered by
//! native validation, not here.

use browser_driver::AxElement;
use macro_engine::{page_goal_diagnostic, select_menu_button, select_page_control};

fn el(id: i64, role: &str, name: &str, landmark: Option<&str>) -> AxElement {
    AxElement {
        backend_node_id: id,
        role: role.to_string(),
        name: name.to_string(),
        description: String::new(),
        container_text: Vec::new(),
        landmark: landmark.map(str::to_string),
    }
}

#[test]
fn select_page_control_matches_noun() {
    let elements = vec![
        el(1, "link", "Home", None),
        el(2, "button", "View profile", None),
        el(3, "link", "Profile settings", None),
    ];
    // First actionable control mentioning the noun, in document order.
    let picked = select_page_control(&elements, "profile", &[]).expect("match");
    assert_eq!(picked.backend_node_id, 2);
    // Buttons and menu items count, not just links.
    let picked = select_page_control(&elements, "settings", &[]).expect("match");
    assert_eq!(picked.backend_node_id, 3);
    // No mention: no pick, never a guess.
    assert!(select_page_control(&elements, "billing", &[]).is_none());
    // Empty noun: no pick.
    assert!(select_page_control(&elements, "  ", &[]).is_none());
}

#[test]
fn select_page_control_skips_clicked() {
    let elements = vec![
        el(1, "button", "View profile", None),
        el(2, "link", "Edit profile", None),
    ];
    let picked = select_page_control(&elements, "profile", &[1]).expect("match");
    assert_eq!(picked.backend_node_id, 2);
    assert!(select_page_control(&elements, "profile", &[1, 2]).is_none());
}

#[test]
fn select_menu_button_prefers_header() {
    let elements = vec![
        el(1, "button", "Submit", None),
        el(2, "button", "Open user menu", Some("banner")),
        el(3, "button", "Sections", Some("navigation")),
    ];
    // Banner button first in document order among header buttons.
    let picked = select_menu_button(&elements, &[]).expect("menu");
    assert_eq!(picked.backend_node_id, 2);
    // Already-clicked menus are not re-offered.
    let picked = select_menu_button(&elements, &[2]).expect("menu");
    assert_eq!(picked.backend_node_id, 3);
    // No header button left: none.
    assert!(select_menu_button(&elements, &[2, 3]).is_none());
    // Buttons outside page chrome are not menus.
    assert!(select_menu_button(&[el(1, "button", "Submit", None)], &[]).is_none());
}

#[test]
fn page_goal_diagnostic_renders_evidence() {
    let elements = vec![
        el(1, "link", "Home", None),
        el(2, "button", "Log in", None),
    ];
    let diagnostic = page_goal_diagnostic(&elements, "profile");
    assert!(diagnostic.contains("profile"), "{diagnostic}");
    assert!(diagnostic.contains("Home"), "{diagnostic}");
    assert!(diagnostic.contains("Log in"), "{diagnostic}");
}
