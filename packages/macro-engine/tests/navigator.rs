//! `PageAction` deserialization contract: the typesafe boundary between the
//! model and the pursuit loop.
//!
//! The model may only ever express `click` / `done` / `give_up`. These tests pin
//! that contract: valid shapes parse, everything else is rejected — which
//! is what makes a separate JSON-schema validator or tool-calling library
//! unnecessary. Rust's type system *is* the integration.

use browser_driver::AxElement;
use macro_engine::{PageAction, PageNavigator, PositionZone};

fn parse(json: &str) -> Result<PageAction, serde_json::Error> {
    serde_json::from_str(json)
}

#[test]
fn click_parses() {
    assert_eq!(
        parse(r#"{"action": "click", "target": 42}"#).unwrap(),
        PageAction::Click { target: 42 }
    );
}

#[test]
fn done_parses() {
    assert_eq!(parse(r#"{"action": "done"}"#).unwrap(), PageAction::Done);
}

#[test]
fn give_up_parses_with_reason() {
    assert_eq!(
        parse(r#"{"action": "give_up", "reason": "no profile control"}"#).unwrap(),
        PageAction::GiveUp {
            reason: "no profile control".to_owned()
        }
    );
}

#[test]
fn unknown_action_is_rejected() {
    // A model inventing a fourth action must not reach the loop.
    assert!(parse(r#"{"action": "navigate", "url": "https://evil.example/"}"#).is_err());
}

#[test]
fn click_without_target_is_rejected() {
    assert!(parse(r#"{"action": "click"}"#).is_err());
}

#[test]
fn click_with_string_target_is_rejected() {
    // The loop compares ids as i64; a string id must fail here, not coerce.
    assert!(parse(r#"{"action": "click", "target": "42"}"#).is_err());
}

#[test]
fn extra_fields_are_ignored() {
    // The contract is about the action shape, not the payload's purity:
    // extra fields never create a new action, so they are ignored — the
    // same leniency the domain grounder applies.
    assert_eq!(
        parse(r#"{"action": "done", "confidence": 0.9}"#).unwrap(),
        PageAction::Done
    );
}

#[test]
fn prose_is_rejected() {
    assert!(parse("I think you should click the avatar").is_err());
}

#[test]
fn empty_is_rejected() {
    assert!(parse("").is_err());
}

/// A navigator that overrides only the zoned variant, so the visual
/// default's delegation path is observable: the plain variant declines,
/// the zoned one answers.
struct ZonedOnlyNavigator;

impl PageNavigator for ZonedOnlyNavigator {
    fn next_action(&self, _goal: &str, _elements: &[AxElement]) -> Option<PageAction> {
        // Must not be reached by the visual default.
        None
    }

    fn next_action_zoned(
        &self,
        _goal: &str,
        _elements: &[AxElement],
        _zones: &[Option<PositionZone>],
    ) -> Option<PageAction> {
        Some(PageAction::Done)
    }
}

#[test]
fn visual_default_ignores_screenshot_and_delegates_to_zoned() {
    let nav = ZonedOnlyNavigator;
    let elements: Vec<AxElement> = Vec::new();
    let zones: Vec<Option<PositionZone>> = vec![None, Some(PositionZone::TopRight)];
    // With a screenshot: the default drops it and reaches the zoned
    // variant (`Done`, where the plain variant would decline).
    assert_eq!(
        nav.next_action_visual("goal", &elements, &zones, Some("aGVsbG8=")),
        Some(PageAction::Done)
    );
    // Without one: same path, same answer.
    assert_eq!(
        nav.next_action_visual("goal", &elements, &zones, None),
        Some(PageAction::Done)
    );
}
