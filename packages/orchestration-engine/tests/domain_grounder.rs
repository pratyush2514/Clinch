//! Integration tests for `orchestration_engine::domain_grounder`.
//!
//! Moved out of `src/domain_grounder.rs` so the main source stays test-free.

use orchestration_engine::DomainGrounder;
use orchestration_engine::domain_grounder::*;
use std::io::{Read, Write};

/// Loopback mock that answers one HTTP request with `response_body` as
/// `application/json`, then exits. Returns the base URL to point the
/// adapter at — hermetic: no internet, no fixed port. `None` when the
/// loopback bind itself fails (the test then fails closed with a
/// clear panic instead of an `expect`). When `captured_body` is given,
/// the raw request body is stored there for assertion.
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
        // Read the full request (headers + Content-Length body) so the
        // client never sees a reset before the response is written.
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

/// Bind a mock server or fail the test with a clear message. Keeps the
/// happy-path tests readable without `expect`.
fn mock_base(response_body: String) -> String {
    match mock_server(200, response_body, None) {
        Some(base) => base,
        None => panic!("loopback mock failed to bind"),
    }
}

/// Bind a mock server answering with an HTTP error status (Groq error
/// envelope when `response_body` carries one).
fn mock_status(status: u16, response_body: String) -> String {
    match mock_server(status, response_body, None) {
        Some(base) => base,
        None => panic!("loopback mock failed to bind"),
    }
}

/// Bind a mock server that also captures the raw request body for
/// assertion. Returns the base URL and the capture buffer.
fn mock_server_capturing(
    status: u16,
    response_body: String,
) -> Option<(String, std::sync::Arc<std::sync::Mutex<Vec<u8>>>)> {
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let base = mock_server(status, response_body, Some(captured.clone()))?;
    Some((base, captured))
}

/// Read the captured request body as JSON, or fail the test.
fn captured_json(captured: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> serde_json::Value {
    let Ok(guard) = captured.lock() else {
        panic!("capture mutex is poisoned");
    };
    let Ok(body) = serde_json::from_slice(&guard) else {
        panic!("request body is not JSON");
    };
    body
}

#[test]
fn extract_domain_accepts_strict_json_shapes() {
    assert_eq!(
        extract_domain(r#"{"domain":"amazon.in"}"#).as_deref(),
        Some("amazon.in")
    );
    assert_eq!(
        extract_domain("  \n{\"domain\": \"amazon.in\"}  ").as_deref(),
        Some("amazon.in")
    );
    assert_eq!(
        extract_domain("```json\n{\"domain\":\"amazon.in\"}\n```").as_deref(),
        Some("amazon.in")
    );
    // Extra fields are tolerated; only `domain` is read.
    assert_eq!(
        extract_domain(r#"{"domain":"amazon.in","confidence":0.9}"#).as_deref(),
        Some("amazon.in")
    );
}

#[test]
fn extract_domain_salvages_decorated_json() {
    // Quoted JSON.
    assert_eq!(
        extract_domain(r#""{"domain": "amazon.in"}""#).as_deref(),
        Some("amazon.in")
    );
    // Prose wrapped around the JSON.
    assert_eq!(
        extract_domain("Here is the domain:\n{\"domain\": \"amazon.in\"}\nHope this helps!")
            .as_deref(),
        Some("amazon.in")
    );
    // Fence plus prose.
    assert_eq!(
        extract_domain("```json\n{\"domain\":\"amazon.in\"}\n```").as_deref(),
        Some("amazon.in")
    );
}

#[test]
fn extract_domain_rejects_everything_else() {
    assert_eq!(extract_domain("amazon.in"), None);
    assert_eq!(extract_domain(""), None);
    assert_eq!(extract_domain("not json at all"), None);
    assert_eq!(extract_domain(r#"{"url":"https://amazon.in"}"#), None);
    assert_eq!(extract_domain(r#"{"domain":42}"#), None);
    assert_eq!(extract_domain(r#"["amazon.in"]"#), None);
}

#[test]
fn groq_grounds_end_to_end_through_loopback() {
    // A canned Groq chat-completions envelope; the adapter must pull the
    // content out and the strict parser must yield the bare domain.
    let envelope = serde_json::json!({
        "choices": [{"message": {"content": "{\"domain\": \"amazon.in\"}"}}],
    })
    .to_string();
    let base = mock_base(envelope);
    let grounder = LlmDomainGrounder::groq("test-key", &base, "test-model");
    assert_eq!(
        grounder.ground_domain("amazon", "IN").as_deref(),
        Some("amazon.in")
    );
}

#[test]
fn groq_malformed_response_declines() {
    let base = mock_base("this is not json".to_owned());
    let grounder = LlmDomainGrounder::groq("test-key", &base, "test-model");
    assert_eq!(grounder.ground_domain("amazon", "IN"), None);
}

#[test]
fn groq_wrong_shape_declines_but_records_content() {
    // Valid JSON, but no usable `domain`: still a decline — and now the
    // raw model output is surfaced instead of dying silent.
    let envelope = serde_json::json!({
        "choices": [{"message": {"content": "{\"url\": \"https://amazon.in\"}"}}],
    })
    .to_string();
    let base = mock_base(envelope);
    let grounder = LlmDomainGrounder::groq("test-key", &base, "test-model");
    assert_eq!(grounder.ground_domain("amazon", "IN"), None);
    let Some(err) = grounder.last_error() else {
        panic!("unparseable content records a diagnostic");
    };
    assert!(err.contains("unparseable"), "unexpected detail: {err}");
    assert!(
        !err.contains("test-key"),
        "detail must never echo credentials"
    );
}

#[test]
fn groq_empty_object_records_abstention() {
    // The `{}` failure: valid JSON, but the model abstained instead of
    // grounding. Must decline with a diagnostic naming the abstention —
    // not the generic "unparseable" label.
    let envelope = serde_json::json!({
        "choices": [{"message": {"content": "{}"}}],
    })
    .to_string();
    let base = mock_base(envelope);
    let grounder = LlmDomainGrounder::groq("test-key", &base, "test-model");
    assert_eq!(grounder.ground_domain("x", "IN"), None);
    let Some(err) = grounder.last_error() else {
        panic!("empty object records a diagnostic");
    };
    assert!(err.contains("empty object"), "unexpected detail: {err}");
    assert!(
        !err.contains("test-key"),
        "detail must never echo credentials"
    );
}

#[test]
fn groq_empty_content_records_finish_reason() {
    // The gpt-oss empty-response failure: HTTP 200, empty string
    // content, finish_reason "length" (the reasoning trace consumed the
    // token budget). Must decline with a diagnostic naming the cause —
    // not a silent miss, and not a confusing "unparseable" preview.
    let envelope = serde_json::json!({
        "choices": [{"message": {"content": ""}, "finish_reason": "length"}],
    })
    .to_string();
    let base = mock_base(envelope);
    let grounder = LlmDomainGrounder::groq("test-key", &base, "openai/gpt-oss-20b");
    assert_eq!(grounder.ground_domain("amazon", "IN"), None);
    let Some(err) = grounder.last_error() else {
        panic!("empty content records a diagnostic");
    };
    assert!(
        err.contains("finish_reason: length"),
        "unexpected detail: {err}"
    );
    assert!(
        !err.contains("test-key"),
        "detail must never echo credentials"
    );
}

#[test]
fn groq_sends_low_reasoning_effort_for_gpt_oss() {
    let envelope = serde_json::json!({
        "choices": [{"message": {"content": "{\"domain\": \"amazon.in\"}"}}],
    })
    .to_string();
    let Some((base, captured)) = mock_server_capturing(200, envelope) else {
        panic!("loopback mock failed to bind");
    };
    let grounder = LlmDomainGrounder::groq("test-key", &base, "openai/gpt-oss-20b");
    assert_eq!(
        grounder.ground_domain("amazon", "IN").as_deref(),
        Some("amazon.in")
    );
    let body = captured_json(&captured);
    assert_eq!(body["reasoning_effort"], "low");
    assert_eq!(body["max_tokens"], 256);
}

#[test]
fn groq_omits_reasoning_effort_for_non_reasoning_models() {
    // reasoning_effort is a 400 on models that cannot reason; it must
    // never leak into requests for other models (e.g. a CLINCH_GROQ_MODEL
    // override).
    let envelope = serde_json::json!({
        "choices": [{"message": {"content": "{\"domain\": \"amazon.in\"}"}}],
    })
    .to_string();
    let Some((base, captured)) = mock_server_capturing(200, envelope) else {
        panic!("loopback mock failed to bind");
    };
    let grounder = LlmDomainGrounder::groq("test-key", &base, "llama-3.1-8b-instant");
    assert_eq!(
        grounder.ground_domain("amazon", "IN").as_deref(),
        Some("amazon.in")
    );
    let body = captured_json(&captured);
    assert!(
        body.get("reasoning_effort").is_none(),
        "reasoning_effort must not be sent to non-reasoning models"
    );
}

#[test]
fn groq_malformed_body_records_diagnostic() {
    let base = mock_base("this is not json".to_owned());
    let grounder = LlmDomainGrounder::groq("test-key", &base, "test-model");
    assert_eq!(grounder.ground_domain("amazon", "IN"), None);
    let Some(err) = grounder.last_error() else {
        panic!("malformed body records a diagnostic");
    };
    assert!(err.contains("malformed"), "unexpected detail: {err}");
    assert!(
        !err.contains("test-key"),
        "detail must never echo credentials"
    );
}

#[test]
fn groq_http_error_records_provider_code() {
    // The exact failure mode of a decommissioned model: HTTP 400 with
    // Groq's OpenAI-compatible error envelope.
    let envelope = serde_json::json!({
        "error": {
            "message": "The model `llama-3.1-8b-instant` has been decommissioned.",
            "type": "invalid_request_error",
            "code": "model_decommissioned",
        },
    })
    .to_string();
    let base = mock_status(400, envelope);
    let grounder = LlmDomainGrounder::groq("test-key", &base, "test-model");
    assert_eq!(grounder.ground_domain("amazon", "IN"), None);
    assert_eq!(
        grounder.last_error().as_deref(),
        Some("groq http 400 (model_decommissioned)")
    );
}

#[test]
fn groq_http_error_without_envelope_records_excerpt() {
    let base = mock_status(500, "upstream exploded".to_owned());
    let grounder = LlmDomainGrounder::groq("test-key", &base, "test-model");
    assert_eq!(grounder.ground_domain("amazon", "IN"), None);
    assert_eq!(
        grounder.last_error().as_deref(),
        Some("groq http 500 (upstream exploded)")
    );
}

#[test]
fn groq_transport_failure_records_kind() {
    // Unroutable port: connection refused before any byte is sent.
    let grounder = LlmDomainGrounder::groq("key", "http://127.0.0.1:9", "model");
    assert_eq!(grounder.ground_domain("amazon", "IN"), None);
    assert_eq!(
        grounder.last_error().as_deref(),
        Some("groq request failed (transport error)")
    );
}

#[test]
fn ollama_http_error_records_status() {
    let base = mock_status(500, "model not found".to_owned());
    let grounder = LlmDomainGrounder::ollama(&base, "test-model");
    assert_eq!(grounder.ground_domain("flipkart", "IN"), None);
    assert_eq!(
        grounder.last_error().as_deref(),
        Some("ollama http 500 (model not found)")
    );
}

#[test]
fn successful_call_leaves_no_diagnostic() {
    let envelope = serde_json::json!({
        "choices": [{"message": {"content": "{\"domain\": \"amazon.in\"}"}}],
    })
    .to_string();
    let base = mock_base(envelope);
    let grounder = LlmDomainGrounder::groq("test-key", &base, "test-model");
    assert_eq!(
        grounder.ground_domain("amazon", "IN").as_deref(),
        Some("amazon.in")
    );
    assert_eq!(grounder.last_error(), None);
}

#[test]
fn ollama_grounds_end_to_end_through_loopback() {
    let envelope = serde_json::json!({"response": "{\"domain\": \"flipkart.com\"}"}).to_string();
    let base = mock_base(envelope);
    let grounder = LlmDomainGrounder::ollama(&base, "test-model");
    assert_eq!(
        grounder.ground_domain("flipkart", "IN").as_deref(),
        Some("flipkart.com")
    );
}

#[test]
fn empty_site_name_never_touches_the_network() {
    // Point at an unroutable port: an empty slot must decline before
    // any socket is opened.
    let grounder = LlmDomainGrounder::groq("key", "http://127.0.0.1:9", "model");
    assert_eq!(grounder.ground_domain("   ", "IN"), None);
    let grounder = LlmDomainGrounder::ollama("http://127.0.0.1:9", "model");
    assert_eq!(grounder.ground_domain("", ""), None);
}

#[test]
fn verb_site_name_declines_without_network() {
    // A verb is never a site name: `open` must decline before any
    // socket is opened, so a parser slip can never ground
    // `open` → `open.com`. Unroutable port proves no request is made.
    let grounder = LlmDomainGrounder::groq("key", "http://127.0.0.1:9", "model");
    assert_eq!(grounder.ground_domain("open", "IN"), None);
    assert_eq!(grounder.last_error(), None);
    let grounder = LlmDomainGrounder::ollama("http://127.0.0.1:9", "model");
    assert_eq!(grounder.ground_domain("launch", "IN"), None);
    assert_eq!(grounder.last_error(), None);
}

#[test]
fn provider_value_parses_tolerantly() {
    assert_eq!(
        GrounderProvider::from_env_value("groq"),
        Some(GrounderProvider::Groq)
    );
    assert_eq!(
        GrounderProvider::from_env_value(" Ollama "),
        Some(GrounderProvider::Ollama)
    );
    assert_eq!(GrounderProvider::from_env_value(""), None);
    assert_eq!(GrounderProvider::from_env_value("openai"), None);
}

#[test]
fn from_env_values_is_absent_without_a_provider() {
    assert!(LlmDomainGrounder::from_env_values(&GrounderEnv::default()).is_none());
    assert!(
        LlmDomainGrounder::from_env_values(&GrounderEnv {
            provider: Some("openai".to_owned()),
            ..Default::default()
        })
        .is_none()
    );
}

#[test]
fn from_env_values_groq_requires_a_key() {
    // No key at all.
    assert!(
        LlmDomainGrounder::from_env_values(&GrounderEnv {
            provider: Some("groq".to_owned()),
            ..Default::default()
        })
        .is_none()
    );
    // A whitespace-only key is the same as a missing one.
    assert!(
        LlmDomainGrounder::from_env_values(&GrounderEnv {
            provider: Some("groq".to_owned()),
            groq_api_key: Some("   ".to_owned()),
            ..Default::default()
        })
        .is_none()
    );
    // A real key builds the adapter against the default base URL.
    let Some(grounder) = LlmDomainGrounder::from_env_values(&GrounderEnv {
        provider: Some("groq".to_owned()),
        groq_api_key: Some("test-key".to_owned()),
        ..Default::default()
    }) else {
        panic!("groq builds with a key");
    };
    assert_eq!(grounder.provider, GrounderProvider::Groq);
    assert_eq!(grounder.base_url, "https://api.groq.com/openai/v1");
    assert_eq!(grounder.model, "openai/gpt-oss-20b");
}

#[test]
fn from_env_values_ollama_needs_no_key_and_applies_defaults() {
    let Some(grounder) = LlmDomainGrounder::from_env_values(&GrounderEnv {
        provider: Some("ollama".to_owned()),
        ..Default::default()
    }) else {
        panic!("ollama builds keyless");
    };
    assert_eq!(grounder.provider, GrounderProvider::Ollama);
    assert_eq!(grounder.base_url, "http://localhost:11434");
    assert_eq!(grounder.model, "qwen2.5:1.5b");
}
