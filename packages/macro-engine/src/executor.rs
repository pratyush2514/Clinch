#![deny(unsafe_code)]
//! Semantic intent execution over live AX snapshots with Set-of-Marks clicks.
//!
//! Playbooks describe *what* they want (`{role, label}`) instead of *where it
//! is* (selectors). At runtime the intent resolves against the current AX
//! snapshot by deterministic text similarity — exact beats prefix beats
//! containment — and the winner is acted on through its backend node id
//! (rect resolution plus coordinate click). No selector is ever constructed,
//! stored, or repaired here.

use browser_driver::{AxElement, Highlight, ManagedBrowser, Mark};
use serde::{Deserialize, Serialize};

/// What a Playbook step wants, in words. Both fields are required: a bare
/// role with several live matches is ambiguous and resolves to nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticIntent {
    pub role: String,
    pub label_query: String,
}

impl SemanticIntent {
    /// Shared bounds so schema validation and execution agree on what a
    /// runnable intent looks like. `pub` for the playbook schema only.
    ///
    /// # Errors
    /// Returns [`browser_driver::BrowserError::InvalidAction`] for empty or
    /// oversized roles and queries.
    pub fn validate(&self) -> Result<(String, String), browser_driver::BrowserError> {
        let role = normalize(&self.role);
        let query = normalize(&self.label_query);
        if role.is_empty() || query.is_empty() || role.len() > 64 || query.len() > 512 {
            return Err(browser_driver::BrowserError::InvalidAction);
        }
        Ok((role, query))
    }
}

/// A resolved intent: the winning element plus its match strength
/// (3 exact, 2 prefix, 1 containment) for planners and previews.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedIntent {
    pub element: AxElement,
    pub score: u8,
}

/// What an executed intent acted on: the badge shown and the rect clicked.
#[derive(Clone, Debug, PartialEq)]
pub struct IntentOutcome {
    pub mark: Mark,
    pub highlight: Highlight,
}

#[derive(Debug, thiserror::Error)]
pub enum IntentError {
    #[error("No live control matches this intent")]
    NoMatch,
    #[error("CDP execution failed")]
    Browser(#[from] browser_driver::BrowserError),
}

/// Lowercase, whitespace-collapsed comparison form. Unicode-aware via
/// `split_whitespace` — no language-specific patterns.
fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn label_score(query: &str, element: &AxElement) -> u8 {
    let name = normalize(&element.name);
    if name == query {
        return 3;
    }
    if name.starts_with(query) {
        return 2;
    }
    if name.contains(query) || normalize(&element.description).contains(query) {
        return 1;
    }
    0
}

/// Resolve `intent` against a live snapshot. Role must match exactly; the
/// strongest label wins, document order breaks ties.
#[must_use]
pub fn resolve_intent(elements: &[AxElement], intent: &SemanticIntent) -> Option<ResolvedIntent> {
    let (role, query) = intent.validate().ok()?;
    let mut best: Option<ResolvedIntent> = None;
    for element in elements {
        if element.role != role {
            continue;
        }
        let score = label_score(&query, element);
        if score == 0 {
            continue;
        }
        let stronger = best.as_ref().is_none_or(|current| score > current.score);
        if stronger {
            best = Some(ResolvedIntent {
                element: element.clone(),
                score,
            });
        }
    }
    best
}

/// Execute one intent: snapshot, resolve, badge, click. The badge stays
/// visible on success as evidence of what was acted on; failures clear it so
/// no stale overlay survives.
///
/// # Errors
/// Returns [`IntentError::NoMatch`] when nothing resolves (or the intent is
/// malformed) and [`IntentError::Browser`] on CDP failure.
pub async fn execute_intent(
    browser: &ManagedBrowser,
    origin: &url::Url,
    intent: &SemanticIntent,
) -> Result<IntentOutcome, IntentError> {
    browser.check_origin(origin).await?;
    let elements = browser.ax_snapshot(origin).await?;
    let resolved = resolve_intent(&elements, intent).ok_or(IntentError::NoMatch)?;
    let highlight = browser.node_rect(resolved.element.backend_node_id).await?;
    let mark = Mark {
        index: 0,
        x: highlight.x,
        y: highlight.y,
        width: highlight.width,
        height: highlight.height,
    };
    browser.show_marks(std::slice::from_ref(&mark)).await?;
    if let Err(error) = browser.click_mark(&mark).await {
        let _ = browser.clear_marks().await;
        return Err(error.into());
    }
    Ok(IntentOutcome { mark, highlight })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(role: &str, name: &str) -> AxElement {
        AxElement {
            backend_node_id: 1,
            role: role.into(),
            name: name.into(),
            description: String::new(),
        }
    }

    fn intent(role: &str, label: &str) -> SemanticIntent {
        SemanticIntent {
            role: role.into(),
            label_query: label.into(),
        }
    }

    #[test]
    fn scoring_prefers_exact_over_prefix_over_containment() {
        let elements = vec![
            element("button", "Submit application form"),
            element("button", "Submitter"),
            element("button", "Submit"),
        ];
        let Some(resolved) = resolve_intent(&elements, &intent("button", "Submit")) else {
            panic!("exact match resolves")
        };
        assert_eq!(resolved.score, 3);
        assert_eq!(resolved.element.name, "Submit");
    }

    #[test]
    fn role_mismatch_and_empty_query_never_match() {
        let elements = vec![element("button", "Sign in")];
        assert!(resolve_intent(&elements, &intent("link", "Sign in")).is_none());
        assert!(resolve_intent(&elements, &intent("button", "")).is_none());
        assert!(resolve_intent(&elements, &intent("", "Sign in")).is_none());
        assert!(resolve_intent(&[], &intent("button", "Sign in")).is_none());
    }

    #[test]
    fn description_matches_when_name_misses() {
        let mut recovery = element("textbox", "q");
        recovery.description = "Search invoices".into();
        let Some(resolved) = resolve_intent(&[recovery], &intent("textbox", "invoices")) else {
            panic!("description match resolves")
        };
        assert_eq!(resolved.score, 1);
    }

    #[test]
    fn ties_keep_document_order() {
        let first = AxElement {
            backend_node_id: 11,
            ..element("link", "Pricing")
        };
        let second = AxElement {
            backend_node_id: 22,
            ..element("link", "Pricing")
        };
        let Some(resolved) = resolve_intent(&[first, second], &intent("link", "pricing")) else {
            panic!("tie resolves")
        };
        assert_eq!(resolved.element.backend_node_id, 11);
    }

    #[test]
    fn intent_shape_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::to_string(&intent("button", "Sign in"))?;
        let parsed: SemanticIntent = serde_json::from_str(&json)?;
        assert_eq!(parsed.role, "button");
        assert!(serde_json::from_str::<SemanticIntent>(r#"{"role":"button"}"#).is_err());
        Ok(())
    }
}
