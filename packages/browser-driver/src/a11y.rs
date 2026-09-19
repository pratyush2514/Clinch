#![deny(unsafe_code)]
//! CDP Accessibility-tree element discovery.
//!
//! Selectors break when portals restyle; the Accessibility tree does not — it
//! describes what each control *is* (role) and *says* (name), independent of
//! markup. This module flattens `Accessibility.getFullAXTree` into the
//! interactive elements planners can act on. The role vocabulary is ARIA
//! semantics reported by the tree itself: no HTML tags, CSS classes, or
//! language-specific patterns appear anywhere here.

use crate::{BrowserError, IO_TIMEOUT, ManagedBrowser};
use chromiumoxide::cdp::browser_protocol::accessibility::{
    AxNode, AxValue, EnableParams, GetFullAxTreeParams,
};
use serde::{Deserialize, Serialize};

/// Interactive AX roles surfaced to planners.
const INTERACTIVE_ROLES: &[&str] = &["button", "link", "textbox", "combobox", "menuitem"];
/// Upper bound on returned elements; CDP document order keeps the visible
/// controls first, so truncation drops deep hidden subtrees, not the page.
const MAX_ELEMENTS: usize = 300;
const MAX_NAME_LEN: usize = 200;
const MAX_DESCRIPTION_LEN: usize = 500;

/// One actionable control: its AX identity plus the backend node that locates
/// it for box resolution and coordinate clicks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AxElement {
    pub backend_node_id: i64,
    pub role: String,
    pub name: String,
    pub description: String,
}

/// Extract display text from an AX value. Accepts both the plain-string shape
/// (`{"type":"string","value":"Sign in"}`) and the wrapped shape some roles
/// use (`{"type":"role","value":{"value":"button"}}`); anything else fails
/// closed to `None`.
fn ax_text(value: Option<&AxValue>) -> Option<String> {
    let payload = value?.value.as_ref()?;
    match payload {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Object(fields) => fields
            .get("value")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Flatten a full AX tree into interactive elements in document order.
/// Ignored subtrees, non-interactive roles, and nodes without a backend id
/// (unlocatable) are skipped.
#[must_use]
pub fn interactive_elements(nodes: &[AxNode]) -> Vec<AxElement> {
    let mut elements = Vec::new();
    for node in nodes {
        if elements.len() >= MAX_ELEMENTS || node.ignored {
            continue;
        }
        let Some(role) = ax_text(node.role.as_ref()) else {
            continue;
        };
        let role = role.to_ascii_lowercase();
        if !INTERACTIVE_ROLES.contains(&role.as_str()) {
            continue;
        }
        let Some(backend) = node.backend_dom_node_id.as_ref() else {
            continue;
        };
        elements.push(AxElement {
            backend_node_id: *backend.inner(),
            role,
            name: truncate(
                ax_text(node.name.as_ref()).as_deref().unwrap_or(""),
                MAX_NAME_LEN,
            ),
            description: truncate(
                ax_text(node.description.as_ref()).as_deref().unwrap_or(""),
                MAX_DESCRIPTION_LEN,
            ),
        });
    }
    elements
}

/// Token-efficient semantic list for planners and previews:
/// `[index] role — name — description` (description omitted when empty).
#[must_use]
pub fn render_semantic_list(elements: &[AxElement]) -> String {
    let mut out = String::new();
    for (index, element) in elements.iter().enumerate() {
        use std::fmt::Write as _;
        let _ = write!(out, "[{index}] {}", element.role);
        if !element.name.is_empty() {
            let _ = write!(out, " — {}", element.name);
        }
        if !element.description.is_empty() {
            let _ = write!(out, " — {}", element.description);
        }
        out.push('\n');
    }
    out
}

impl ManagedBrowser {
    /// Capture the live interactive-element snapshot for `origin`.
    ///
    /// # Errors
    /// Returns [`BrowserError`] on origin drift, CDP failure, or timeout.
    pub async fn ax_snapshot(&self, origin: &url::Url) -> Result<Vec<AxElement>, BrowserError> {
        self.check_origin(origin).await?;
        tokio::time::timeout(IO_TIMEOUT, self.page.execute(EnableParams {}))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?;
        let tree = tokio::time::timeout(
            IO_TIMEOUT,
            self.page.execute(GetFullAxTreeParams::builder().build()),
        )
        .await
        .map_err(|_| BrowserError::Timeout)?
        .map_err(|_| BrowserError::Connection)?;
        Ok(interactive_elements(&tree.result.nodes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(
        role: Option<&str>,
        name: Option<&str>,
        ignored: bool,
        backend: Option<i64>,
    ) -> Result<AxNode, Box<dyn std::error::Error>> {
        let value = |text: &str| serde_json::json!({"type": "string", "value": text});
        let mut json = serde_json::json!({"nodeId": "1", "ignored": ignored});
        if let Some(role) = role {
            json["role"] = value(role);
        }
        if let Some(name) = name {
            json["name"] = value(name);
        }
        if let Some(backend) = backend {
            json["backendDOMNodeId"] = serde_json::json!(backend);
        }
        Ok(serde_json::from_value(json)?)
    }

    #[test]
    fn text_extraction_covers_both_wire_shapes() -> Result<(), Box<dyn std::error::Error>> {
        let plain: AxValue =
            serde_json::from_value(serde_json::json!({"type": "string", "value": "Sign in"}))?;
        assert_eq!(ax_text(Some(&plain)).as_deref(), Some("Sign in"));
        let wrapped: AxValue = serde_json::from_value(
            serde_json::json!({"type": "role", "value": {"value": "button"}}),
        )?;
        assert_eq!(ax_text(Some(&wrapped)).as_deref(), Some("button"));
        assert_eq!(ax_text(None), None);
        let numeric: AxValue =
            serde_json::from_value(serde_json::json!({"type": "number", "value": 3}))?;
        assert_eq!(ax_text(Some(&numeric)), None);
        Ok(())
    }

    #[test]
    fn filter_keeps_only_locatable_interactive_roles() -> Result<(), Box<dyn std::error::Error>> {
        let nodes = vec![
            node(Some("button"), Some("Sign in"), false, Some(11))?,
            node(Some("link"), Some("Pricing"), false, Some(12))?,
            node(Some("textbox"), Some("Email"), false, Some(13))?,
            node(Some("combobox"), Some("Country"), false, Some(14))?,
            node(Some("menuitem"), Some("Copy"), false, Some(15))?,
            // Noise the filter must drop:
            node(Some("StaticText"), Some("Welcome"), false, Some(16))?,
            node(Some("button"), Some("Hidden"), true, Some(17))?,
            node(Some("button"), Some("Detached"), false, None)?,
            node(None, Some("Roleless"), false, Some(18))?,
        ];
        let elements = interactive_elements(&nodes);
        assert_eq!(elements.len(), 5);
        assert_eq!(elements[0].backend_node_id, 11);
        assert_eq!(elements[0].role, "button");
        assert_eq!(elements[0].name, "Sign in");
        Ok(())
    }

    #[test]
    fn role_matching_ignores_case_and_truncates_long_names()
    -> Result<(), Box<dyn std::error::Error>> {
        let nodes = vec![node(
            Some("Button"),
            Some(&"x".repeat(500)),
            false,
            Some(21),
        )?];
        let elements = interactive_elements(&nodes);
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].role, "button");
        assert_eq!(elements[0].name.len(), MAX_NAME_LEN);
        Ok(())
    }

    #[test]
    fn result_is_capped_in_document_order() -> Result<(), Box<dyn std::error::Error>> {
        let mut nodes = Vec::new();
        for id in 0..MAX_ELEMENTS + 50 {
            let backend = i64::try_from(id).map_err(std::io::Error::other)?;
            nodes.push(node(Some("link"), Some("more"), false, Some(backend))?);
        }
        let elements = interactive_elements(&nodes);
        assert_eq!(elements.len(), MAX_ELEMENTS);
        assert_eq!(elements[0].backend_node_id, 0);
        Ok(())
    }

    #[test]
    fn semantic_list_is_compact_and_indexed() {
        let elements = vec![
            AxElement {
                backend_node_id: 1,
                role: "button".into(),
                name: "Sign in".into(),
                description: String::new(),
            },
            AxElement {
                backend_node_id: 2,
                role: "textbox".into(),
                name: String::new(),
                description: "Email address".into(),
            },
        ];
        let list = render_semantic_list(&elements);
        assert!(list.contains("[0] button — Sign in\n"));
        assert!(list.contains("[1] textbox — Email address\n"));
        assert!(render_semantic_list(&[]).is_empty());
    }
}
