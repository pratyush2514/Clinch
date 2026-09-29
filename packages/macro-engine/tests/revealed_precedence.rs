//! Revealed-destination precedence: a control that itself names the verb's
//! vocabulary beats one that only matches through the menu's shared container
//! text, even when the container-only control sorts earlier; container-only
//! matching stays as the fail-open second pass.

use browser_driver::AxElement;
use macro_engine::{
    ClickedControl, MatchTier, VerbKind, VerbSpec, select_already_open_menu_target,
    select_already_open_menu_target_tiered, select_revealed_action, select_revealed_action_tiered,
};
use std::collections::HashSet;

const MENU_TEXT: &str = "View Profile Settings Log Out";

fn el(id: i64, role: &str, name: &str, container: &[&str]) -> AxElement {
    AxElement {
        backend_node_id: id,
        role: role.to_string(),
        name: name.to_string(),
        description: String::new(),
        container_text: container.iter().map(|text| (*text).to_string()).collect(),
        landmark: None,
    }
}

fn log_out_spec() -> &'static VerbSpec {
    VerbSpec::for_kind(VerbKind::LogOut)
}

fn opener_clicked() -> Vec<ClickedControl> {
    vec![ClickedControl::of(&el(
        1,
        "button",
        "Open user actions",
        &[],
    ))]
}

#[test]
fn revealed_direct_name_beats_earlier_container_only() {
    let elements = vec![
        el(10, "link", "View Profile", &[MENU_TEXT]),
        el(11, "menuitem", "Log Out", &[MENU_TEXT]),
    ];
    let seen = HashSet::new();
    let picked = select_revealed_action(&elements, &opener_clicked(), &seen, log_out_spec())
        .expect("a control is picked");
    assert_eq!(picked.backend_node_id, 11);
    let (_, tier) =
        select_revealed_action_tiered(&elements, &opener_clicked(), &seen, log_out_spec())
            .expect("tiered pick");
    assert_eq!(tier, MatchTier::Direct);
}

#[test]
fn revealed_container_only_still_matches_when_no_direct_match() {
    let elements = vec![
        el(10, "link", "View Profile", &[MENU_TEXT]),
        el(11, "menuitem", "Settings", &[MENU_TEXT]),
    ];
    let seen = HashSet::new();
    let (picked, tier) =
        select_revealed_action_tiered(&elements, &opener_clicked(), &seen, log_out_spec())
            .expect("fail-open container pick");
    assert_eq!(picked.backend_node_id, 10);
    assert_eq!(tier, MatchTier::Container);
}

#[test]
fn open_menu_direct_name_beats_earlier_container_only() {
    let elements = vec![
        el(5, "menu", "", &[]),
        el(10, "menuitem", "View Profile", &[MENU_TEXT]),
        el(11, "menuitem", "Log Out", &[MENU_TEXT]),
    ];
    let picked =
        select_already_open_menu_target(&elements, log_out_spec()).expect("a control is picked");
    assert_eq!(picked.backend_node_id, 11);
    let (_, tier) =
        select_already_open_menu_target_tiered(&elements, log_out_spec()).expect("tiered pick");
    assert_eq!(tier, MatchTier::Direct);
}

#[test]
fn open_menu_container_only_still_matches_when_no_direct_match() {
    let elements = vec![
        el(5, "menu", "", &[]),
        el(10, "menuitem", "View Profile", &[MENU_TEXT]),
        el(11, "menuitem", "Settings", &[MENU_TEXT]),
    ];
    let (picked, tier) = select_already_open_menu_target_tiered(&elements, log_out_spec())
        .expect("fail-open container pick");
    assert_eq!(picked.backend_node_id, 10);
    assert_eq!(tier, MatchTier::Container);
}
