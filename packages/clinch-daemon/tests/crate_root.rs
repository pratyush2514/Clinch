//! Integration tests for `clinch-daemon`.
//!
//! Moved out of `src/main.rs` so the main source stays test-free.

#![allow(clippy::unwrap_used)]

use clinch_daemon::*;

use async_tungstenite::tungstenite::Message;
use futures::StreamExt;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;

fn test_service() -> (service::AppService, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let service = service::AppService::new(dir.path().join("data"), dir.path().join("home"));
    (service, dir)
}

fn test_ctx() -> (ConnCtx, mpsc::UnboundedReceiver<WireMessage>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (ConnCtx::new(tx), rx)
}

fn request(id: u64, cmd: &str, params: serde_json::Value) -> ClientRequest {
    ClientRequest {
        id,
        cmd: cmd.to_owned(),
        params,
    }
}

/// Read the next [`ServerResponse`] off a client WebSocket, skipping any
/// pushes (none are expected in these tests, but the helper is robust).
async fn next_response<S>(ws: &mut async_tungstenite::WebSocketStream<S>) -> ServerResponse
where
    S: futures::AsyncRead + futures::AsyncWrite + Unpin,
{
    loop {
        let msg = ws.next().await.unwrap().unwrap();
        let Message::Text(text) = msg else { continue };
        if let Ok(resp) = serde_json::from_str::<ServerResponse>(&text) {
            return resp;
        }
    }
}

async fn reply_of(service: &service::AppService, req: ClientRequest) -> (ServerResponse, ConnCtx) {
    let (mut ctx, _rx) = test_ctx();
    match handle_request(service, req, &mut ctx).await {
        Action::Reply(response) => (response, ctx),
        Action::Stream { .. } => panic!("expected an immediate reply"),
    }
}

#[test]
fn parse_request_ignores_messages_without_a_readable_id() {
    // Not JSON at all, or JSON without a numeric id: nothing to
    // correlate, so the message is ignored (the connection stays open).
    assert!(parse_request("this is not json").is_none());
    assert!(parse_request(r#"{"cmd":"initialize","params":{}}"#).is_none());
    assert!(parse_request(r#"{"id":"seven","cmd":"initialize"}"#).is_none());
    assert!(parse_request("").is_none());
}

#[test]
fn parse_request_reads_a_well_formed_request() {
    let req = parse_request(r#"{"id":7,"cmd":"initialize","params":{}}"#).unwrap();
    assert_eq!(req.id, 7);
    assert_eq!(req.cmd, "initialize");
}

#[test]
fn parse_request_salvages_the_id_from_a_broken_request_shape() {
    // Valid JSON but not a ClientRequest (cmd has the wrong type): the
    // id is still readable, so the sender gets an invalid_input reply.
    let req = parse_request(r#"{"id":9,"cmd":42}"#).unwrap();
    assert_eq!(req.id, 9);
    assert!(req.cmd.is_empty());
}

#[tokio::test]
async fn unknown_command_replies_invalid_input() {
    let (service, _dir) = test_service();
    let (response, _) =
        reply_of(&service, request(42, "definitely-not-a-command", json!({}))).await;
    assert_eq!(response.id, 42);
    assert!(!response.ok);
    assert!(response.result.is_none());
    assert_eq!(response.error.as_ref().unwrap()["code"], "invalid_input");
}

#[tokio::test]
async fn malformed_params_reply_invalid_input() {
    let (service, _dir) = test_service();
    // task_decision needs {id, index, approved}; a string id is not it.
    let (response, _) = reply_of(
        &service,
        request(7, cmd::TASK_DECISION, json!({"id": "nope"})),
    )
    .await;
    assert_eq!(response.id, 7);
    assert!(!response.ok);
    assert_eq!(response.error.as_ref().unwrap()["code"], "invalid_input");
}

#[tokio::test]
async fn salvaged_broken_shape_replies_invalid_input() {
    let (service, _dir) = test_service();
    let req = parse_request(r#"{"id":11,"cmd":42}"#).unwrap();
    let (response, _) = reply_of(&service, req).await;
    assert_eq!(response.id, 11);
    assert!(!response.ok);
    assert_eq!(response.error.as_ref().unwrap()["code"], "invalid_input");
}

#[tokio::test]
async fn streaming_commands_classify_without_running() {
    let (service, _dir) = test_service();
    let (mut ctx, _rx) = test_ctx();
    // dispatch_natural_command parses params and defers to a spawned
    // task — handle_request itself must not run the engine.
    match handle_request(
        &service,
        request(3, cmd::DISPATCH_NATURAL_COMMAND, json!({"prompt": "hi"})),
        &mut ctx,
    )
    .await
    {
        Action::Stream { id, kind } => {
            assert_eq!(id, 3);
            assert!(matches!(kind, StreamKind::DispatchNatural { .. }));
        }
        Action::Reply(_) => panic!("expected a deferred stream"),
    }
    // Bad params on a streaming command still fail fast.
    let (response, _) = reply_of(&service, request(4, cmd::EXECUTE_PLAYBOOK, json!({}))).await;
    assert!(!response.ok);
}

#[tokio::test]
async fn dispatch_browser_context_status_and_initialize() {
    let (service, _dir) = test_service();
    service.initialize().await.unwrap();

    let (response, _) =
        reply_of(&service, request(1, cmd::BROWSER_CONTEXT_STATUS, json!({}))).await;
    assert!(response.ok, "error: {:?}", response.error);
    assert_eq!(response.id, 1);
    let status = response.result.unwrap();
    assert!(status.is_object());
    assert!(
        status
            .get("attached")
            .is_some_and(serde_json::Value::is_boolean)
    );

    let (response, _) = reply_of(&service, request(2, cmd::INITIALIZE, json!({}))).await;
    assert!(response.ok, "error: {:?}", response.error);
    let status = response.result.unwrap();
    assert!(
        status
            .get("ready")
            .is_some_and(serde_json::Value::is_boolean)
    );
}

#[tokio::test]
async fn dispatch_list_playbooks_and_poc_metrics() {
    let (service, _dir) = test_service();
    service.initialize().await.unwrap();

    let (response, _) = reply_of(&service, request(5, cmd::LIST_PLAYBOOKS, json!({}))).await;
    assert!(response.ok, "error: {:?}", response.error);
    assert!(response.result.as_ref().unwrap().is_array());

    let (response, _) = reply_of(&service, request(6, cmd::GET_POC_METRICS, json!({}))).await;
    assert!(response.ok, "error: {:?}", response.error);
    assert!(response.result.as_ref().unwrap().is_object());
}

#[tokio::test]
async fn listener_binds_an_ephemeral_port() {
    // Startup path: port 0 asks the OS for a free port; the listener
    // must come up without hardcoding 18790.
    let listener = bind_listener(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    assert_ne!(port, 0);
    assert_ne!(port, clinch_protocol::DEFAULT_PORT);
}

#[tokio::test]
async fn socket_round_trip_over_loopback() {
    // Full wire path: real TCP listener, real WebSocket handshake, then
    // requests through `serve_connection` — no browser involved.
    let listener = bind_listener(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (service, _dir) = test_service();
    service.initialize().await.unwrap();
    let service = Arc::new(service);
    let server_service = service.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_connection(server_service, stream).await;
    });

    let (mut ws, _) = async_tungstenite::tokio::connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .unwrap();

    // Happy path.
    ws.send(Message::Text(
        r#"{"id":1,"cmd":"initialize","params":{}}"#.into(),
    ))
    .await
    .unwrap();
    let resp = next_response(&mut ws).await;
    assert_eq!(resp.id, 1);
    assert!(resp.ok);

    // Unknown command -> ok:false, connection stays open.
    ws.send(Message::Text(
        r#"{"id":2,"cmd":"bogus","params":{}}"#.into(),
    ))
    .await
    .unwrap();
    let resp = next_response(&mut ws).await;
    assert_eq!(resp.id, 2);
    assert!(!resp.ok);

    // Garbage with no readable id is ignored; the next valid request
    // still gets its reply on the same connection.
    ws.send(Message::Text("not json at all".into()))
        .await
        .unwrap();
    ws.send(Message::Text(
        r#"{"id":3,"cmd":"browser_context_status","params":{}}"#.into(),
    ))
    .await
    .unwrap();
    let resp = next_response(&mut ws).await;
    assert_eq!(resp.id, 3);
    assert!(resp.ok);

    ws.close(None).await.unwrap();
    server.await.unwrap();
}

#[test]
fn resolve_port_prefers_flag_over_env_over_default() {
    let flag = resolve_port(&[
        "clinch-daemon".to_owned(),
        "--port".to_owned(),
        "19999".to_owned(),
    ])
    .unwrap();
    assert_eq!(flag, 19999);

    assert!(resolve_port(&["clinch-daemon".to_owned(), "--port".to_owned()]).is_err());
    assert!(resolve_port(&["clinch-daemon".to_owned(), "--bogus".to_owned()]).is_err());
}
