//! Strict notifications verifier: pure title/AX/URL cases, no browser.

use browser_driver::AxElement;
use macro_engine::{
    IdentityMenuPolicy, VerbKind, VerbSpec, VerifierKind, verify_notifications_surface,
};
use url::Url;

fn el(id: i64, role: &str, name: &str) -> AxElement {
    AxElement {
        backend_node_id: id,
        role: role.to_string(),
        name: name.to_string(),
        description: String::new(),
        container_text: Vec::new(),
        landmark: None,
    }
}

fn url(text: &str) -> Url {
    Url::parse(text).unwrap_or_else(|error| panic!("bad test url {text}: {error}"))
}

fn origin() -> Url {
    url("https://www.example.com/")
}

#[test]
fn non_root_page_with_title_vocabulary_verifies() {
    assert!(verify_notifications_surface(
        &url("https://www.example.com/inbox/all"),
        &origin(),
        Some("Your Notifications - Example"),
        &[],
    ));
}

#[test]
fn non_root_page_with_ax_heading_verifies() {
    let elements = [el(1, "heading", "Notifications")];
    assert!(verify_notifications_surface(
        &url("https://example.com/inbox"),
        &origin(),
        Some("Example"),
        &elements,
    ));
}

#[test]
fn non_root_page_with_landmark_name_verifies() {
    let elements = [el(1, "navigation", "Notification filters")];
    assert!(verify_notifications_surface(
        &url("https://example.com/inbox"),
        &origin(),
        None,
        &elements,
    ));
}

#[test]
fn root_page_with_revealed_dialog_verifies() {
    let elements = [el(1, "button", "Home"), el(2, "dialog", "Notifications")];
    assert!(verify_notifications_surface(
        &origin(),
        &origin(),
        Some("Example"),
        &elements,
    ));
}

#[test]
fn root_page_without_overlay_misses_even_with_heading() {
    // A heading on the root page is not a revealed surface.
    let elements = [el(1, "heading", "Notifications")];
    assert!(!verify_notifications_surface(
        &origin(),
        &origin(),
        Some("Example"),
        &elements,
    ));
}

#[test]
fn root_page_title_alone_misses() {
    assert!(!verify_notifications_surface(
        &origin(),
        &origin(),
        Some("Notifications"),
        &[],
    ));
}

#[test]
fn non_root_page_without_vocabulary_misses() {
    let elements = [el(1, "heading", "Account preferences")];
    assert!(!verify_notifications_surface(
        &url("https://example.com/settings"),
        &origin(),
        Some("Settings"),
        &elements,
    ));
}

#[test]
fn overlay_without_vocabulary_misses() {
    let elements = [el(1, "dialog", "Sign in")];
    assert!(!verify_notifications_surface(
        &origin(),
        &origin(),
        Some("Example"),
        &elements,
    ));
}

#[test]
fn vocabulary_on_a_non_ax_naming_role_does_not_count() {
    // A button/link named "Notifications" is a control, not a heading,
    // landmark or dialog name.
    let elements = [el(1, "button", "Notifications")];
    assert!(!verify_notifications_surface(
        &url("https://example.com/inbox"),
        &origin(),
        Some("Example"),
        &elements,
    ));
}

#[test]
fn container_text_and_description_do_not_count() {
    let mut heading = el(1, "heading", "Inbox");
    heading.description = "notifications".to_string();
    heading.container_text = vec!["notifications".to_string()];
    assert!(!verify_notifications_surface(
        &url("https://example.com/inbox"),
        &origin(),
        Some("Example"),
        &[heading],
    ));
}

#[test]
fn leaving_the_named_site_misses() {
    let elements = [el(1, "heading", "Notifications")];
    assert!(!verify_notifications_surface(
        &url("https://other-site.net/notifications"),
        &origin(),
        Some("Notifications"),
        &elements,
    ));
}

#[test]
fn notifications_row_is_in_the_verb_table() {
    let spec = VerbSpec::for_kind(VerbKind::Notifications);
    assert_eq!(spec.verifier, VerifierKind::NotificationSurface);
    assert!(spec.matches_noun("notifications"));
    assert!(spec.matches_noun("Notification"));
    assert!(!spec.matches_noun("messages"));
    assert_eq!(VerbKind::Notifications.as_str(), "notifications");
}

#[test]
fn only_log_out_runs_the_bounded_menu_policy() {
    assert_eq!(
        VerbSpec::for_kind(VerbKind::LogOut).menu_policy(),
        IdentityMenuPolicy::BOUNDED
    );
    for kind in [
        VerbKind::AccountHome,
        VerbKind::Settings,
        VerbKind::Notifications,
    ] {
        assert_eq!(
            VerbSpec::for_kind(kind).menu_policy(),
            IdentityMenuPolicy::STANDARD,
            "kind: {kind:?}"
        );
    }
}
