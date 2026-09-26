#![deny(unsafe_code)]
//! elementFromPoint click diagnostics: after a reported click the driver
//! probes which element the page itself has under the click point, so a
//! click that lands on the wrong control (e.g. an ad's "..." button instead
//! of the avatar) shows up as a MISMATCH journal line rather than silent
//! flailing. Hermetic: only the probe parsing and the journal rendering are
//! asserted — no browser is launched, example shapes only.

use browser_driver::ClickHitTest;
use serde_json::json;

/// A typical available probe: the page reports a BUTTON with role `button`
/// under the click point at fractional coordinates (60.4, 40.2).
fn available_hit() -> ClickHitTest {
    ClickHitTest::from_probe(
        60.4,
        40.2,
        &json!({
            "tag": "BUTTON",
            "role": "button",
            "name": "User avatar",
            "idcls": "#avatar-btn .a.b",
        }),
    )
}

#[test]
fn probe_parses_tag_role_name() {
    let hit = available_hit();
    assert!(hit.available);
    assert_eq!(hit.tag, "BUTTON");
    assert_eq!(hit.role, "button");
    assert_eq!(hit.name, "User avatar");
    assert_eq!((hit.x, hit.y), (60.4, 40.2));
}

#[test]
fn null_payload_is_unavailable() {
    let hit = ClickHitTest::from_probe(1.0, 2.0, &json!(null));
    assert!(!hit.available);
}

#[test]
fn wrong_shaped_payload_is_unavailable() {
    // A non-object, a missing tag, and a non-string tag all degrade to
    // unavailable instead of erroring — the click must never fail over
    // diagnostics.
    for payload in [
        json!("just a string"),
        json!({"role": "button", "name": "x"}),
        json!({"tag": 42, "role": "button"}),
    ] {
        let hit = ClickHitTest::from_probe(3.0, 4.0, &payload);
        assert!(!hit.available, "payload: {payload}");
    }
}

#[test]
fn probe_fields_are_capped_at_80_chars() {
    let long = "n".repeat(200);
    let hit = ClickHitTest::from_probe(
        0.0,
        0.0,
        &json!({"tag": "BUTTON", "role": "button", "name": long, "idcls": long}),
    );
    assert!(hit.available);
    assert_eq!(hit.name.chars().count(), 80);
    assert_eq!(hit.role.chars().count(), 6);
}

#[test]
fn journal_line_renders_available_hit() {
    // Coordinates round to integers; the role matches case-insensitively
    // and the name contains the expectation case-insensitively, so no
    // MISMATCH suffix.
    let line = available_hit().journal_line("BUTTON", "AVATAR");
    assert_eq!(
        line,
        "click_hit_test: (60, 40) -> BUTTON role=button name=\"User avatar\""
    );
}

#[test]
fn journal_line_flags_mismatch() {
    let line = available_hit().journal_line("link", "Log out");
    assert_eq!(
        line,
        "click_hit_test: (60, 40) -> BUTTON role=button name=\"User avatar\" \
         MISMATCH(expected role=\"link\" name~=\"Log out\")"
    );
}

#[test]
fn journal_line_matches_on_name_alone() {
    // The role is wrong but the name contains the expectation: no mismatch.
    let line = available_hit().journal_line("link", "avatar");
    assert_eq!(
        line,
        "click_hit_test: (60, 40) -> BUTTON role=button name=\"User avatar\""
    );
}

#[test]
fn journal_line_matches_on_role_alone() {
    // The name misses but the role equals case-insensitively: no mismatch.
    let line = available_hit().journal_line("BUTTON", "settings");
    assert_eq!(
        line,
        "click_hit_test: (60, 40) -> BUTTON role=button name=\"User avatar\""
    );
}

#[test]
fn journal_line_renders_empty_role_as_dash() {
    let hit = ClickHitTest::from_probe(
        10.0,
        20.0,
        &json!({"tag": "DIV", "role": "", "name": "banner"}),
    );
    // Name "banner" contains the expectation, so no MISMATCH.
    assert_eq!(
        hit.journal_line("button", "BANNER"),
        "click_hit_test: (10, 20) -> DIV role=- name=\"banner\""
    );
}

#[test]
fn journal_line_reports_unavailable() {
    let hit = ClickHitTest::from_probe(1.0, 2.0, &json!(null));
    assert_eq!(
        hit.journal_line("button", "avatar"),
        "click_hit_test: unavailable"
    );
}

#[test]
fn journal_line_rounds_coordinates() {
    // 60.5 rounds away from zero; the exact integer rendering matters for
    // journal diffing.
    let hit = ClickHitTest::from_probe(
        60.5,
        39.4,
        &json!({"tag": "A", "role": "link", "name": "x"}),
    );
    assert_eq!(
        hit.journal_line("link", "x"),
        "click_hit_test: (61, 39) -> A role=link name=\"x\""
    );
}
