//! Worker-level semantic integration: the classifier paths behind the
//! chrome worker's revealed fallback and opener ambiguity gate, driven
//! through [`SemanticMatcher::new`]'s `CLINCH_CLASSIFIER_BASE_URL`
//! override against a loopback mock classifier — the same seam the worker
//! itself uses, since the worker cannot inject a matcher.
//!
//! One test, one server, sequential scenarios: the env override is
//! process-global, so everything runs inside a single `#[tokio::test]`
//! and the var is restored afterwards. No internet, no fixed port.
//!
//! Edition 2024 marks `std::env::set_var` unsafe (it can race other
//! threads); the override is the worker's seam under test, so the
//! `unsafe` is allowed for this file only — the scenarios are the sole
//! env-mutating tests in the process.
#![allow(unsafe_code)]

use browser_driver::AxElement;
use macro_engine::semantic::{SEMANTIC_ACCEPT, SemanticMatcher};
use macro_engine::{ClickedControl, semantic_opener_winner, semantic_revealed_target};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const CLASSIFIER_ENV: &str = "CLINCH_CLASSIFIER_BASE_URL";

fn el(id: i64, role: &str, name: &str) -> AxElement {
    AxElement {
        backend_node_id: id,
        role: role.to_string(),
        name: name.to_string(),
        description: String::new(),
        container_text: Vec::new(),
        landmark: None,
    }
}

/// Loopback classifier: answers `POST /v1/classify` with a per-input
/// score looked up from `scores` (normalized input → score, default
/// 0.05), echoing the request's first label as the action label.
struct MockClassifier {
    base: String,
    hits: Arc<AtomicUsize>,
}

fn serve_classifier(scores: HashMap<String, f64>, max_requests: usize) -> MockClassifier {
    let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
        panic!("loopback classifier failed to bind");
    };
    let Ok(addr) = listener.local_addr() else {
        panic!("loopback classifier has no address");
    };
    let hits = Arc::new(AtomicUsize::new(0));
    let thread_hits = hits.clone();
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
            let payload: serde_json::Value =
                serde_json::from_slice(&buf[header_end..]).unwrap_or_default();
            let input = payload
                .get("input")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let action = payload
                .get("labels")
                .and_then(|value| value.as_array())
                .and_then(|labels| labels.first())
                .and_then(|value| value.as_str())
                .unwrap_or("action");
            let key = input.trim().to_lowercase();
            let score = scores.get(&key).copied().unwrap_or(0.05);
            let body = serde_json::json!({
                "results": [{
                    "label": action,
                    "scores": { action: score, "unrelated control": 1.0 - score },
                }],
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    MockClassifier {
        base: format!("http://{addr}"),
        hits,
    }
}

#[tokio::test]
async fn worker_semantic_paths_via_loopback_classifier() {
    let mut scores = HashMap::new();
    scores.insert("configure things".to_owned(), 0.92);
    scores.insert("copy link".to_owned(), 0.1);
    scores.insert("share".to_owned(), 0.15);
    scores.insert("your profile".to_owned(), 0.9);
    scores.insert("notifications".to_owned(), 0.2);
    scores.insert("first avatar".to_owned(), 0.9);
    scores.insert("second avatar".to_owned(), 0.95);
    let server = serve_classifier(scores, 16);
    let previous = std::env::var(CLASSIFIER_ENV).ok();
    unsafe { std::env::set_var(CLASSIFIER_ENV, &server.base) };

    // Scenario 1: high-score revealed pick. The vocabulary matcher found
    // nothing, but the worker opened something (`clicked` non-empty) and a
    // genuinely-new menu item scores above threshold.
    let mut verb_matcher = SemanticMatcher::new("settings");
    let opener = el(1, "button", "Open user actions");
    let elements = vec![opener.clone(), el(10, "menuitem", "Configure things")];
    let clicked = vec![ClickedControl::of(&opener)];
    let mut tried = Vec::new();
    let picked = semantic_revealed_target(
        &mut verb_matcher,
        &elements,
        &clicked,
        &HashSet::new(),
        &mut tried,
    )
    .await;
    assert_eq!(
        picked.map(|element| element.backend_node_id),
        Some(10),
        "the high-score menu item is picked"
    );
    assert!(
        tried
            .iter()
            .any(|line| line.contains("semantic match 'Configure things'")),
        "the pick is journaled: {tried:?}"
    );

    // Scenario 2: every candidate below threshold — the fallback misses
    // and leaves the journal untouched.
    let elements = vec![
        opener.clone(),
        el(10, "menuitem", "Copy link"),
        el(11, "menuitem", "Share"),
    ];
    let mut tried = Vec::new();
    let picked = semantic_revealed_target(
        &mut verb_matcher,
        &elements,
        &clicked,
        &HashSet::new(),
        &mut tried,
    )
    .await;
    assert!(picked.is_none(), "all-below-threshold is a miss");
    assert!(tried.is_empty(), "a miss journals nothing: {tried:?}");

    // Scenario 3: opener disambiguation — one winner above threshold.
    let mut opener_matcher = SemanticMatcher::new("account menu");
    let openers = [
        el(1, "button", "Your profile"),
        el(2, "button", "Notifications"),
    ];
    let refs: Vec<&AxElement> = openers.iter().collect();
    let winner = semantic_opener_winner(&mut opener_matcher, &refs).await;
    match winner {
        Some((element, score)) => {
            assert_eq!(element.backend_node_id, 1);
            assert!(
                score >= SEMANTIC_ACCEPT,
                "the winner clears the accept threshold: {score}"
            );
        }
        None => panic!("expected the high-score opener to win"),
    }

    // Scenario 4: two above threshold is still ambiguous.
    let openers = [
        el(3, "button", "First avatar"),
        el(4, "button", "Second avatar"),
    ];
    let refs: Vec<&AxElement> = openers.iter().collect();
    assert!(
        semantic_opener_winner(&mut opener_matcher, &refs)
            .await
            .is_none(),
        "two above threshold stays ambiguous"
    );

    assert!(
        server.hits.load(Ordering::SeqCst) > 0,
        "the scenarios actually hit the loopback classifier"
    );
    match previous {
        Some(value) => unsafe { std::env::set_var(CLASSIFIER_ENV, value) },
        None => unsafe { std::env::remove_var(CLASSIFIER_ENV) },
    }
}
