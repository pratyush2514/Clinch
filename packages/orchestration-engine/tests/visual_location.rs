//! Vision reply parsing and vision-model gating for the visual fallback.
//! Pure — no HTTP, no live model.

use macro_engine::{PageNavigator, VisualLocation};
use orchestration_engine::{LlmPageNavigator, NavigatorEnv, parse_visual_location};

#[test]
fn parses_point_reply() {
    assert_eq!(
        parse_visual_location(r#"{"x": 512, "y": 340.5}"#),
        VisualLocation::Point { x: 512.0, y: 340.5 }
    );
}

#[test]
fn parses_found_false() {
    assert_eq!(
        parse_visual_location(r#"{"found": false}"#),
        VisualLocation::NotFound
    );
}

#[test]
fn tolerates_fences_and_prose() {
    assert_eq!(
        parse_visual_location("```json\n{\"x\": 10, \"y\": 20}\n```"),
        VisualLocation::Point { x: 10.0, y: 20.0 }
    );
    assert_eq!(
        parse_visual_location("Sure! {\"found\": false} Hope that helps."),
        VisualLocation::NotFound
    );
}

#[test]
fn anything_else_is_a_failure_never_a_guess() {
    for reply in [
        "",
        "the button is at the top right",
        r#"{"x": 10}"#,
        r#"{"x": "10", "y": "20"}"#,
        r#"{"found": true}"#,
    ] {
        assert!(
            matches!(parse_visual_location(reply), VisualLocation::Failed(_)),
            "reply {reply:?} must fail"
        );
    }
}

fn navigator(vision_model: Option<&str>) -> LlmPageNavigator {
    let env = NavigatorEnv {
        provider: Some("ollama".to_owned()),
        vision_model: vision_model.map(str::to_owned),
        ..Default::default()
    };
    LlmPageNavigator::from_env_values(&env).unwrap_or_else(|| panic!("ollama navigator builds"))
}

#[test]
fn no_vision_model_is_unsupported() {
    assert_eq!(
        navigator(None).locate_visual("target", "AAAA"),
        VisualLocation::Unsupported
    );
    assert_eq!(
        navigator(Some("   ")).locate_visual("target", "AAAA"),
        VisualLocation::Unsupported
    );
}
