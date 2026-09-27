//! Integration tests for `clinch_desktop::daemon_client`.
//!
//! Moved out of `src/daemon_client.rs` so the main source stays test-free.

mod common;

use common::test_support::{push_text, response_text, spawn_mock_daemon};
use std::time::Duration;

use clinch_desktop::daemon_client::*;
use clinch_protocol::{ServerPush, ServerResponse, cmd, event};

#[tokio::test]
async fn call_round_trips_result_and_params() -> Result<(), Box<dyn std::error::Error>> {
    // The mock echoes the command name and params it received, proving
    // the client sent exactly what the caller asked for.
    let url = spawn_mock_daemon(|request| {
        vec![response_text(&ServerResponse::ok(
            request.id,
            serde_json::json!({"cmd": request.cmd, "params": request.params}),
        ))]
    })
    .await?;
    let client = DaemonClient::connect(&url)
        .await
        .map_err(|error| format!("connect: {error}"))?;
    let result = client
        .call(
            cmd::BROWSER_CONTEXT_STATUS,
            serde_json::json!({"probe": true}),
        )
        .await
        .map_err(|error| format!("unexpected call error: {error}"))?;
    assert_eq!(
        result,
        serde_json::json!({"cmd": "browser_context_status", "params": {"probe": true}})
    );
    Ok(())
}

#[tokio::test]
async fn call_routes_error_value_untouched() -> Result<(), Box<dyn std::error::Error>> {
    let daemon_error = serde_json::json!({"code": "browser_unavailable", "message": "nope"});
    let url = spawn_mock_daemon({
        let daemon_error = daemon_error.clone();
        move |request| {
            vec![response_text(&ServerResponse::err(
                request.id,
                daemon_error.clone(),
            ))]
        }
    })
    .await?;
    let client = DaemonClient::connect(&url)
        .await
        .map_err(|error| format!("connect: {error}"))?;
    let error = client
        .call(cmd::CLOSE_BROWSER, serde_json::json!({}))
        .await
        .map_or_else(|error| error, |ok| panic!("expected error, got {ok:?}"));
    assert_eq!(error, daemon_error);
    Ok(())
}

#[tokio::test]
async fn call_with_progress_delivers_pushes_in_order() -> Result<(), Box<dyn std::error::Error>> {
    let url = spawn_mock_daemon(|request| {
        let id = request.id;
        let mut frames = vec![
            // A push for an unknown id must not leak into this stream.
            push_text(&ServerPush {
                event: event::PROGRESS.to_owned(),
                id: Some(id + 1000),
                payload: serde_json::json!("stray"),
            }),
            // A screencast push is not progress either.
            push_text(&ServerPush {
                event: event::SCREENCAST_FRAME.to_owned(),
                id: None,
                payload: serde_json::json!({"frame": 0}),
            }),
        ];
        for (index, payload) in ["one", "two", "three"].into_iter().enumerate() {
            frames.push(push_text(&ServerPush {
                event: event::PROGRESS.to_owned(),
                id: Some(id),
                payload: serde_json::json!({"seq": index, "note": payload}),
            }));
        }
        frames.push(response_text(&ServerResponse::ok(
            id,
            serde_json::json!({"done": true}),
        )));
        frames
    })
    .await?;
    let client = DaemonClient::connect(&url)
        .await
        .map_err(|error| format!("connect: {error}"))?;
    let (mut pushes, outcome) = client
        .call_with_progress(
            cmd::DISPATCH_NATURAL_COMMAND,
            serde_json::json!({"prompt": "hi"}),
        )
        .await;
    let mut seen = Vec::new();
    while let Some(payload) = pushes.recv().await {
        seen.push(payload);
    }
    assert_eq!(
        seen,
        vec![
            serde_json::json!({"seq": 0, "note": "one"}),
            serde_json::json!({"seq": 1, "note": "two"}),
            serde_json::json!({"seq": 2, "note": "three"}),
        ]
    );
    let final_outcome = outcome
        .await
        .map_err(|_| "outcome channel cancelled")?
        .map_err(|error| format!("unexpected final error: {error}"))?;
    assert_eq!(final_outcome, serde_json::json!({"done": true}));
    Ok(())
}

#[tokio::test]
async fn malformed_frames_do_not_kill_client() -> Result<(), Box<dyn std::error::Error>> {
    let url = spawn_mock_daemon(|request| {
        vec![
            "{oops".to_owned(),
            r#"{"nonsense":1}"#.to_owned(),
            push_text(&ServerPush {
                event: "no-such-event".to_owned(),
                id: None,
                payload: serde_json::json!([]),
            }),
            response_text(&ServerResponse::ok(request.id, serde_json::Value::Null)),
        ]
    })
    .await?;
    let client = DaemonClient::connect(&url)
        .await
        .map_err(|error| format!("connect: {error}"))?;
    // The client dropped the garbage and still matched the real response.
    let result = client
        .call(cmd::PICKER_DISABLE, serde_json::json!({}))
        .await
        .map_err(|error| format!("call failed after malformed frames: {error}"))?;
    assert_eq!(result, serde_json::Value::Null);
    // And the connection is still usable for the next call.
    let again = client
        .call(cmd::PICKER_DISABLE, serde_json::json!({}))
        .await
        .map_err(|error| format!("second call failed: {error}"))?;
    assert_eq!(again, serde_json::Value::Null);
    Ok(())
}

#[tokio::test]
async fn push_stream_carries_frames_and_cursor() -> Result<(), Box<dyn std::error::Error>> {
    let url = spawn_mock_daemon(|request| {
        vec![
            push_text(&ServerPush {
                event: event::SCREENCAST_FRAME.to_owned(),
                id: None,
                payload: serde_json::json!({"frame": "aGVsbG8="}),
            }),
            push_text(&ServerPush {
                event: event::CURSOR_MOVED.to_owned(),
                id: None,
                payload: serde_json::json!({"x": 3, "y": 4}),
            }),
            response_text(&ServerResponse::ok(request.id, serde_json::Value::Null)),
        ]
    })
    .await?;
    let client = DaemonClient::connect(&url)
        .await
        .map_err(|error| format!("connect: {error}"))?;
    // Subscribe before the call: broadcast only delivers what arrives
    // after subscribing.
    let mut pushes = client.subscribe_pushes();
    client
        .call(cmd::ACQUIRE_BROWSER_CONTEXT, serde_json::json!({}))
        .await
        .map_err(|error| format!("acquire failed: {error}"))?;
    let first = tokio::time::timeout(Duration::from_secs(5), pushes.recv())
        .await
        .map_err(|_| "timed out waiting for frame push")?
        .map_err(|_| "push stream closed")?;
    assert_eq!(first.event, event::SCREENCAST_FRAME);
    assert_eq!(first.payload, serde_json::json!({"frame": "aGVsbG8="}));
    let second = tokio::time::timeout(Duration::from_secs(5), pushes.recv())
        .await
        .map_err(|_| "timed out waiting for cursor push")?
        .map_err(|_| "push stream closed")?;
    assert_eq!(second.event, event::CURSOR_MOVED);
    assert_eq!(second.payload, serde_json::json!({"x": 3, "y": 4}));
    Ok(())
}

#[tokio::test]
async fn connect_to_nothing_fails_fast() -> Result<(), Box<dyn std::error::Error>> {
    // Nothing listens on this port; the handshake must fail, not hang.
    // Port 9 (discard) is the classic guaranteed-closed choice.
    let error = DaemonClient::connect("ws://127.0.0.1:9")
        .await
        .map_or_else(|error| error, |_| panic!("expected connect failure"));
    assert!(!error.is_empty());
    Ok(())
}
