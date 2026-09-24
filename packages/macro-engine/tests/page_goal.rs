#![deny(unsafe_code)]
//! In-page goal selection: pure, browser-free.
//!
//! `select_page_control` and `select_menu_button` choose from a fabricated
//! AX snapshot the same way the live pursuit loop does. The multi-step
//! loop itself (`pursue_page_goal`) needs a real browser and is covered by
//! native validation, not here.

use browser_driver::AxElement;
use macro_engine::{
    page_goal_diagnostic, pick_topmost, select_menu_button, select_page_control, zone_for,
};

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

fn el_desc(id: i64, role: &str, name: &str, description: &str) -> AxElement {
    AxElement {
        backend_node_id: id,
        role: role.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        container_text: Vec::new(),
        landmark: None,
    }
}

#[test]
fn select_menu_button_falls_back_to_account_words() {
    let elements = vec![
        el(1, "button", "Submit", None),
        // No landmark anywhere: the avatar-style button named after the
        // username still opens the account surface.
        el(2, "button", "u_someuser", None),
        el(3, "button", "Search", None),
    ];
    let picked = select_menu_button(&elements, &[]).expect("menu");
    assert_eq!(picked.backend_node_id, 2);
    // Already-clicked account buttons are not re-offered.
    assert!(select_menu_button(&elements, &[2]).is_none());
}

#[test]
fn select_menu_button_prefers_landmark_over_account_word() {
    let elements = vec![
        el(1, "button", "User settings", None),
        el(2, "button", "Sections", Some("navigation")),
    ];
    // Landmark pass still wins over the word pass.
    let picked = select_menu_button(&elements, &[]).expect("menu");
    assert_eq!(picked.backend_node_id, 2);
}

#[test]
fn select_menu_button_matches_account_word_in_description() {
    let elements = vec![el_desc(1, "button", "", "Open user menu")];
    let picked = select_menu_button(&elements, &[]).expect("menu");
    assert_eq!(picked.backend_node_id, 1);
}

#[test]
fn select_page_control_expands_profile_to_account() {
    let elements = vec![
        el(1, "link", "Home", None),
        el(2, "link", "Account settings", None),
    ];
    // "profile" matches the account surface: same user page, different word.
    let picked = select_page_control(&elements, "profile", &[]).expect("match");
    assert_eq!(picked.backend_node_id, 2);
    // Symmetric: "account" matches "profile".
    let elements = vec![el(1, "link", "View profile", None)];
    let picked = select_page_control(&elements, "account", &[]).expect("match");
    assert_eq!(picked.backend_node_id, 1);
    // Unrelated nouns are untouched by the expansion.
    assert!(select_page_control(&elements, "billing", &[]).is_none());
}

#[test]
fn pick_topmost_selects_highest_button_in_strip() {
    // (backend_node_id, y): smallest y wins when inside the strip.
    assert_eq!(pick_topmost(&[(1, 40.0), (2, 12.0), (3, 80.0)], 200.0), Some(2));
    // Below the strip is page content, never a header menu.
    assert_eq!(pick_topmost(&[(1, 300.0), (2, 500.0)], 200.0), None);
    // Empty candidates: none.
    assert_eq!(pick_topmost(&[], 200.0), None);
}

#[test]
fn zone_for_maps_nine_cells() {
    use macro_engine::PositionZone;
    let bounds = (0.0, 0.0, 300.0, 300.0);
    assert_eq!(zone_for(10.0, 10.0, bounds), PositionZone::TopLeft);
    assert_eq!(zone_for(150.0, 10.0, bounds), PositionZone::TopCenter);
    assert_eq!(zone_for(290.0, 10.0, bounds), PositionZone::TopRight);
    assert_eq!(zone_for(10.0, 150.0, bounds), PositionZone::MiddleLeft);
    assert_eq!(zone_for(150.0, 150.0, bounds), PositionZone::MiddleCenter);
    assert_eq!(zone_for(290.0, 150.0, bounds), PositionZone::MiddleRight);
    assert_eq!(zone_for(10.0, 290.0, bounds), PositionZone::BottomLeft);
    assert_eq!(zone_for(150.0, 290.0, bounds), PositionZone::BottomCenter);
    assert_eq!(zone_for(290.0, 290.0, bounds), PositionZone::BottomRight);
    // Degenerate bounds (all points identical): middle, never a panic.
    assert_eq!(
        zone_for(50.0, 50.0, (50.0, 50.0, 50.0, 50.0)),
        PositionZone::MiddleCenter
    );
    // Display renders the zone the model sees in the element line.
    assert_eq!(PositionZone::TopRight.to_string(), "top-right");
}
