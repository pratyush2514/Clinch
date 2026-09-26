//! Integration tests for the `DevTools` handshake helpers in `browser_driver`:
//! [`pick_free_port`] and [`probe_devtools_http`].
//!
//! The launch path no longer trusts `--remote-debugging-port=0` (on some
//! Linux builds Chrome binds the ephemeral port but its `DevTools` HTTP
//! server never answers). Instead the driver allocates the port itself and
//! confirms readiness over HTTP (`/json/version` → `webSocketDebuggerUrl`)
//! before opening the CDP WebSocket. These tests drive the helpers against
//! a tiny scripted TCP server — no real Chromium needed.

use browser_driver::{pick_free_port, probe_devtools_http};
use std::fmt::Debug;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

/// Unwrap a test-setup result, panicking with context on failure.
///
/// (Plain `.expect()` trips the workspace's `clippy::expect-used` lint,
/// so test setup goes through this instead.)
fn must<T, E: Debug>(result: Result<T, E>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("test setup failed ({what}): {error:?}"),
    }
}

/// Serve one canned HTTP response on a fresh loopback port, then close.
///
/// Returns the port. The server thread reads (and discards) the request
/// before writing the response, mirroring a minimal HTTP/1.1 exchange.
fn serve_once(response: Vec<u8>) -> u16 {
    let listener = must(TcpListener::bind("127.0.0.1:0"), "bind mock server");
    let port = must(listener.local_addr(), "mock addr").port();
    thread::spawn(move || {
        let (mut stream, _) = must(listener.accept(), "mock accept");
        let mut buf = [0u8; 2048];
        let _ = stream.read(&mut buf);
        let _ = stream.write_all(&response);
        // `stream` drops here → connection closes (like `Connection: close`).
    });
    // Let the server thread reach `accept()` before the probe connects.
    thread::sleep(Duration::from_millis(50));
    port
}

/// A minimal `/json/version`-shaped response carrying the given WS URL.
fn version_response(ws_url: &str) -> Vec<u8> {
    let body = format!(
        "{{\n   \"Browser\": \"Chrome/154.0.8037.57\",\n   \
         \"Protocol-Version\": \"1.3\",\n   \
         \"webSocketDebuggerUrl\": \"{ws_url}\"\n}}"
    );
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[test]
fn pick_free_port_returns_a_bindable_port() {
    let port = must(pick_free_port(), "pick_free_port");
    assert_ne!(port, 0, "ephemeral port must be non-zero");
    // The port was free a moment ago; rebinding it must succeed
    // (barring an unlikely race with another process on the machine).
    let _listener = must(
        TcpListener::bind(("127.0.0.1", port)),
        "picked port should be bindable",
    );
}

#[tokio::test]
async fn probe_returns_ws_url_on_200() {
    let ws_url = "ws://127.0.0.1:39999/devtools/browser/abe877b2-1234";
    let port = serve_once(version_response(ws_url));
    assert_eq!(
        probe_devtools_http(port).await.as_deref(),
        Some(ws_url),
        "probe must extract webSocketDebuggerUrl from /json/version"
    );
}

#[tokio::test]
async fn probe_returns_none_when_nothing_listens() {
    let port = must(pick_free_port(), "pick_free_port");
    // Nothing is bound: the connect must fail fast and the probe must
    // report "not ready" rather than erroring.
    assert_eq!(probe_devtools_http(port).await, None);
}

#[tokio::test]
async fn probe_returns_none_on_non_200_status() {
    let port = serve_once(
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
    );
    assert_eq!(probe_devtools_http(port).await, None);
}

#[tokio::test]
async fn probe_returns_none_on_malformed_json_body() {
    let body = b"this is not json";
    let raw = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut response = raw.into_bytes();
    response.extend_from_slice(body);
    let port = serve_once(response);
    assert_eq!(probe_devtools_http(port).await, None);
}

#[tokio::test]
async fn probe_returns_none_when_ws_url_field_missing() {
    let body = br#"{"Browser": "Chrome/154.0.8037.57", "Protocol-Version": "1.3"}"#;
    let raw = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut response = raw.into_bytes();
    response.extend_from_slice(body);
    let port = serve_once(response);
    assert_eq!(probe_devtools_http(port).await, None);
}
