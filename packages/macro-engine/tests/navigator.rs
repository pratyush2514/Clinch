//! `PageAction` deserialization contract: the typesafe boundary between the
//! model and the pursuit loop.
//!
//! The model may only ever express `click` / `done` / `give_up`. These tests pin
//! that contract: valid shapes parse, everything else is rejected — which
//! is what makes a separate JSON-schema validator or tool-calling library
//! unnecessary. Rust's type system *is* the integration.

use macro_engine::PageAction;

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
