//! Account-home worker decisions: the verifier, revealed-href
//! validation, username extraction, same-site comparison, and miss
//! diagnostics. Everything here is pure — the browser stays outside;
//! the live page is exercised by the opt-in native run, not by tests.
use browser_driver::AxElement;
use macro_engine::{
    PageGoalOutcome, VerbKind, VerbSpec, chrome_action_miss_diagnostic, label_names_profile,
    path_names_account, same_site_host, tried_label, username_from_href, username_from_menu_text,
    validate_revealed_href, verify_account_landing,
};
use url::Url;

fn origin() -> Url {
    Url::parse("https://www.example.com/").expect("test origin parses")
}

fn url(s: &str) -> Url {
    Url::parse(s).expect("test URL parses")
}

// ---- verify_account_landing ----

#[test]
fn verifier_accepts_page_revealed_username_in_path() {
    let current = url("https://www.example.com/user/someuser/");
    let outcome = verify_account_landing(&current, &origin(), "u/someuser", Some("someuser"))
        .expect("verified landing");
    match outcome {
        PageGoalOutcome::Verified {
            label,
            landed,
            username,
        } => {
            assert_eq!(label, "u/someuser");
            assert_eq!(landed, current);
            assert_eq!(username.as_deref(), Some("someuser"));
        }
        other => panic!("expected Verified, got {other:?}"),
    }
}

#[test]
fn verifier_rejects_wrong_username_in_path() {
    let current = url("https://www.example.com/user/other/");
    let err =
        verify_account_landing(&current, &origin(), "u/someuser", Some("someuser")).unwrap_err();
    assert!(err.contains("account-home landing failed verification"));
}

#[test]
fn verifier_rejects_root_landing_even_with_username() {
    let current = url("https://www.example.com/");
    assert!(verify_account_landing(&current, &origin(), "u/someuser", Some("someuser")).is_err());
}

#[test]
fn verifier_rejects_cross_site_landing() {
    let current = url("https://evil.example.net/user/someuser/");
    assert!(verify_account_landing(&current, &origin(), "u/someuser", Some("someuser")).is_err());
}

#[test]
fn verifier_accepts_www_mismatch_as_same_site() {
    let bare = url("https://example.com/");
    let current = url("https://www.example.com/user/someuser/");
    assert!(verify_account_landing(&current, &bare, "u/someuser", Some("someuser")).is_ok());
}

#[test]
fn verifier_accepts_account_path_without_username() {
    // No username evidence (avatar click with no handle in the menu):
    // a same-site, non-root account-worded path is enough.
    let current = url("https://www.example.com/settings");
    assert!(verify_account_landing(&current, &origin(), "Open user menu", None).is_ok());
}

#[test]
fn verifier_accepts_profile_worded_trigger_without_username() {
    let current = url("https://www.example.com/xyz");
    assert!(verify_account_landing(&current, &origin(), "Profile", None).is_ok());
}

#[test]
fn verifier_rejects_arbitrary_navigation_without_evidence() {
    // Same-site and non-root, but nothing ties the landing to the
    // account: this must fail, never silently pass.
    let current = url("https://www.example.com/discover");
    let err = verify_account_landing(&current, &origin(), "Open menu", None).unwrap_err();
    assert!(err.contains("account-home landing failed verification"));
}

// ---- validate_revealed_href ----

#[test]
fn revealed_href_accepts_relative_same_site_destination() {
    let href =
        validate_revealed_href("/user/someuser/", &origin()).expect("revealed href accepted");
    assert_eq!(href.host_str(), Some("www.example.com"));
    assert_eq!(href.path(), "/user/someuser/");
    assert_eq!(href.scheme(), "https");
}

#[test]
fn revealed_href_accepts_absolute_same_site_destination() {
    let href = validate_revealed_href("https://www.example.com/user/someuser", &origin())
        .expect("revealed href accepted");
    assert_eq!(href.path(), "/user/someuser");
}

#[test]
fn revealed_href_accepts_www_mismatch() {
    let bare = url("https://example.com/");
    assert!(validate_revealed_href("https://www.example.com/user/x", &bare).is_some());
}

#[test]
fn revealed_href_rejects_cross_origin() {
    assert!(validate_revealed_href("https://evil.example.net/user/x", &origin()).is_none());
}

#[test]
fn revealed_href_rejects_embedded_credentials() {
    assert!(
        validate_revealed_href("https://user:pass@www.example.com/user/x", &origin()).is_none()
    );
}

#[test]
fn revealed_href_rejects_non_https() {
    assert!(validate_revealed_href("http://www.example.com/user/x", &origin()).is_none());
}

#[test]
fn revealed_href_rejects_root_destination() {
    assert!(validate_revealed_href("/", &origin()).is_none());
    assert!(validate_revealed_href("https://www.example.com/", &origin()).is_none());
}

#[test]
fn revealed_href_rejects_non_navigable_schemes() {
    assert!(validate_revealed_href("javascript:alert(1)", &origin()).is_none());
    assert!(validate_revealed_href("", &origin()).is_none());
    assert!(validate_revealed_href("   ", &origin()).is_none());
}

// ---- username extraction ----

#[test]
fn username_from_href_takes_last_path_segment() {
    let href = url("https://www.example.com/user/someuser/");
    assert_eq!(username_from_href(&href).as_deref(), Some("someuser"));
    let bare = url("https://www.example.com/settings");
    assert_eq!(username_from_href(&bare).as_deref(), Some("settings"));
}

#[test]
fn username_from_menu_text_reads_handle_shapes() {
    assert_eq!(
        username_from_menu_text("u/someuser").as_deref(),
        Some("someuser")
    );
    assert_eq!(
        username_from_menu_text("@someuser").as_deref(),
        Some("someuser")
    );
}

#[test]
fn username_from_menu_text_rejects_sentences_and_plain_words() {
    assert_eq!(username_from_menu_text("u/some user"), None);
    assert_eq!(username_from_menu_text("Profile"), None);
    assert_eq!(username_from_menu_text("u/"), None);
    assert_eq!(username_from_menu_text(""), None);
}

// ---- same-site comparison ----

#[test]
fn same_site_host_ignores_www_and_case() {
    assert!(same_site_host("www.example.com", "example.com"));
    assert!(same_site_host("example.com", "www.example.com"));
    assert!(same_site_host("WWW.EXAMPLE.COM", "example.com"));
    assert!(!same_site_host("other.example.com", "example.com"));
    assert!(!same_site_host("example.com", "evil.example.net"));
}

// ---- account evidence vocabulary ----

#[test]
fn path_names_account_matches_whole_segments() {
    assert!(path_names_account("/user/someuser/"));
    assert!(path_names_account("/settings"));
    assert!(path_names_account("/me"));
    assert!(!path_names_account("/discover"));
    // Substring is not enough: `/users` is not the account area.
    assert!(!path_names_account("/users"));
}

#[test]
fn label_names_profile_matches_profile_words() {
    assert!(label_names_profile("Profile"));
    assert!(label_names_profile("My account"));
    assert!(!label_names_profile("Open user menu"));
    assert!(!label_names_profile(""));
}

// ---- miss diagnostics ----

fn element(role: &str, name: &str) -> AxElement {
    AxElement {
        backend_node_id: 7,
        role: role.to_owned(),
        name: name.to_owned(),
        description: String::new(),
        container_text: Vec::new(),
        landmark: None,
    }
}

#[test]
fn tried_label_names_role_name_and_effect() {
    let line = tried_label(&element("button", "Open user menu"), "menu opened");
    assert_eq!(line, "button 'Open user menu' → menu opened");
}

#[test]
fn tried_label_truncates_long_names() {
    let long = "x".repeat(100);
    let line = tried_label(&element("button", &long), "no new controls");
    assert!(line.len() < 100);
    assert!(line.ends_with("→ no new controls"));
}

#[test]
fn miss_diagnostic_reports_no_control_found() {
    let diagnostic = chrome_action_miss_diagnostic(VerbSpec::for_kind(VerbKind::AccountHome), &[]);
    assert!(diagnostic.starts_with("account_home:"));
    assert!(diagnostic.contains("no identity control revealed"));
}

#[test]
fn miss_diagnostic_lists_tried_clicks() {
    let diagnostic = chrome_action_miss_diagnostic(
        VerbSpec::for_kind(VerbKind::AccountHome),
        &[
            "button 'Open user menu' → menu opened".to_owned(),
            "button 'Avatar' → no new controls".to_owned(),
        ],
    );
    assert!(diagnostic.contains("Tried:"));
    assert!(diagnostic.contains("menu opened"));
    assert!(diagnostic.contains("no new controls"));
}

// ---- revealed-profile freshness ----

fn revealed_element(id: i64, role: &str, name: &str, container: &[&str]) -> AxElement {
    AxElement {
        backend_node_id: id,
        role: role.to_owned(),
        name: name.to_owned(),
        description: String::new(),
        container_text: container.iter().map(|s| (*s).to_owned()).collect(),
        landmark: None,
    }
}

/// Regression: the container-text rollup shares an opened menu's "Profile"
/// wording with the header buttons that were already on the page. A
/// revealed destination must be genuinely new since the previous
/// snapshot, or the worker clicks the header chrome itself and burns its
/// click budget (caught by the live-browser proof).
#[test]
fn revealed_profile_ignores_stale_header_buttons() {
    use macro_engine::{ClickedControl, VerbKind, VerbSpec, select_revealed_action};
    use std::collections::HashSet;
    let spec = VerbSpec::for_kind(VerbKind::AccountHome);
    // Document order: header buttons first, the menu link last — like a
    // real opened menu.
    let elements = vec![
        revealed_element(1, "button", "Search", &["Profile"]),
        revealed_element(2, "button", "kx7", &["Profile"]),
        revealed_element(3, "link", "Profile", &[]),
    ];
    // The kx7 button was already tried (stable identity, not node id).
    let clicked = vec![ClickedControl::of(&elements[1])];
    // The header buttons were in the previous snapshot: only the menu
    // link is genuinely revealed.
    let seen: HashSet<i64> = [1, 2].into_iter().collect();
    let picked =
        select_revealed_action(&elements, &clicked, &seen, spec).expect("revealed link picked");
    assert_eq!(picked.backend_node_id, 3);
}

#[test]
fn revealed_profile_without_freshness_would_pick_stale_chrome() {
    use macro_engine::{ClickedControl, VerbKind, VerbSpec, select_revealed_action};
    use std::collections::HashSet;
    let spec = VerbSpec::for_kind(VerbKind::AccountHome);
    // Pins the failure mode the freshness set exists to prevent: without
    // it, the first header button wins by document order. The tried
    // control matches nothing on the page, so exclusion changes nothing.
    let elements = vec![
        revealed_element(1, "button", "Search", &["Profile"]),
        revealed_element(3, "link", "Profile", &[]),
    ];
    let clicked = vec![ClickedControl::of(&revealed_element(
        9,
        "button",
        "already tried elsewhere",
        &[],
    ))];
    let picked = select_revealed_action(&elements, &clicked, &HashSet::new(), spec)
        .expect("something picked");
    assert_eq!(picked.backend_node_id, 1);
}

#[test]
fn revealed_profile_stays_gated_on_worker_opened_something() {
    use macro_engine::{VerbKind, VerbSpec, select_revealed_action};
    use std::collections::HashSet;
    let elements = vec![revealed_element(3, "link", "u/someone", &[])];
    assert!(
        select_revealed_action(
            &elements,
            &[],
            &HashSet::new(),
            VerbSpec::for_kind(VerbKind::AccountHome)
        )
        .is_none()
    );
}

/// Regression (live Reddit, 2026-09-25): the worker clicked "Open user
/// actions" three times because retry exclusion keyed on
/// `backend_node_id`, which churns when the page re-renders between
/// snapshots. Exclusion is by stable role+name identity, so a
/// re-rendered control is still recognized as already tried.
#[test]
fn clicked_identity_survives_node_id_churn() {
    use macro_engine::{ClickedControl, select_menu_button};
    let first = revealed_element(7, "button", "Open user actions", &[]);
    // Same logical button, new backend node id after a re-render.
    let rerendered = revealed_element(42, "button", "Open user actions", &[]);
    let other = revealed_element(43, "button", "Search", &[]);
    let clicked = vec![ClickedControl::of(&first)];
    let elements = vec![rerendered, other];
    // The re-rendered button is recognized as already tried despite its
    // new node id, so the worker advances instead of re-clicking it.
    // ("Search" matches no account word, so nothing is left to offer.)
    assert!(select_menu_button(&elements, &clicked).is_none());
    // And an untouched control is still offered.
    assert!(select_menu_button(&elements, &[]).is_some());
}
