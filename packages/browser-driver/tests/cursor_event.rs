//! Cursor-event wire contract: the payload the UI overlay depends on.
//!
//! These tests pin the shape the frontend matches on — coordinates in page
//! CSS pixels, the viewport they are relative to, the CDP screencast session
//! id for stale-session filtering, and the `snake_case` kind strings
//! ("move"/"press"/"release") the overlay switches on. A rename or reshape
//! here breaks the agent pointer without any automation noticing, because the
//! events are purely presentational: dispatches never read them.

use browser_driver::{CursorEvent, CursorEventKind, FALLBACK_VIEWPORT, NO_CURSOR_SESSION};

/// Example coordinates: dead-center of the default 1920x1080 off-screen
/// viewport the cursor overlay is built for.
#[test]
fn cursor_event_carries_coordinates_viewport_and_session() {
    let event = CursorEvent::new(CursorEventKind::Press, 960.0, 540.0, FALLBACK_VIEWPORT, 42);
    // Whole-struct equality: exact field comparison without tripping
    // `float_cmp` on the individual `f64`s.
    assert_eq!(
        event,
        CursorEvent {
            x: 960.0,
            y: 540.0,
            kind: CursorEventKind::Press,
            viewport_width: 1920.0,
            viewport_height: 1080.0,
            session_id: 42,
        }
    );
}

#[test]
fn cursor_event_kind_serializes_snake_case_for_the_frontend() {
    let event = CursorEvent::new(
        CursorEventKind::Move,
        0.0,
        0.0,
        FALLBACK_VIEWPORT,
        NO_CURSOR_SESSION,
    );
    let json = serde_json::to_string(&event).unwrap_or_default();
    assert!(json.contains(r#""kind":"move""#), "got {json}");
    let kind = serde_json::to_string(&CursorEventKind::Release).unwrap_or_default();
    assert_eq!(kind, r#""release""#);
}

#[test]
fn no_cursor_session_is_negative_so_it_never_matches_a_latched_session() {
    // A negative session id can never collide with a real CDP screencast
    // session, so the UI can safely drop events carrying it.
    let event = CursorEvent::new(
        CursorEventKind::Move,
        1.0,
        2.0,
        FALLBACK_VIEWPORT,
        NO_CURSOR_SESSION,
    );
    assert!(event.session_id < 0);
    assert_eq!(event.session_id, NO_CURSOR_SESSION);
}
