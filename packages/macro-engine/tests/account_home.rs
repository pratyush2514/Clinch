//! Account-home worker decisions: the verifier, revealed-href
//! validation, username extraction, same-site comparison, and miss
//! diagnostics. Everything here is pure — the browser stays outside;
//! the live page is exercised by the opt-in native run, not by tests.
use browser_driver::AxElement;
use macro_engine::{
    PageGoalOutcome, identity_miss_diagnostic, label_names_profile, path_names_account,
    same_site_host, tried_label, username_from_href, username_from_menu_text,
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
    let diagnostic = identity_miss_diagnostic(&[]);
    assert!(diagnostic.starts_with("account-home:"));
    assert!(diagnostic.contains("no identity control found"));
}

#[test]
fn miss_diagnostic_lists_tried_clicks() {
    let diagnostic = identity_miss_diagnostic(&[
        "button 'Open user menu' → menu opened".to_owned(),
        "button 'Avatar' → no new controls".to_owned(),
    ]);
    assert!(diagnostic.contains("Tried:"));
    assert!(diagnostic.contains("menu opened"));
    assert!(diagnostic.contains("no new controls"));
}
