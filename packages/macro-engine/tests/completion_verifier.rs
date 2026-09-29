#![deny(unsafe_code)]
//! Step 5: `Completed` means the verifier observed the goal state.
//! Batch dispatch failures are honest partial failures; a generic noun
//! landing needs a positive noun signal, not just a URL change.

use browser_driver::{AxElement, BrowserError, Highlight, Mark};
use macro_engine::{
    BatchDriver, CLICK_FAILED_REASON, ExecuteOutcome, IntentError, IntentOutcome, click_batch_with,
    verify_noun_landing,
};
use std::sync::Mutex;
use url::Url;

fn element(name: &str) -> AxElement {
    AxElement {
        backend_node_id: 1,
        role: "button".into(),
        name: name.into(),
        description: String::new(),
        container_text: Vec::new(),
        landmark: None,
    }
}

fn heading(name: &str) -> AxElement {
    AxElement {
        role: "heading".into(),
        ..element(name)
    }
}

struct FakeDriver {
    fail_on: Option<&'static str>,
    clicked: Mutex<Vec<String>>,
}

impl BatchDriver for FakeDriver {
    async fn batch_current_url(&self) -> Result<Option<Url>, IntentError> {
        Ok(Some(Url::parse("https://example.com/list").unwrap()))
    }

    async fn batch_click(&self, element: &AxElement) -> Result<IntentOutcome, IntentError> {
        if self.fail_on == Some(element.name.as_str()) {
            return Err(IntentError::Browser(BrowserError::WrongOrigin));
        }
        self.clicked.lock().unwrap().push(element.name.clone());
        Ok(IntentOutcome {
            mark: Mark {
                index: 0,
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
            highlight: Highlight {
                selector: String::new(),
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
                matches: 1,
            },
            hit_line: format!("click_hit_test: button '{}' → ok", element.name),
        })
    }
}

#[tokio::test]
async fn failed_click_is_a_partial_failure_naming_the_click() {
    let driver = FakeDriver {
        fail_on: Some("Two"),
        clicked: Mutex::new(Vec::new()),
    };
    let batch = [element("One"), element("Two"), element("Three")];
    let outcome = click_batch_with(&driver, &batch).await.unwrap();
    assert_eq!(
        outcome,
        ExecuteOutcome::HaltedEarly {
            reason: CLICK_FAILED_REASON,
            clicks_completed: 1,
            failed_candidate_index: 1,
            failed_candidate_label: "Two".into(),
            diverged_url: String::new(),
        }
    );
    // Nothing after the failed click was attempted.
    assert_eq!(*driver.clicked.lock().unwrap(), ["One"]);
}

#[tokio::test]
async fn fully_dispatched_batch_completes_with_per_click_evidence() {
    let driver = FakeDriver {
        fail_on: None,
        clicked: Mutex::new(Vec::new()),
    };
    let batch = [element("One"), element("Two")];
    let ExecuteOutcome::Completed(clicks) = click_batch_with(&driver, &batch).await.unwrap() else {
        panic!("expected Completed");
    };
    assert_eq!(clicks.len(), 2);
    assert!(
        clicks
            .iter()
            .all(|c| c.hit_line.starts_with("click_hit_test:"))
    );
}

#[test]
fn noun_navigation_without_a_noun_signal_is_not_verified() {
    let origin = Url::parse("https://example.com/").unwrap();
    // The click changed the URL, but nothing on the page names the noun.
    let landed = Url::parse("https://example.com/home/feed").unwrap();
    assert!(!verify_noun_landing(
        "billing",
        &landed,
        &origin,
        Some("Your feed"),
        &[heading("Trending")]
    ));
}

#[test]
fn noun_landing_verifies_on_path_title_or_heading_and_same_site() {
    let origin = Url::parse("https://example.com/").unwrap();
    let by_path = Url::parse("https://example.com/account/billing").unwrap();
    assert!(verify_noun_landing("billing", &by_path, &origin, None, &[]));
    let plain = Url::parse("https://example.com/x").unwrap();
    assert!(verify_noun_landing(
        "billing",
        &plain,
        &origin,
        Some("Billing history"),
        &[]
    ));
    assert!(verify_noun_landing(
        "billing",
        &plain,
        &origin,
        None,
        &[heading("Billing")]
    ));
    // Off-site never verifies, even with the noun in the path.
    let off = Url::parse("https://other.example.org/billing").unwrap();
    assert!(!verify_noun_landing("billing", &off, &origin, None, &[]));
}
