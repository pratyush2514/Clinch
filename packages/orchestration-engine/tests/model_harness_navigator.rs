//! `LlmPageNavigator`'s side of the model harness: the harness-turn entry
//! point (`next_turn`), tolerant multi-action parsing, harness notes in a
//! fixed message shape, the fixed system prompt across turns, and the
//! vision flag the harness reads before capturing a screenshot.
//!
//! Hermetic: every HTTP test points the adapter at a loopback mock.

use browser_driver::AxElement;
use macro_engine::{NavigatorTurn, PageAction, PageNavigator};
use orchestration_engine::{LlmPageNavigator, parse_page_actions, with_harness_notes};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

fn element(id: i64, role: &str, name: &str) -> AxElement {
    AxElement {
        backend_node_id: id,
        role: role.to_owned(),
        name: name.to_owned(),
        description: String::new(),
        container_text: Vec::new(),
        landmark: None,
    }
}

fn snapshot() -> Vec<AxElement> {
    vec![
        element(11, "button", "Open user menu"),
        element(12, "link", "Home"),
    ]
}

/// Loopback mock answering a fixed sequence of requests; every raw
/// request body is appended to `captured` in arrival order.
fn mock_server_seq(responses: Vec<(u16, String)>, captured: Arc<Mutex<Vec<String>>>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap_or_else(|error| panic!("loopback bind: {error}"));
    let addr = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("loopback addr: {error}"));
    std::thread::spawn(move || {
        for (status, response_body) in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let Ok(n) = stream.read(&mut chunk) else {
                    return;
                };
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let header_end = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map_or(buf.len(), |pos| pos + 4);
            let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let content_length: usize = headers
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|line| line.split(':').nth(1))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            while buf.len() - header_end < content_length {
                let Ok(n) = stream.read(&mut chunk) else {
                    return;
                };
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            if let Ok(mut guard) = captured.lock() {
                let end = (header_end + content_length).min(buf.len());
                guard.push(String::from_utf8_lossy(&buf[header_end..end]).into_owned());
            }
            let response = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{addr}")
}

fn groq_envelope(content: &str) -> String {
    serde_json::json!({"choices": [{"message": {"content": content}, "finish_reason": "stop"}]})
        .to_string()
}

fn request_json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|_| panic!("request body is not JSON: {body}"))
}

// ---- parsing ----

#[test]
fn single_action_parses_as_a_one_item_turn() {
    assert_eq!(
        parse_page_actions(r#"{"action": "click", "target": 4}"#),
        Some(vec![PageAction::Click { target: 4 }])
    );
}

#[test]
fn over_answered_array_parses_as_a_batch_in_order() {
    assert_eq!(
        parse_page_actions(
            "```json\n[{\"action\": \"click\", \"target\": 4}, {\"action\": \"done\"}]\n```"
        ),
        Some(vec![PageAction::Click { target: 4 }, PageAction::Done])
    );
    assert_eq!(
        parse_page_actions(
            "Plan: [{\"action\": \"click\", \"target\": 1}, {\"action\": \"click\", \"target\": 2}]"
        ),
        Some(vec![
            PageAction::Click { target: 1 },
            PageAction::Click { target: 2 }
        ])
    );
}

#[test]
fn batch_with_an_invented_action_declines_whole() {
    // The typesafe boundary holds for batches too: nothing outside the
    // closed enum reaches the loop, not even its valid neighbours.
    assert_eq!(
        parse_page_actions(
            r#"[{"action": "click", "target": 1}, {"action": "navigate", "url": "https://evil.example/"}]"#
        ),
        None
    );
    assert_eq!(parse_page_actions("[]"), None);
    assert_eq!(parse_page_actions("no json here"), None);
}

#[test]
fn harness_notes_follow_the_element_list() {
    let rendered = with_harness_notes(
        "[11] button \"Open user menu\"".to_owned(),
        &["Element 9 is stale.".to_owned()],
    );
    assert_eq!(
        rendered,
        "[11] button \"Open user menu\"\nharness notes:\n- Element 9 is stale."
    );
    assert_eq!(
        with_harness_notes("x".to_owned(), &[]),
        "x",
        "no notes, no block"
    );
}

// ---- the harness turn over HTTP ----

#[test]
fn next_turn_sends_notes_and_keeps_the_system_prompt_fixed() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let base = mock_server_seq(
        vec![
            (200, groq_envelope(r#"{"action": "click", "target": 11}"#)),
            (
                200,
                groq_envelope(r#"[{"action": "click", "target": 12}, {"action": "done"}]"#),
            ),
        ],
        captured.clone(),
    );
    let nav = LlmPageNavigator::groq("test-key", &base, "test-model");
    let elements = snapshot();
    let zones = vec![None; elements.len()];
    let first = nav.next_turn(&NavigatorTurn {
        goal: "open settings",
        elements: &elements,
        zones: &zones,
        screenshot_jpeg_b64: None,
        notes: &[],
    });
    assert_eq!(first, Some(vec![PageAction::Click { target: 11 }]));
    let notes =
        vec!["Element 99 from your last answer is no longer on the page (stale ref).".to_owned()];
    let second = nav.next_turn(&NavigatorTurn {
        goal: "open settings",
        elements: &elements,
        zones: &zones,
        screenshot_jpeg_b64: None,
        notes: &notes,
    });
    assert_eq!(
        second,
        Some(vec![PageAction::Click { target: 12 }, PageAction::Done])
    );
    let bodies = captured
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    assert_eq!(bodies.len(), 2);
    let (a, b) = (request_json(&bodies[0]), request_json(&bodies[1]));
    assert_eq!(
        a["messages"][0], b["messages"][0],
        "the system prompt is byte-identical across turns"
    );
    let text = b["messages"][1]["content"].as_str().unwrap_or("");
    assert!(
        text.starts_with("goal: open settings\nelements:\n[11] button"),
        "fixed message shape: {text}"
    );
    assert!(
        text.ends_with("harness notes:\n- Element 99 from your last answer is no longer on the page (stale ref)."),
        "notes after the element list: {text}"
    );
    assert!(
        !a["messages"][1]["content"]
            .as_str()
            .unwrap_or("")
            .contains("harness notes"),
        "no empty notes block"
    );
}

#[test]
fn accepts_screenshots_flips_off_after_a_rejected_image() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let base = mock_server_seq(
        vec![
            (
                400,
                r#"{"error": {"code": "model_not_supported"}}"#.to_owned(),
            ),
            (200, groq_envelope(r#"{"action": "done"}"#)),
        ],
        captured.clone(),
    );
    let nav = LlmPageNavigator::groq("test-key", &base, "text-only-model");
    assert!(nav.accepts_screenshots(), "vision is tried until it fails");
    let elements = snapshot();
    let zones = vec![None; elements.len()];
    let reply = nav.next_turn(&NavigatorTurn {
        goal: "open settings",
        elements: &elements,
        zones: &zones,
        screenshot_jpeg_b64: Some("aGVsbG8="),
        notes: &[],
    });
    assert_eq!(
        reply,
        Some(vec![PageAction::Done]),
        "the turn still answers"
    );
    assert!(
        !nav.accepts_screenshots(),
        "the harness stops capturing once the model rejected the image"
    );
}
