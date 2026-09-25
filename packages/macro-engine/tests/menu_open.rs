//! Shared menu-opening primitive: pure candidate ranking.
//!
//! `rank_menu_candidates` orders (a) landmarked banner/navigation
//! buttons, (b) account-worded buttons, (c) blank-named buttons inside
//! the header strip (rightmost first), (d) remaining in-strip buttons
//! (rightmost first). The click/verify loop itself needs a live browser
//! and is covered by native validation, like the other pursuit loops —
//! no Chromium is available in this environment, so there is no live
//! integration test here.

use browser_driver::AxElement;
use macro_engine::{ClickedControl, rank_menu_candidates};

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

fn ids(ranked: &[&AxElement]) -> Vec<i64> {
    ranked.iter().map(|el| el.backend_node_id).collect()
}

#[test]
fn tier_order_landmarked_then_account_worded_then_blank_then_named() {
    let elements = vec![
        el(1, "button", "Open user actions", None),
        el(2, "button", "Sections", Some("navigation")),
        el(3, "button", "", None),
        el(4, "button", "Chat", None),
        el(5, "link", "User profile", None),
    ];
    let rects = vec![(3, 100.0, 50.0), (4, 900.0, 60.0)];
    let ranked = rank_menu_candidates(&elements, &[], &rects, Some(200.0));
    // (a) landmarked, (b) account-worded, (c) blank in strip,
    // (d) named in strip. The account-worded link is never a candidate.
    assert_eq!(ids(&ranked), vec![2, 1, 3, 4]);
}

#[test]
fn already_clicked_excluded_in_every_tier() {
    let elements = vec![
        el(1, "button", "Open user actions", None),
        el(2, "button", "Sections", Some("navigation")),
        el(3, "button", "", None),
        el(4, "button", "Chat", None),
    ];
    let rects = vec![(3, 100.0, 50.0), (4, 900.0, 60.0)];
    // Clicked landmarked + blank: both vanish, the rest keep their order.
    let clicked = vec![
        ClickedControl::of(&elements[1]),
        ClickedControl::of(&elements[2]),
    ];
    let ranked = rank_menu_candidates(&elements, &clicked, &rects, Some(200.0));
    assert_eq!(ids(&ranked), vec![1, 4]);
    // Everything clicked: nothing left to try.
    let clicked_all: Vec<ClickedControl> = elements.iter().map(ClickedControl::of).collect();
    let ranked = rank_menu_candidates(&elements, &clicked_all, &rects, Some(200.0));
    assert!(ranked.is_empty());
}

#[test]
fn below_strip_excluded_from_geometry_tiers() {
    let elements = vec![
        el(1, "button", "Open user actions", None),
        el(3, "button", "", None),
        el(4, "button", "Chat", None),
    ];
    // Blank button below the strip: not tier (c), and not tier (d) either.
    let rects = vec![(3, 100.0, 500.0), (4, 900.0, 60.0)];
    let ranked = rank_menu_candidates(&elements, &[], &rects, Some(200.0));
    assert_eq!(ids(&ranked), vec![1, 4]);
}

#[test]
fn unreadable_viewport_disables_geometry_tiers() {
    let elements = vec![
        el(1, "button", "Open user actions", None),
        el(2, "button", "Sections", Some("navigation")),
        el(3, "button", "", None),
        el(4, "button", "Chat", None),
    ];
    let rects = vec![(3, 100.0, 50.0), (4, 900.0, 60.0)];
    // No viewport: tiers (c) and (d) stay empty, (a) and (b) still rank.
    let ranked = rank_menu_candidates(&elements, &[], &rects, None);
    assert_eq!(ids(&ranked), vec![2, 1]);
}

#[test]
fn control_appears_once_at_highest_tier() {
    let elements = vec![
        el(1, "button", "User menu", Some("banner")),
        el(2, "button", "Open user actions", None),
    ];
    let ranked = rank_menu_candidates(&elements, &[], &[], Some(200.0));
    assert_eq!(ids(&ranked), vec![1, 2]);
}

#[test]
fn non_buttons_never_ranked() {
    let elements = vec![el(1, "link", "User menu", None), el(2, "link", "", None)];
    let rects = vec![(1, 100.0, 50.0), (2, 200.0, 50.0)];
    assert!(rank_menu_candidates(&elements, &[], &rects, Some(200.0)).is_empty());
}

#[test]
fn non_header_landmark_button_needs_geometry() {
    // A footer-landmark button is not tier (a): without an in-strip rect
    // it ranks nowhere. With one it is a tier-(d) candidate — the
    // geometry fallback is landmark-agnostic, like select_rightmost_button.
    let elements = vec![el(1, "button", "Back to top", Some("contentinfo"))];
    assert!(rank_menu_candidates(&elements, &[], &[], Some(200.0)).is_empty());
    let ranked = rank_menu_candidates(&elements, &[], &[(1, 100.0, 50.0)], Some(200.0));
    assert_eq!(ids(&ranked), vec![1]);
}

#[test]
fn blank_tier_prefers_rightmost() {
    let elements = vec![el(1, "button", "", None), el(2, "button", "", None)];
    let rects = vec![(1, 100.0, 50.0), (2, 800.0, 50.0)];
    let ranked = rank_menu_candidates(&elements, &[], &rects, Some(200.0));
    assert_eq!(ids(&ranked), vec![2, 1]);
}

#[test]
fn whitespace_only_name_counts_as_blank() {
    let elements = vec![el(1, "button", "   ", None), el(2, "button", "Chat", None)];
    let rects = vec![(1, 100.0, 50.0), (2, 900.0, 50.0)];
    // Whitespace-named button is tier (c): ahead of the named tier-(d) button.
    let ranked = rank_menu_candidates(&elements, &[], &rects, Some(200.0));
    assert_eq!(ids(&ranked), vec![1, 2]);
}

#[test]
fn empty_snapshot_ranks_nothing() {
    let ranked = rank_menu_candidates(&[], &[], &[(1, 100.0, 50.0)], Some(200.0));
    assert!(ranked.is_empty());
}
