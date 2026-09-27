//! Integration tests for `clinch_desktop::ws_server` (companion bridge).
//!
//! Moved out of `src/ws_server.rs` so the main source stays test-free.

use clinch_desktop::ws_server::*;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

fn response_fixture() -> serde_json::Value {
    serde_json::json!({
        "requestId": "req-1",
        "domain": "chatgpt.com",
        "userAgent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/131.0.0.0",
        "cookies": [
            {"name": "session", "value": "abc", "domain": "chatgpt.com", "path": "/",
             "secure": true, "httpOnly": true, "sameSite": "lax", "expirationDate": 4_102_444_800.0},
            {"name": "sso", "value": "def", "domain": ".openai.com", "path": "/",
             "secure": true, "httpOnly": true, "sameSite": "strict"},
            {"name": "evil", "value": "x", "domain": "chatgpt.com.evil.com", "path": "/",
             "secure": false, "httpOnly": false, "sameSite": "unspecified"},
            {"name": "huge", "value": "y".repeat(MAX_VALUE_LEN + 1), "domain": "chatgpt.com",
             "path": "/", "secure": true, "httpOnly": true, "sameSite": "lax"},
        ],
    })
}

#[test]
fn scope_covers_portal_subdomains_and_curated_sso() {
    // The worked chatgpt.com example from the bridge contract.
    for domain in [
        "chatgpt.com",
        ".chatgpt.com",
        "auth.openai.com",
        ".openai.com",
    ] {
        assert!(in_scope(domain, "chatgpt.com"), "{domain}");
    }
    for domain in [
        "evil.com",
        "openai.com.evil.com",
        "chatgpt.com.evil.com",
        "notchatgpt.com",
        "",
    ] {
        assert!(!in_scope(domain, "chatgpt.com"), "{domain}");
    }
    // Generic suffix rule without any curated entry.
    assert!(in_scope("auth.example.com", "portal.example.com"));
    assert!(in_scope(".sso.example.com", "portal.example.com"));
    assert!(!in_scope("evil-example.com", "portal.example.com"));
}

#[test]
fn validation_filters_and_rejects_by_rule() -> Result<(), Box<dyn std::error::Error>> {
    let response: SyncResponse = serde_json::from_value(response_fixture())?;
    let session = validate_response("chatgpt.com", response)?;
    // Evil-domain and oversized cookies are dropped; two survive.
    assert_eq!(session.cookies.len(), 2);
    assert_eq!(session.cookies[0].domain, "chatgpt.com");
    assert!(matches!(
        session.cookies[0].same_site,
        session_sync::CookieSameSite::Lax
    ));
    assert_eq!(session.cookies[0].expires, Some(4_102_444_800));
    assert_eq!(session.cookies[1].domain, ".openai.com");
    assert!(session.user_agent.contains("Windows NT"));

    let mismatch: SyncResponse = serde_json::from_value(serde_json::json!({
        "requestId": "req-1", "domain": "evil.com", "userAgent": "Mozilla/5.0",
        "cookies": [{"name": "a", "value": "b", "domain": "evil.com"}],
    }))?;
    assert!(matches!(
        validate_response("chatgpt.com", mismatch),
        Err(BridgeError::DomainMismatch)
    ));

    let empty: SyncResponse = serde_json::from_value(serde_json::json!({
        "requestId": "req-1", "domain": "chatgpt.com", "userAgent": "Mozilla/5.0",
        "cookies": [{"name": "a", "value": "b", "domain": "evil.com"}],
    }))?;
    assert!(matches!(
        validate_response("chatgpt.com", empty),
        Err(BridgeError::NoCookies)
    ));

    let bad_ua: SyncResponse = serde_json::from_value(serde_json::json!({
        "requestId": "req-1", "domain": "chatgpt.com", "userAgent": "",
        "cookies": [{"name": "a", "value": "b", "domain": "chatgpt.com"}],
    }))?;
    assert!(matches!(
        validate_response("chatgpt.com", bad_ua),
        Err(BridgeError::Invalid)
    ));
    Ok(())
}

#[test]
fn unknown_request_ids_are_ignored() {
    let server = BridgeServer::new();
    server.complete_request("no-such-request", Err(BridgeError::Invalid));
    assert!(server.pending.lock().is_ok_and(|guard| guard.is_empty()));
}

#[tokio::test]
async fn bridge_round_trip_over_loopback() -> Result<(), Box<dyn std::error::Error>> {
    use async_tungstenite::tungstenite::Message;
    use futures::StreamExt;

    let (server, _task) = BridgeServer::start(0).await?;
    let port = server.local_port().ok_or("listener has no port")?;
    // Regression guard for `ERR_CONNECTION_REFUSED`: the socket must be
    // an explicit IPv4 loopback bind — never the `localhost` hostname
    // (which may resolve to `::1` while the extension dials `127.0.0.1`).
    let bound = server
        .local_addr
        .lock()
        .map_err(|_| "state lock failed")?
        .ok_or("listener has no address")?;
    assert!(bound.ip().is_loopback());
    assert!(bound.ip().is_ipv4());
    let portal = url::Url::parse("https://chatgpt.com/")?;

    // Fake companion: answer the first SYNC_SESSION with the fixture.
    let responder = tokio::spawn(async move {
        let (mut ws, _) = async_tungstenite::tokio::connect_async(format!("ws://127.0.0.1:{port}"))
            .await
            .map_err(|_| "connect failed")?;
        while let Some(message) = ws.next().await {
            let Message::Text(text) = message.map_err(|_| "read failed")? else {
                continue;
            };
            if !text.contains("SYNC_SESSION") || text.contains("RESPONSE") {
                continue;
            }
            let request: serde_json::Value =
                serde_json::from_str(&text).map_err(|_| "bad request")?;
            let id = request
                .get("requestId")
                .and_then(serde_json::Value::as_str)
                .ok_or("missing requestId")?;
            let mut payload = response_fixture();
            // The unit-test fixture omits the envelope tag that the real
            // extension always sends; the server routes on it.
            payload["type"] = serde_json::Value::String("SYNC_SESSION_RESPONSE".to_owned());
            payload["requestId"] = serde_json::Value::String(id.to_owned());
            ws.send(Message::Text(
                serde_json::to_string(&payload)
                    .map_err(|_| "encode")?
                    .into(),
            ))
            .await
            .map_err(|_| "send failed")?;
            break;
        }
        ws.close(None).await.map_err(|_| "close failed")?;
        Ok::<_, &str>(())
    });

    let session = server
        .request_sync(&portal, Duration::from_secs(10), None)
        .await
        .map_err(|error| format!("request failed: {error}"))?;
    assert_eq!(session.cookies.len(), 2);
    assert_eq!(session.cookies[0].domain, "chatgpt.com");
    assert!(server.is_alive());
    responder
        .await
        .map_err(|_| "responder panicked")?
        .map_err(|reason| format!("responder: {reason}"))?;
    Ok(())
}

fn fake_connection(
    server: &BridgeServer,
    id: u64,
) -> Result<mpsc::Receiver<String>, Box<dyn std::error::Error>> {
    let (tx, rx) = mpsc::channel(8);
    server
        .connections
        .lock()
        .map_err(|_| "connections lock")?
        .insert(
            id,
            Connection {
                created: Instant::now(),
                outbox: tx,
                install_id: String::new(),
                browser: String::new(),
                connected_at_secs: 1_700_000_000,
            },
        );
    Ok(rx)
}

#[test]
fn hello_registers_connection_identity() -> Result<(), Box<dyn std::error::Error>> {
    let server = Arc::new(BridgeServer::new());
    let _rx = fake_connection(&server, 7)?;
    server.handle_text(
        7,
        r#"{"type":"HELLO","name":"clinch-companion","version":1,"installId":"abc-123","browser":"Brave"}"#,
    );
    let infos = server.connection_infos();
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].id, 7);
    assert_eq!(infos[0].install_id, "abc-123");
    assert_eq!(infos[0].browser, "Brave");
    // Unknown connection ids are ignored, never created.
    server.handle_text(99, r#"{"type":"HELLO","installId":"x","browser":"Y"}"#);
    assert_eq!(server.connection_infos().len(), 1);
    Ok(())
}

#[test]
fn hello_identity_is_length_capped() -> Result<(), Box<dyn std::error::Error>> {
    let server = Arc::new(BridgeServer::new());
    let _rx = fake_connection(&server, 3)?;
    let long = "z".repeat(10_000);
    server.handle_text(
        3,
        &format!("{{\"type\":\"HELLO\",\"installId\":\"{long}\",\"browser\":\"{long}\"}}"),
    );
    let infos = server.connection_infos();
    assert!(infos[0].install_id.len() <= MAX_INSTALL_ID_LEN);
    assert!(infos[0].browser.len() <= MAX_BROWSER_LEN);
    Ok(())
}

#[test]
fn targeted_send_reaches_only_the_chosen_connection() -> Result<(), Box<dyn std::error::Error>> {
    let server = BridgeServer::new();
    let mut rx1 = fake_connection(&server, 1)?;
    let mut rx2 = fake_connection(&server, 2)?;
    server.send_to(Some(2), "for-two");
    assert!(
        rx1.try_recv().is_err(),
        "untargeted connection got the message"
    );
    assert_eq!(rx2.try_recv().map_err(|_| "target got nothing")?, "for-two");
    Ok(())
}

#[test]
fn stale_target_falls_back_to_broadcast() -> Result<(), Box<dyn std::error::Error>> {
    let server = BridgeServer::new();
    let mut rx1 = fake_connection(&server, 1)?;
    // The chosen companion reconnected with a new id: the tap still works.
    server.send_to(Some(999), "for-all");
    assert_eq!(
        rx1.try_recv()
            .map_err(|_| "broadcast fallback got nothing")?,
        "for-all"
    );
    Ok(())
}

#[test]
fn ping_gets_a_nonce_echo_pong() -> Result<(), Box<dyn std::error::Error>> {
    let server = Arc::new(BridgeServer::new());
    let mut rx = fake_connection(&server, 5)?;
    server.handle_text(5, r#"{"type":"PING","nonce":"n-42"}"#);
    let reply = rx.try_recv().map_err(|_| "no PONG")?;
    let value: serde_json::Value = serde_json::from_str(&reply).map_err(|_| "PONG is JSON")?;
    assert_eq!(value.get("type").and_then(|t| t.as_str()), Some("PONG"));
    assert_eq!(value.get("nonce").and_then(|t| t.as_str()), Some("n-42"));
    Ok(())
}
