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

// --- Vision ---

/// Loopback mock answering a fixed sequence of HTTP requests, then
/// exiting. Each accepted connection consumes the next `(status, body)`
/// pair in order; every raw request body is appended to `captured` in
/// arrival order. Needed for the vision fallback: one turn makes two
/// requests (failed vision, then text-only).
fn mock_server_seq(
    responses: Vec<(u16, String)>,
    captured: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
) -> Option<String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    let addr = listener.local_addr().ok()?;
    std::thread::spawn(move || {
        for (status, response_body) in responses {
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
            if let Ok(mut guard) = captured.lock() {
                let end = (header_end + content_length).min(buf.len());
                guard.push(buf[header_end..end].to_vec());
            }
            let response = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    Some(format!("http://{addr}"))
}

fn visual_zones() -> Vec<Option<macro_engine::PositionZone>> {
    use macro_engine::PositionZone;
    vec![
        Some(PositionZone::TopRight),
        None,
        Some(PositionZone::BottomLeft),
    ]
}

fn captured_bodies(captured: &std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>) -> Vec<String> {
    captured
        .lock()
        .map(|guard| {
            guard
                .iter()
                .map(|body| String::from_utf8_lossy(body).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Parse one captured request body as JSON, panicking with the body on
/// failure so a shape assertion never masks a serialization change.
fn request_json(body: &str) -> serde_json::Value {
    let Ok(request) = serde_json::from_str(body) else {
        panic!("captured request body is not JSON: {body}")
    };
    request
}

#[test]
fn groq_visual_request_carries_image_url_part() {
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let Some(base) = mock_server(
        200,
        groq_envelope(r#"{"action": "done"}"#),
        Some(captured.clone()),
    ) else {
        panic!("loopback mock failed to bind")
    };
    let nav = LlmPageNavigator::groq("test-key", &base, "test-model");
    assert_eq!(
        nav.next_action_visual("profile", &snapshot(), &visual_zones(), Some("aGVsbG8=")),
        Some(PageAction::Done)
    );
    let body = captured.lock().map_or(Vec::new(), |guard| guard.clone());
    let body = String::from_utf8_lossy(&body);
    // Structural assertion (ureq pretty-prints bodies): the user message
    // is the OpenAI-style parts array — text first, then the image as a
    // data URI (the prefix lives only on the wire).
    let request = request_json(&body);
    let content = &request["messages"][1]["content"];
    assert!(content.is_array(), "vision parts array: {body}");
    assert_eq!(content[0]["type"], "text");
    let text = content[0]["text"].as_str().unwrap_or("");
    assert!(text.contains("profile"), "goal in text part: {body}");
    assert!(
        text.contains("Open user menu"),
        "elements in text part: {body}"
    );
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(
        content[1]["image_url"]["url"], "data:image/jpeg;base64,aGVsbG8=",
        "data-URI prefix: {body}"
    );
    assert!(!body.contains("test-key"), "key stays in the header");
}

#[test]
fn groq_visual_none_screenshot_sends_text_only() {
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let Some(base) = mock_server(
        200,
        groq_envelope(r#"{"action": "done"}"#),
        Some(captured.clone()),
    ) else {
        panic!("loopback mock failed to bind")
    };
    let nav = LlmPageNavigator::groq("test-key", &base, "test-model");
    assert_eq!(
        nav.next_action_visual("profile", &snapshot(), &visual_zones(), None),
        Some(PageAction::Done)
    );
    let body = captured.lock().map_or(Vec::new(), |guard| guard.clone());
    let body = String::from_utf8_lossy(&body);
    let request = request_json(&body);
    assert!(
        request["messages"][1]["content"].is_string(),
        "no screenshot means a plain text user message: {body}"
    );
}

#[test]
fn ollama_visual_request_carries_images_array() {
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let body = r#"{"response": "{\"action\": \"done\"}"}"#.to_owned();
    let Some(base) = mock_server_seq(vec![(200, body)], captured.clone()) else {
        panic!("loopback mock failed to bind")
    };
    let nav = LlmPageNavigator::ollama(&base, "test-model");
    assert_eq!(
        nav.next_action_visual("profile", &snapshot(), &visual_zones(), Some("aGVsbG8=")),
        Some(PageAction::Done)
    );
    let bodies = captured_bodies(&captured);
    assert_eq!(bodies.len(), 1);
    let request: serde_json::Value = request_json(&bodies[0]);
    assert_eq!(
        request["images"],
        serde_json::json!(["aGVsbG8="]),
        "images array: {}",
        bodies[0]
    );
}

#[test]
fn groq_vision_failure_falls_back_to_text_once_per_run() {
    // A text-only model (like the default gpt-oss-20b) rejects the image
    // part: first visual turn 400s, the same turn retries text-only, and
    // the next visual turn skips vision entirely.
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let responses = vec![
        (
            400,
            r#"{"error": {"code": "model_not_supported"}}"#.to_owned(),
        ),
        (200, groq_envelope(r#"{"action": "done"}"#)),
        (200, groq_envelope(r#"{"action": "done"}"#)),
    ];
    let Some(base) = mock_server_seq(responses, captured.clone()) else {
        panic!("loopback mock failed to bind")
    };
    let nav = LlmPageNavigator::groq("test-key", &base, "test-model");
    let zones = visual_zones();
    // First visual turn: vision fails, the turn still completes via text.
    assert_eq!(
        nav.next_action_visual("profile", &snapshot(), &zones, Some("aGVsbG8=")),
        Some(PageAction::Done)
    );
    // Second visual turn: vision-disabled — one text request, no vision.
    assert_eq!(
        nav.next_action_visual("profile", &snapshot(), &zones, Some("aGVsbG8=")),
        Some(PageAction::Done)
    );
    let bodies = captured_bodies(&captured);
    assert_eq!(
        bodies.len(),
        3,
        "at most one failed vision call per run: {bodies:?}"
    );
    let first: serde_json::Value = request_json(&bodies[0]);
    assert!(
        first["messages"][1]["content"].is_array(),
        "first request is the vision attempt: {}",
        bodies[0]
    );
    for (index, text_body) in bodies.iter().enumerate().skip(1) {
        let request: serde_json::Value = request_json(text_body);
        assert!(
            request["messages"][1]["content"].is_string(),
            "request {index} must be text-only: {text_body}"
        );
    }
}

#[test]
fn ollama_vision_failure_falls_back_to_text_once_per_run() {
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let responses = vec![
        (400, r#"{"error": "images not supported"}"#.to_owned()),
        (200, r#"{"response": "{\"action\": \"done\"}"}"#.to_owned()),
        (200, r#"{"response": "{\"action\": \"done\"}"}"#.to_owned()),
    ];
    let Some(base) = mock_server_seq(responses, captured.clone()) else {
        panic!("loopback mock failed to bind")
    };
    let nav = LlmPageNavigator::ollama(&base, "test-model");
    let zones = visual_zones();
    assert_eq!(
        nav.next_action_visual("profile", &snapshot(), &zones, Some("aGVsbG8=")),
        Some(PageAction::Done)
    );
    assert_eq!(
        nav.next_action_visual("profile", &snapshot(), &zones, Some("aGVsbG8=")),
        Some(PageAction::Done)
    );
    let bodies = captured_bodies(&captured);
    assert_eq!(
        bodies.len(),
        3,
        "at most one failed vision call per run: {bodies:?}"
    );
    let first: serde_json::Value = request_json(&bodies[0]);
    assert_eq!(
        first["images"],
        serde_json::json!(["aGVsbG8="]),
        "first request is the vision attempt: {}",
        bodies[0]
    );
    for (index, text_body) in bodies.iter().enumerate().skip(1) {
        let request: serde_json::Value = request_json(text_body);
        assert!(
            request.get("images").is_none(),
            "request {index} must be text-only: {text_body}"
        );
    }
}

// --- Escalation config ---
// Hermetic: the pure `from_escalation_values` path only, so no test ever
// touches the process environment.

#[test]
fn escalation_unset_model_means_no_escalation() {
    // `CLINCH_ESCALATION_MODEL` unset: today's behavior, unchanged — even
    // with a live provider configured elsewhere.
    let env = NavigatorEnv {
        provider: Some("groq".to_owned()),
        groq_api_key: Some("test-key".to_owned()),
        ..Default::default()
    };
    assert!(LlmPageNavigator::from_escalation_values(&env).is_none());
}

#[test]
fn escalation_empty_or_whitespace_model_means_no_escalation() {
    for model in ["", "   ", "\t\n "] {
        let env = NavigatorEnv {
            escalation_provider: Some("groq".to_owned()),
            groq_api_key: Some("test-key".to_owned()),
            escalation_model: Some(model.to_owned()),
            ..Default::default()
        };
        assert!(
            LlmPageNavigator::from_escalation_values(&env).is_none(),
            "model: {model:?}"
        );
    }
}

#[test]
fn escalation_groq_with_model_builds() {
    let env = NavigatorEnv {
        escalation_provider: Some("groq".to_owned()),
        groq_api_key: Some("test-key".to_owned()),
        escalation_model: Some("escalation-model".to_owned()),
        ..Default::default()
    };
    let nav = LlmPageNavigator::from_escalation_values(&env).expect("escalation navigator");
    assert_eq!(nav.model_name(), "escalation-model");
}

#[test]
fn escalation_groq_model_overrides_grounder_model() {
    // The escalation model wins over the regular navigator's model: it is
    // a separate, deliberately chosen wave-2 model.
    let env = NavigatorEnv {
        groq_api_key: Some("test-key".to_owned()),
        groq_model: Some("regular-model".to_owned()),
        escalation_model: Some("  escalation-model  ".to_owned()),
        ..Default::default()
    };
    let nav = LlmPageNavigator::from_escalation_values(&env).expect("escalation navigator");
    assert_eq!(nav.model_name(), "escalation-model");
}

#[test]
fn escalation_provider_unset_defaults_to_groq() {
    let env = NavigatorEnv {
        groq_api_key: Some("test-key".to_owned()),
        escalation_model: Some("escalation-model".to_owned()),
        ..Default::default()
    };
    let nav = LlmPageNavigator::from_escalation_values(&env).expect("groq is the default");
    assert_eq!(nav.model_name(), "escalation-model");
}

#[test]
fn escalation_groq_default_still_requires_key() {
    let env = NavigatorEnv {
        escalation_model: Some("escalation-model".to_owned()),
        ..Default::default()
    };
    assert!(LlmPageNavigator::from_escalation_values(&env).is_none());
}

#[test]
fn escalation_ollama_builds_with_defaults() {
    let env = NavigatorEnv {
        escalation_provider: Some("ollama".to_owned()),
        escalation_model: Some("escalation-model".to_owned()),
        ..Default::default()
    };
    let nav = LlmPageNavigator::from_escalation_values(&env).expect("ollama escalation");
    assert_eq!(nav.model_name(), "escalation-model");
}

#[test]
fn escalation_ollama_builds_without_key() {
    // Ollama selection is proven by needing no key, where groq declines.
    let env = NavigatorEnv {
        escalation_provider: Some("ollama".to_owned()),
        escalation_model: Some("escalation-model".to_owned()),
        ..Default::default()
    };
    assert!(LlmPageNavigator::from_escalation_values(&env).is_some());
}

#[test]
fn escalation_unknown_provider_means_no_escalation() {
    let env = NavigatorEnv {
        escalation_provider: Some("Muse".to_owned()),
        groq_api_key: Some("test-key".to_owned()),
        escalation_model: Some("escalation-model".to_owned()),
        ..Default::default()
    };
    assert!(LlmPageNavigator::from_escalation_values(&env).is_none());
}

#[test]
fn escalation_blank_provider_string_means_groq_default() {
    // Blank is not "unrecognized": it falls back to the groq default.
    let env = NavigatorEnv {
        escalation_provider: Some(" ".to_owned()),
        groq_api_key: Some("test-key".to_owned()),
        escalation_model: Some("escalation-model".to_owned()),
        ..Default::default()
    };
    let nav = LlmPageNavigator::from_escalation_values(&env).expect("groq is the default");
    assert_eq!(nav.model_name(), "escalation-model");
}
