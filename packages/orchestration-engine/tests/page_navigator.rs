//! `LlmPageNavigator` through its public surface: provider selection,
//! strict-JSON action parsing (including the decorations models add), and
//! the privacy fence (only goal + element list leave the machine).
//!
//! Hermetic: every HTTP test points the adapter at a loopback mock — no
//! internet, no fixed port, no credentials.

use browser_driver::AxElement;
use macro_engine::{PageAction, PageNavigator};
use orchestration_engine::{LlmPageNavigator, NavigatorEnv};
use std::io::{Read, Write};

fn element(id: i64, role: &str, name: &str, landmark: Option<&str>) -> AxElement {
    AxElement {
        backend_node_id: id,
        role: role.to_owned(),
        name: name.to_owned(),
        description: String::new(),
        container_text: Vec::new(),
        landmark: landmark.map(str::to_owned),
    }
}

fn snapshot() -> Vec<AxElement> {
    vec![
        element(1, "button", "Open user menu", Some("banner")),
        element(2, "link", "Home", Some("navigation")),
        element(3, "button", "Search", Some("banner")),
    ]
}

/// Loopback mock answering one HTTP request with `response_body`, then
/// exiting. Returns the base URL to point the adapter at. When
/// `captured_body` is given, the raw request body is stored for assertion.
fn mock_server(
    status: u16,
    response_body: String,
    captured_body: Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>,
) -> Option<String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    let addr = listener.local_addr().ok()?;
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let Ok(n) = stream.read(&mut chunk) else {
                return;
            };
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let header_end = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map_or(buf.len(), |pos| pos + 4);
        let headers = String::from_utf8_lossy(&buf[..header_end]);
        let content_length: usize = headers
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
            .and_then(|line| line.split(':').nth(1))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0);
        let mut body_read = buf.len().saturating_sub(header_end);
        while body_read < content_length {
            let Ok(n) = stream.read(&mut chunk) else {
                return;
            };
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            body_read += n;
        }
        if let Some(sink) = captured_body
            && let Ok(mut guard) = sink.lock()
        {
            let end = (header_end + content_length).min(buf.len());
            guard.extend_from_slice(&buf[header_end..end]);
        }
        let response = format!(
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    });
    Some(format!("http://{addr}"))
}

fn mock_base(response_body: String) -> String {
    match mock_server(200, response_body, None) {
        Some(base) => base,
        None => panic!("loopback mock failed to bind"),
    }
}

/// Groq chat-completion envelope around a raw assistant content string.
fn groq_envelope(content: &str) -> String {
    let escaped = content
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!(
        r#"{{"choices": [{{"message": {{"content": "{escaped}"}}, "finish_reason": "stop"}}]}}"#
    )
}

// --- Provider selection ---

#[test]
fn no_provider_means_no_navigator() {
    assert!(LlmPageNavigator::from_env_values(&NavigatorEnv::default()).is_none());
}

#[test]
fn unknown_provider_means_no_navigator() {
    let env = NavigatorEnv {
        provider: Some("Muse".to_owned()),
        ..Default::default()
    };
    assert!(LlmPageNavigator::from_env_values(&env).is_none());
}

#[test]
fn groq_without_key_means_no_navigator() {
    let env = NavigatorEnv {
        provider: Some("groq".to_owned()),
        ..Default::default()
    };
    assert!(LlmPageNavigator::from_env_values(&env).is_none());
}

#[test]
fn groq_with_key_builds() {
    let env = NavigatorEnv {
        provider: Some("groq".to_owned()),
        groq_api_key: Some("test-key".to_owned()),
        ..Default::default()
    };
    assert!(LlmPageNavigator::from_env_values(&env).is_some());
}

#[test]
fn ollama_builds_without_key() {
    let env = NavigatorEnv {
        provider: Some("ollama".to_owned()),
        ..Default::default()
    };
    assert!(LlmPageNavigator::from_env_values(&env).is_some());
}

// --- Groq action parsing ---

fn groq_navigator(content: &str) -> LlmPageNavigator {
    let base = mock_base(groq_envelope(content));
    LlmPageNavigator::groq("test-key", &base, "test-model")
}

#[test]
fn groq_click_parses() {
    let nav = groq_navigator(r#"{"action": "click", "target": 1}"#);
    assert_eq!(
        nav.next_action("profile", &snapshot()),
        Some(PageAction::Click { target: 1 })
    );
}

#[test]
fn groq_fenced_json_parses() {
    let nav = groq_navigator("```json\n{\"action\": \"done\"}\n```");
    assert_eq!(
        nav.next_action("profile", &snapshot()),
        Some(PageAction::Done)
    );
}

#[test]
fn groq_prose_wrapped_json_parses() {
    let nav = groq_navigator("Sure, here it is: {\"action\": \"click\", \"target\": 2}");
    assert_eq!(
        nav.next_action("profile", &snapshot()),
        Some(PageAction::Click { target: 2 })
    );
}

#[test]
fn groq_give_up_parses() {
    let nav = groq_navigator(r#"{"action": "give_up", "reason": "no menu"}"#);
    assert_eq!(
        nav.next_action("profile", &snapshot()),
        Some(PageAction::GiveUp {
            reason: "no menu".to_owned()
        })
    );
}

#[test]
fn groq_garbage_content_declines() {
    let nav = groq_navigator("I have no idea what to click");
    assert_eq!(nav.next_action("profile", &snapshot()), None);
}

#[test]
fn groq_invented_action_declines() {
    // The typesafe boundary: a fourth action never reaches the loop.
    let nav = groq_navigator(r#"{"action": "navigate", "url": "https://evil.example/"}"#);
    assert_eq!(nav.next_action("profile", &snapshot()), None);
}

#[test]
fn groq_http_error_declines() {
    let Some(base) = mock_server(
        500,
        r#"{"error": {"code": "server_error"}}"#.to_owned(),
        None,
    ) else {
        panic!("loopback mock failed to bind")
    };
    let nav = LlmPageNavigator::groq("test-key", &base, "test-model");
    assert_eq!(nav.next_action("profile", &snapshot()), None);
}

#[test]
fn groq_empty_content_declines() {
    let nav = groq_navigator("   ");
    assert_eq!(nav.next_action("profile", &snapshot()), None);
}

// --- Ollama ---

#[test]
fn ollama_click_parses() {
    let body = r#"{"response": "{\"action\": \"click\", \"target\": 3}"}"#.to_owned();
    let base = mock_base(body);
    let nav = LlmPageNavigator::ollama(&base, "test-model");
    assert_eq!(
        nav.next_action("profile", &snapshot()),
        Some(PageAction::Click { target: 3 })
    );
}

#[test]
fn ollama_malformed_declines() {
    let base = mock_base(r#"{"nope": true}"#.to_owned());
    let nav = LlmPageNavigator::ollama(&base, "test-model");
    assert_eq!(nav.next_action("profile", &snapshot()), None);
}

// --- Privacy fence ---

#[test]
fn request_carries_only_goal_and_elements() {
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let Some(base) = mock_server(
        200,
        groq_envelope(r#"{"action": "done"}"#),
        Some(captured.clone()),
    ) else {
        panic!("loopback mock failed to bind")
    };
    let nav = LlmPageNavigator::groq("test-key", &base, "test-model");
    let _ = nav.next_action("my profile", &snapshot());
    let body = captured.lock().map_or(Vec::new(), |guard| guard.clone());
    let body = String::from_utf8_lossy(&body);
    // The fence: goal and element names travel; nothing else does.
    assert!(body.contains("my profile"), "goal must be sent");
    assert!(body.contains("Open user menu"), "elements must be sent");
    assert!(!body.contains("test-key"), "key must stay in the header");
}

// --- Zone-aware rendering ---

#[test]
fn zoned_rendering_surfaces_position_zones() {
    use macro_engine::PositionZone;
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let base = mock_server(
        200,
        groq_envelope(r#"{"action": "done"}"#),
        Some(captured.clone()),
    )
    .expect("loopback mock failed to bind");
    let nav = LlmPageNavigator::groq("test-key", &base, "test-model");
    let zones = vec![
        Some(PositionZone::TopRight),
        None,
        Some(PositionZone::BottomLeft),
    ];
    assert_eq!(
        nav.next_action_zoned("profile", &snapshot(), &zones),
        Some(PageAction::Done)
    );
    let body = captured
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    let body = String::from_utf8_lossy(&body);
    // The request body is JSON: quotes arrive escaped, newlines as `\n`.
    // Zoned lines carry their zone; unzoned lines render no suffix.
    assert!(
        body.contains("[1] button \\\"Open user menu\\\" (banner) [top-right]"),
        "rendered: {body}"
    );
    assert!(
        body.contains("[2] link \\\"Home\\\" (navigation)"),
        "rendered: {body}"
    );
    assert!(
        !body.contains("[2] link \\\"Home\\\" (navigation) ["),
        "rendered: {body}"
    );
    assert!(
        body.contains("[3] button \\\"Search\\\" (banner) [bottom-left]"),
        "rendered: {body}"
    );
}

#[test]
fn unzoned_fallback_renders_without_zones() {
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let base = mock_server(
        200,
        groq_envelope(r#"{"action": "done"}"#),
        Some(captured.clone()),
    )
    .expect("loopback mock failed to bind");
    let nav = LlmPageNavigator::groq("test-key", &base, "test-model");
    // The plain trait method degrades to zone-less rendering.
    assert_eq!(
        nav.next_action("profile", &snapshot()),
        Some(PageAction::Done)
    );
    let body = captured
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    let body = String::from_utf8_lossy(&body);
    // No zone suffix on any element line (the system prompt may name zones;
    // the rendered elements must not).
    assert!(
        !body.contains("(banner) [") && !body.contains("(navigation) ["),
        "rendered: {body}"
    );
}
