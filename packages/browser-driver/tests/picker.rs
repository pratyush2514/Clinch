//! Integration tests for `browser_driver::picker`.
//!
//! Moved out of `src/` so the main source stays test-free.

use browser_driver::picker::*;

#[test]
fn binding_payload_roundtrip_and_validation() -> Result<(), Box<dyn std::error::Error>> {
    let picked = PickedElement {
        tag: "a".into(),
        selectors: vec!["[data-testid=\"report-link\"]".into(), "a.report".into()],
        rect: PickerRect {
            x: 8.0,
            y: 16.0,
            width: 120.0,
            height: 24.0,
        },
        text: "Download report".into(),
    };
    let payload = serde_json::to_string(&picked)?;
    assert_eq!(parse_binding_payload(&payload)?, picked);
    assert_eq!(picked.primary(), Some("[data-testid=\"report-link\"]"));
    Ok(())
}

#[test]
fn binding_payload_rejects_shape_and_bounds_violations() {
    // Wrong envelope: binding sends exactly one JSON object.
    assert!(parse_binding_payload("not-json").is_err());
    assert!(parse_binding_payload("{\"tag\":\"a\"}").is_err());
    // Oversized selector / text budgets fail closed.
    let bad = PickedElement {
        tag: "a".into(),
        selectors: vec!["x".repeat(3000)],
        rect: PickerRect {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
        },
        text: String::new(),
    };
    assert!(bad.validate().is_err());
    let bad_rect = PickedElement {
        tag: "a".into(),
        selectors: vec!["a".into()],
        rect: PickerRect {
            x: f64::NAN,
            y: 0.0,
            width: 10.0,
            height: 10.0,
        },
        text: String::new(),
    };
    assert!(bad_rect.validate().is_err());
    let empty = PickedElement {
        tag: "a".into(),
        selectors: Vec::new(),
        rect: PickerRect {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
        },
        text: String::new(),
    };
    assert!(empty.validate().is_err());
    assert!(parse_binding_payload(&"x".repeat(9000)).is_err());
}

#[test]
fn selector_ranking_prefers_stable_hooks() {
    let ranked = rank_selectors_from_attrs(
        "BUTTON",
        Some("submit-btn"),
        Some("report-submit"),
        Some("Submit report"),
        &["primary-btn".into(), "extra".into()],
        Some("submit"),
    );
    assert_eq!(ranked[0], "[data-testid=\"report-submit\"]");
    assert_eq!(ranked[1], "[aria-label=\"Submit report\"]");
    assert_eq!(ranked[2], "#submit-btn");
    assert!(ranked[3].starts_with("button."));
    assert!(ranked[3].contains("[type=\"submit\"]"));

    let minimal = rank_selectors_from_attrs("a", None, None, None, &[], None);
    assert_eq!(minimal, vec!["a".to_string()]);
}

#[test]
fn picker_script_contains_mandated_contract() {
    assert!(PICKER_SCRIPT.contains("__clinch_element_picked__"));
    assert!(PICKER_SCRIPT.contains("outline"));
    assert!(PICKER_SCRIPT.contains("2px solid #3b82f6"));
    assert!(PICKER_SCRIPT.contains("rgba(59, 130, 246, 0.1)"));
    assert!(PICKER_SCRIPT.contains("data-testid"));
    assert!(PICKER_SCRIPT.contains("aria-label"));
    assert!(PICKER_SCRIPT.contains("__clinchPickerTeardown"));
    assert_eq!(PICKER_BINDING, "__clinch_element_picked__");
}
