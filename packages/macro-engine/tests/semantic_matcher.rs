//! `SemanticMatcher` through its public surface: request shape against the
//! classifier.dev contract, response parsing (including malformed and empty
//! bodies), per-run caching, failure/timeout posture, name truncation, and
//! the privacy fence (only the candidate name + two labels leave the
//! machine).
//!
//! Hermetic: every HTTP test points the matcher at a loopback mock — no
//! internet, no fixed port.

use macro_engine::semantic::{SEMANTIC_ACCEPT, SemanticMatcher};
use std::io::{Read, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

/// A loopback mock answering up to `max_requests` HTTP requests with
/// `status` + `body`, counting hits and capturing raw requests (head +
/// body) for assertion.
struct MockServer {
    base: String,
    hits: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
}

fn serve(status: u16, body: String, max_requests: usize) -> MockServer {
    let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
        panic!("loopback mock failed to bind");
    };
    let Ok(addr) = listener.local_addr() else {
        panic!("loopback mock has no address");
    };
    let hits = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let thread_hits = hits.clone();
    let thread_requests = requests.clone();
    std::thread::spawn(move || {
        for _ in 0..max_requests {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            thread_hits.fetch_add(1, Ordering::SeqCst);
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(n) = stream.read(&mut chunk) {
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
                    break;
                };
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                body_read += n;
            }
            if let Ok(mut guard) = thread_requests.lock() {
                guard.push(buf);
            }
            let response = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    MockServer {
        base: format!("http://{addr}"),
        hits,
        requests,
    }
}

/// Split one captured raw request into (request line, JSON body).
fn request_parts(raw: &[u8]) -> (String, serde_json::Value) {
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(raw.len(), |pos| pos + 4);
    let head = String::from_utf8_lossy(&raw[..header_end]);
    let request_line = head.lines().next().unwrap_or("").to_owned();
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(&raw[header_end..]) else {
        panic!("mock received a non-JSON request body");
    };
    (request_line, body)
}

fn matcher_with(base: &str) -> SemanticMatcher {
    SemanticMatcher::with_base_url("log out", base)
}

fn classify_ok(action_score: f64, other_score: f64) -> String {
    serde_json::json!({
        "results": [{
            "label": "log out action",
            "confidence": action_score,
            "scores": {
                "log out action": action_score,
                "unrelated control": other_score,
            },
        }],
        "tier": "fast",
        "model": "jev",
    })
    .to_string()
}

fn assert_approx(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() < 1e-6,
        "expected ~{expected}, got {actual}"
    );
}

// --- Request shape: the classifier.dev contract ---

#[tokio::test]
async fn request_body_matches_classifier_dev_contract() {
    let server = serve(200, classify_ok(0.9, 0.1), 4);
    let mut matcher = matcher_with(&server.base);
    let _ = matcher.score("Sign out").await;

    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    let Ok(requests) = server.requests.lock() else {
        panic!("request capture poisoned");
    };
    assert_eq!(requests.len(), 1);
    let (request_line, body) = request_parts(&requests[0]);

    assert!(
        request_line.starts_with("POST /v1/classify "),
        "unexpected request line: {request_line}"
    );
    assert_eq!(body["input"], serde_json::json!("Sign out"));
    assert_eq!(
        body["labels"],
        serde_json::json!(["log out action", "unrelated control"])
    );
    assert_eq!(body["tier"], serde_json::json!("fast"));
    // Privacy fence: nothing but the candidate name and the two labels
    // (plus the fixed tier) crosses the wire — no URLs, no page text.
    let Some(object) = body.as_object() else {
        panic!("request body is not a JSON object");
    };
    let keys: Vec<&str> = object.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["input", "labels", "tier"]);
}

// --- Response parsing ---

#[tokio::test]
async fn scores_map_drives_the_returned_score() {
    let server = serve(200, classify_ok(0.82, 0.18), 4);
    let mut matcher = matcher_with(&server.base);
    let score = matcher.score("Log out").await;
    match score {
        Some(score) => assert_approx(score, 0.82),
        None => panic!("expected a score, got a decline"),
    }
}

#[tokio::test]
async fn scores_beat_confidence_when_they_disagree() {
    let body = serde_json::json!({
        "results": [{
            "label": "log out action",
            "confidence": 0.40,
            "scores": {"log out action": 0.81, "unrelated control": 0.19},
        }],
    })
    .to_string();
    let server = serve(200, body, 4);
    let mut matcher = matcher_with(&server.base);
    let score = matcher.score("Sign out").await;
    match score {
        Some(score) => assert_approx(score, 0.81),
        None => panic!("expected a score, got a decline"),
    }
}

#[tokio::test]
async fn confidence_fallback_when_scores_are_missing() {
    let body = serde_json::json!({
        "results": [{"label": "log out action", "confidence": 0.66}],
    })
    .to_string();
    let server = serve(200, body, 4);
    let mut matcher = matcher_with(&server.base);
    let score = matcher.score("Sign out").await;
    match score {
        Some(score) => assert_approx(score, 0.66),
        None => panic!("expected a score, got a decline"),
    }
}

#[tokio::test]
async fn losing_label_still_returns_its_own_low_score() {
    let body = serde_json::json!({
        "results": [{
            "label": "unrelated control",
            "confidence": 0.91,
            "scores": {"log out action": 0.09, "unrelated control": 0.91},
        }],
    })
    .to_string();
    let server = serve(200, body, 4);
    let mut matcher = matcher_with(&server.base);
    let score = matcher.score("Home").await;
    match score {
        Some(score) => assert_approx(score, 0.09),
        None => panic!("expected a low score, got a decline"),
    }
}

#[tokio::test]
async fn malformed_and_empty_responses_decline() {
    for body in [
        "this is not json".to_owned(),
        "{}".to_owned(),
        r#"{"results": []}"#.to_owned(),
        r#"{"results": [{"label": "unrelated control"}]}"#.to_owned(),
        r#"{"results": [{"label": "log out action"}]}"#.to_owned(),
        r#"{"error": "boom", "code": "internal"}"#.to_owned(),
    ] {
        let server = serve(200, body, 4);
        let mut matcher = matcher_with(&server.base);
        assert!(
            matcher.score("Sign out").await.is_none(),
            "malformed response should decline silently"
        );
    }
}

// --- Cache ---

#[tokio::test]
async fn cache_hit_avoids_a_second_network_call() {
    let server = serve(200, classify_ok(0.8, 0.2), 16);
    let mut matcher = matcher_with(&server.base);

    let first = matcher.score("Sign out").await;
    let second = matcher.score("Sign out").await;
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    match (first, second) {
        (Some(first), Some(second)) => {
            assert_approx(first, 0.8);
            assert_approx(second, 0.8);
        }
        _ => panic!("expected two cached scores"),
    }
}

#[tokio::test]
async fn normalization_collapses_cosmetic_name_differences() {
    let server = serve(200, classify_ok(0.7, 0.3), 16);
    let mut matcher = matcher_with(&server.base);

    let _ = matcher.score("  Sign   OUT ").await;
    let _ = matcher.score("sign out").await;
    let _ = matcher.score("SIGN OUT").await;
    assert_eq!(
        server.hits.load(Ordering::SeqCst),
        1,
        "whitespace/case variants must share one cache entry"
    );
}

#[tokio::test]
async fn distinct_names_hit_the_network_independently() {
    let server = serve(200, classify_ok(0.7, 0.3), 16);
    let mut matcher = matcher_with(&server.base);

    let _ = matcher.score("Sign out").await;
    let _ = matcher.score("Settings").await;
    assert_eq!(server.hits.load(Ordering::SeqCst), 2);
}

// --- Failure posture: silent decline, never a panic ---

#[tokio::test]
async fn http_failures_decline_silently() {
    for status in [500u16, 429, 400] {
        let server = serve(status, r#"{"error": "x", "code": "y"}"#.to_owned(), 4);
        let mut matcher = matcher_with(&server.base);
        assert!(
            matcher.score("Sign out").await.is_none(),
            "HTTP {status} should decline silently"
        );
    }
}

#[tokio::test]
async fn connection_refused_declines_silently() {
    // Port 1 is never open: instant transport failure, no panic.
    let mut matcher = SemanticMatcher::with_base_url("log out", "http://127.0.0.1:1");
    assert!(matcher.score("Sign out").await.is_none());
}

#[tokio::test]
async fn timeout_declines_silently() {
    let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
        panic!("hanging mock failed to bind");
    };
    let Ok(addr) = listener.local_addr() else {
        panic!("hanging mock has no address");
    };
    std::thread::spawn(move || {
        // Accept and never answer: the client's ~5s timeout must fire.
        if let Ok((stream, _)) = listener.accept() {
            std::thread::sleep(Duration::from_secs(10));
            drop(stream);
        }
    });

    let mut matcher = SemanticMatcher::with_base_url("log out", &format!("http://{addr}"));
    let started = tokio::time::Instant::now();
    let score = matcher.score("Sign out").await;
    let elapsed = started.elapsed();
    assert!(score.is_none(), "a hung server must decline, not hang");
    assert!(
        elapsed < Duration::from_secs(10),
        "the call must be bounded, took {elapsed:?}"
    );
}

#[tokio::test]
async fn blank_name_or_hint_makes_no_request() {
    let server = serve(200, classify_ok(0.9, 0.1), 16);
    let mut matcher = matcher_with(&server.base);
    assert!(matcher.score("").await.is_none());
    assert!(matcher.score("   ").await.is_none());
    assert_eq!(server.hits.load(Ordering::SeqCst), 0);

    let mut blank_hint = SemanticMatcher::with_base_url("   ", &server.base);
    assert!(blank_hint.score("Sign out").await.is_none());
    assert_eq!(server.hits.load(Ordering::SeqCst), 0);
}

// --- Truncation ---

#[tokio::test]
async fn name_truncated_to_120_chars() {
    let server = serve(200, classify_ok(0.9, 0.1), 4);
    let mut matcher = matcher_with(&server.base);
    let _ = matcher.score(&"x".repeat(200)).await;

    let Ok(requests) = server.requests.lock() else {
        panic!("request capture poisoned");
    };
    assert_eq!(requests.len(), 1);
    let (_, body) = request_parts(&requests[0]);
    let Some(input) = body["input"].as_str() else {
        panic!("request input is not a string");
    };
    assert_eq!(input.chars().count(), 120);
}

// --- Constants ---

#[test]
fn semantic_accept_threshold_is_conservative() {
    assert!((SEMANTIC_ACCEPT - 0.75).abs() < f32::EPSILON);
}
