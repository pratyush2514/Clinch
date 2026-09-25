#![deny(unsafe_code)]
//! The real Tier 2B: an LLM-backed slot parser behind the fenced seam.
//!
//! Proves, without a model, the network, or a browser:
//!
//! 1. The two prompts that failed live now extract `site_context: "x"`
//!    instead of `target: "login"` / `target: "you"`.
//! 2. Hostile model output — URLs, selectors, code, invented actions,
//!    unknown keys — is declined after the call, by the existing fence.
//! 3. HTTP errors and stalled providers degrade to `None` inside the
//!    seam's bound instead of hanging the run.
//! 4. No configured provider means no adapter (the offline default).
//! 5. Wrapped prompts now report low grammar confidence, so the seam
//!    actually fires for them.

use orchestration_engine::{
    Confidence, GrounderEnv, IntentParser, LlmIntentParser, PARSER_TIMEOUT_MS, ParsedSlots,
    ResolutionContext, SlotSource, TestDoubleIntentParser, parse_grammar, parse_prompt_bounded,
    resolve_slots,
};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The two prompts from the live failures: buried open-verbs that the old
/// confidence check scored High, so the parser seam never ran.
const WRAPPED_ONE: &str = "can you open the X for me and make sure ill do the login first";
const WRAPPED_TWO: &str = "i want you to open X for me";

/// How the loopback mock behaves for one accepted connection.
enum MockBehavior {
    /// Answer with an HTTP status and body.
    Respond { status: u16, body: String },
    /// Accept the connection and never answer within `delay` — the seam's
    /// bound must abandon the call first.
    Stall { delay: Duration },
}

/// Groq chat-completions envelope carrying `content` as the assistant
/// message.
fn chat_envelope(content: &str) -> String {
    serde_json::json!({
        "choices": [{"message": {"content": content}}],
    })
    .to_string()
}

/// Loopback mock answering one HTTP request, then exiting. Hermetic: no
/// internet, no fixed port. `None` when the loopback bind itself fails.
/// Returns the base URL to point the adapter at plus the raw request bytes
/// (head + body) for assertion.
fn mock_server(behavior: MockBehavior) -> Option<(String, Arc<Mutex<Vec<u8>>>)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    let addr = listener.local_addr().ok()?;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_thread = captured.clone();
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
        if let Ok(mut guard) = captured_thread.lock() {
            guard.extend_from_slice(&buf);
        }
        match behavior {
            MockBehavior::Stall { delay } => {
                std::thread::sleep(delay);
                // Answer after the stall so a slow-but-alive provider is
                // distinguishable from a dead one; the seam abandons the
                // call long before this arrives.
                let body = chat_envelope(
                    r#"{"action":"link","artifact_noun":null,"site_context":"late"}"#,
                );
                let response = format!(
                    "HTTP/1.1 200 Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
            MockBehavior::Respond { status, body } => {
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        }
    });
    Some((format!("http://{addr}"), captured))
}

/// Bind a mock answering 200 with a chat envelope, or fail the test with a
/// clear message.
fn mock_chat(content: &str) -> (String, Arc<Mutex<Vec<u8>>>) {
    let Some(mock) = mock_server(MockBehavior::Respond {
        status: 200,
        body: chat_envelope(content),
    }) else {
        panic!("loopback mock failed to bind");
    };
    mock
}

/// Split captured raw request bytes into the head (headers) and the parsed
/// JSON body.
fn request_parts(captured: &Arc<Mutex<Vec<u8>>>) -> (String, serde_json::Value) {
    let Ok(guard) = captured.lock() else {
        panic!("capture mutex is not poisoned");
    };
    let raw = String::from_utf8_lossy(&guard);
    let Some((head, body)) = raw.split_once("\r\n\r\n") else {
        panic!("request has a head and body");
    };
    let Ok(body) = serde_json::from_str::<serde_json::Value>(body) else {
        panic!("request body is JSON");
    };
    (head.to_owned(), body)
}

/// A parser wired to a loopback mock that answers `content` as the model's
/// strict-JSON payload.
fn parser_answering(content: &str) -> (LlmIntentParser, Arc<Mutex<Vec<u8>>>) {
    let (base, captured) = mock_chat(content);
    (
        LlmIntentParser::groq("test-key", &base, "test-model"),
        captured,
    )
}

#[test]
fn failing_prompts_extract_the_site_slot() {
    // The live failures: grammar read `target="login"` and `target="you"`
    // with High confidence. The model answers with the real site slot, and
    // the adapter extracts it.
    for (prompt, expected_site) in [(WRAPPED_ONE, "x"), (WRAPPED_TWO, "x")] {
        let (parser, _captured) =
            parser_answering(r#"{"action":"link","artifact_noun":null,"site_context":"x"}"#);
        let Some(slots) = parser.parse_prompt(prompt) else {
            panic!("{prompt} yields slots");
        };
        assert_eq!(slots.action, "link");
        assert_eq!(slots.artifact_noun, None);
        assert_eq!(slots.site_context.as_deref(), Some(expected_site));
    }
}

#[test]
fn natural_variants_extract_action_artifact_and_site() {
    for (prompt, content, action, artifact, site) in [
        (
            "could you show me the billing dashboard on stripe",
            r#"{"action":"link","artifact_noun":"dashboard","site_context":"stripe"}"#,
            "link",
            Some("dashboard"),
            Some("stripe"),
        ),
        (
            "i need to check my inbox on gmail",
            r#"{"action":"link","artifact_noun":"inbox","site_context":"gmail"}"#,
            "link",
            Some("inbox"),
            Some("gmail"),
        ),
        (
            "please pull up the invoice for acmecorp",
            r#"{"action":"link","artifact_noun":"invoice","site_context":"acmecorp"}"#,
            "link",
            Some("invoice"),
            Some("acmecorp"),
        ),
    ] {
        let (parser, _captured) = parser_answering(content);
        let Some(slots) = parser.parse_prompt(prompt) else {
            panic!("{prompt} yields slots");
        };
        assert_eq!(slots.action, action);
        assert_eq!(slots.artifact_noun.as_deref(), artifact);
        assert_eq!(slots.site_context.as_deref(), site);
    }
}

#[test]
fn hostile_payloads_are_declined_by_the_fence() {
    // Each payload is a capability-escalation attempt: the adapter returns
    // raw slots and `parse_prompt_bounded` applies `sanitized()` on the way
    // back, so every one of these must read as a decline.
    for content in [
        // URL in the site slot.
        r#"{"action":"link","artifact_noun":null,"site_context":"https://evil.example/x"}"#,
        // Selector in the artifact slot.
        r#"{"action":"link","artifact_noun":"a.btn > span","site_context":null}"#,
        // Code in the artifact slot.
        r#"{"action":"link","artifact_noun":"document.querySelector('a')","site_context":null}"#,
        // Multi-word phrase in the site slot.
        r#"{"action":"link","artifact_noun":null,"site_context":"two words"}"#,
        // Unknown key: the contract is exactly three keys.
        r#"{"action":"link","artifact_noun":null,"site_context":"x","url":"https://x.example"}"#,
        // Invented action: not in the closed INTENT_ROLES vocabulary.
        r#"{"action":"download","artifact_noun":null,"site_context":"x"}"#,
        // Missing action key.
        r#"{"artifact_noun":null,"site_context":"x"}"#,
        // Not JSON at all.
        "just some prose",
        // Empty content.
        "",
    ] {
        let (parser, _captured) = parser_answering(content);
        let parser: Arc<dyn IntentParser> = Arc::new(parser);
        assert_eq!(
            parse_prompt_bounded(&parser, "open something"),
            None,
            "hostile payload declined: {content}"
        );
    }
}

#[test]
fn http_error_declines() {
    let Some((base, _captured)) = mock_server(MockBehavior::Respond {
        status: 500,
        body: "upstream exploded".to_owned(),
    }) else {
        panic!("loopback mock failed to bind");
    };
    let parser = LlmIntentParser::groq("test-key", &base, "test-model");
    assert_eq!(parser.parse_prompt("open something"), None);
}

#[test]
fn stalled_provider_degrades_within_the_seam_bound() {
    // The provider accepts the connection and never answers: the seam's
    // 1.5s bound must abandon the call, not the adapter's 15s backstop.
    let Some((base, _captured)) = mock_server(MockBehavior::Stall {
        delay: Duration::from_secs(10),
    }) else {
        panic!("loopback mock failed to bind");
    };
    let parser: Arc<dyn IntentParser> =
        Arc::new(LlmIntentParser::groq("test-key", &base, "test-model"));
    let started = std::time::Instant::now();
    assert_eq!(parse_prompt_bounded(&parser, "open something"), None);
    assert!(
        started.elapsed() < Duration::from_millis(PARSER_TIMEOUT_MS * 3),
        "bounded wait, took {:?}",
        started.elapsed()
    );
}

#[test]
fn empty_prompt_never_touches_the_network() {
    // Point at an unroutable port: an empty prompt must decline before any
    // socket is opened.
    let parser = LlmIntentParser::groq("test-key", "http://127.0.0.1:9", "test-model");
    assert_eq!(parser.parse_prompt("   "), None);
}

#[test]
fn groq_request_carries_the_prompt_and_not_the_key() {
    let (base, captured) =
        mock_chat(r#"{"action":"link","artifact_noun":null,"site_context":"x"}"#);
    // Empty model falls back to the default; the request must name it.
    let parser = LlmIntentParser::groq("test-key", &base, "");
    let Some(slots) = parser.parse_prompt(WRAPPED_TWO) else {
        panic!("mock answers");
    };
    assert_eq!(slots.site_context.as_deref(), Some("x"));
    let (head, body) = request_parts(&captured);
    assert_eq!(body["model"], "openai/gpt-oss-20b");
    assert_eq!(body["temperature"], 0);
    assert_eq!(body["reasoning_effort"], "low");
    let Some(messages) = body["messages"].as_array() else {
        panic!("messages is an array");
    };
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "user");
    assert!(
        messages[1]["content"]
            .as_str()
            .is_some_and(|content| content.contains(WRAPPED_TWO)),
        "the raw prompt travels in the user message"
    );
    // The key travels only in the Authorization header, never the body.
    assert!(
        head.to_ascii_lowercase()
            .contains("authorization: bearer test-key"),
        "key is a Bearer header"
    );
    assert!(
        !body.to_string().contains("test-key"),
        "key must not appear in the body"
    );
}

#[test]
fn ollama_adapter_answers_through_loopback() {
    let body = serde_json::json!({"response": r#"{"action":"link","artifact_noun":"inbox","site_context":"gmail"}"#}).to_string();
    let Some((base, captured)) = mock_server(MockBehavior::Respond { status: 200, body }) else {
        panic!("loopback mock failed to bind");
    };
    let parser = LlmIntentParser::ollama(&base, "test-model");
    let Some(slots) = parser.parse_prompt("i need to check my inbox on gmail") else {
        panic!("mock answers");
    };
    assert_eq!(slots.action, "link");
    assert_eq!(slots.artifact_noun.as_deref(), Some("inbox"));
    assert_eq!(slots.site_context.as_deref(), Some("gmail"));
    let (head, _body) = request_parts(&captured);
    assert!(head.contains("POST /api/generate"), "ollama endpoint");
}

#[test]
fn from_env_values_needs_a_provider_and_a_key() {
    // Nothing configured: no adapter, the stub stays.
    assert!(LlmIntentParser::from_env_values(&GrounderEnv::default()).is_none());
    // groq without a key is the same as unconfigured.
    assert!(
        LlmIntentParser::from_env_values(&GrounderEnv {
            provider: Some("groq".to_owned()),
            ..Default::default()
        })
        .is_none()
    );
    assert!(
        LlmIntentParser::from_env_values(&GrounderEnv {
            provider: Some("groq".to_owned()),
            groq_api_key: Some("   ".to_owned()),
            ..Default::default()
        })
        .is_none()
    );
    // groq with a key builds; ollama builds keyless.
    assert!(
        LlmIntentParser::from_env_values(&GrounderEnv {
            provider: Some("groq".to_owned()),
            groq_api_key: Some("test-key".to_owned()),
            ..Default::default()
        })
        .is_some()
    );
    assert!(
        LlmIntentParser::from_env_values(&GrounderEnv {
            provider: Some("ollama".to_owned()),
            ..Default::default()
        })
        .is_some()
    );
    // Unknown provider: no adapter.
    assert!(
        LlmIntentParser::from_env_values(&GrounderEnv {
            provider: Some("openai".to_owned()),
            ..Default::default()
        })
        .is_none()
    );
}

#[test]
fn wrapped_prompts_report_low_grammar_confidence() {
    // The positional fix: a verb buried mid-sentence no longer earns the
    // fast path, so these reach the parser seam instead of misparsing.
    for prompt in [WRAPPED_ONE, WRAPPED_TWO] {
        assert_eq!(
            parse_grammar(prompt, None).confidence,
            Confidence::Low,
            "{prompt} is not verb-headed"
        );
    }
    // Crisp imperatives keep the fast path — the fix must not demote them.
    for prompt in [
        "open amazon for me",
        "please open amazon",
        "download all my invoices from github",
        "click pay now",
    ] {
        assert_eq!(
            parse_grammar(prompt, None).confidence,
            Confidence::High,
            "{prompt} stays on the fast path"
        );
    }
}

#[test]
fn parser_tier_fires_for_the_failing_prompts() {
    // End to end through `resolve_slots` with the deterministic test
    // double: wrapped prompts consult the seam exactly once and adopt its
    // slots.
    for prompt in [WRAPPED_ONE, WRAPPED_TWO] {
        let double = Arc::new(TestDoubleIntentParser::answering(ParsedSlots {
            action: "link".into(),
            artifact_noun: None,
            site_context: Some("x".into()),
        }));
        let parser: Arc<dyn IntentParser> = double.clone();
        let ctx = ResolutionContext {
            account_dir: None,
            llm: None,
            parser: Some(&parser),
            shortcuts: None,
            site_search: None,
            domain_grounder: None,
            region_hint: "",
        };
        let resolved = resolve_slots(prompt, None, &ctx);
        assert_eq!(resolved.source, SlotSource::IntentParser, "{prompt}");
        assert_eq!(double.calls(), 1, "exactly one bounded shot");
        assert_eq!(resolved.grammar.site_context.as_deref(), Some("x"));
        assert_eq!(resolved.grammar.confidence, Confidence::High);
    }
}
