//! Integration tests for `clinch_protocol`.
//!
//! Moved out of `src/lib.rs` so the main source stays test-free.

#![allow(clippy::unwrap_used)]
use clinch_protocol::*;
use serde_json::json;

#[test]
fn request_round_trip() {
    let req = ClientRequest {
        id: 7,
        cmd: cmd::BROWSER_CONTEXT_STATUS.to_owned(),
        params: json!({}),
    };
    let text = serde_json::to_string(&req).unwrap();
    let back: ClientRequest = serde_json::from_str(&text).unwrap();
    assert_eq!(req, back);
}

#[test]
fn response_ok_shape() {
    let resp = ServerResponse::ok(3, json!({"ready": true}));
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["result"]["ready"], true);
    assert!(v.get("error").is_none());
}

#[test]
fn response_err_shape() {
    // Mirrors the embedded AppError serialization (tag/code + message).
    let app_error = json!({"code": "busy", "message": null});
    let resp = ServerResponse::err(3, app_error.clone());
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"], app_error);
}

#[test]
fn push_progress_carries_request_id() {
    let push = ServerPush {
        event: event::PROGRESS.to_owned(),
        id: Some(9),
        payload: json!({"kind": "journal"}),
    };
    let back: ServerPush = serde_json::from_str(&serde_json::to_string(&push).unwrap()).unwrap();
    assert_eq!(back.id, Some(9));
}

#[test]
fn malformed_json_is_not_a_wire_message() {
    assert!(serde_json::from_str::<WireMessage>("{oops").is_err());
    // A JSON value that matches no variant is rejected, not misrouted.
    assert!(serde_json::from_str::<WireMessage>(r#"{"nonsense":1}"#).is_err());
}
