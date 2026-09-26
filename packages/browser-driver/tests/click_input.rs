#![deny(unsafe_code)]
//! Trusted-input click contract: the action engine's clicks must go out as
//! a human-like hover → press → release sequence through CDP input events,
//! never a synthetic DOM click. Hermetic: only the event construction is
//! asserted, no browser is launched. Example shapes only — no real sites.

use browser_driver::{WindowMode, click_event_sequence};
use chromiumoxide::cdp::browser_protocol::input::{DispatchMouseEventType, MouseButton};

#[test]
fn click_sequence_is_hover_press_release_in_order() {
    let [hover, press, release] = click_event_sequence(120.0, 80.0).expect("sequence builds");
    assert_eq!(hover.r#type, DispatchMouseEventType::MouseMoved);
    assert_eq!(press.r#type, DispatchMouseEventType::MousePressed);
    assert_eq!(release.r#type, DispatchMouseEventType::MouseReleased);
}

#[test]
fn click_sequence_targets_the_same_point() {
    let [hover, press, release] = click_event_sequence(120.5, 80.25).expect("sequence builds");
    for event in [&hover, &press, &release] {
        assert_eq!((event.x, event.y), (120.5, 80.25));
    }
}

#[test]
fn click_sequence_presses_and_releases_the_left_button() {
    let [_, press, release] = click_event_sequence(10.0, 10.0).expect("sequence builds");
    assert_eq!(press.button, Some(MouseButton::Left));
    assert_eq!(press.buttons, Some(1));
    assert_eq!(press.click_count, Some(1));
    assert_eq!(release.buttons, Some(0));
}

#[test]
fn window_mode_serializes_for_the_ui_badge() {
    // The frontend badge keys off this string: off-screen headed must not
    // serialize as "headless".
    assert_eq!(
        serde_json::to_string(&WindowMode::Offscreen).expect("serializes"),
        "\"offscreen\""
    );
    assert_eq!(
        serde_json::to_string(&WindowMode::Headless).expect("serializes"),
        "\"headless\""
    );
    assert_eq!(
        serde_json::to_string(&WindowMode::Headed).expect("serializes"),
        "\"headed\""
    );
}
