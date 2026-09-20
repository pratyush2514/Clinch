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
/// Shared bounds for collected surroundings, whether they come from the AX
/// graph rollup or the opt-in DOM fallback in `session.rs`: at most a dozen
/// snippets of a hundred characters each.
const MAX_CONTAINER_ITEMS: usize = 12;
const MAX_CONTAINER_ITEM_LEN: usize = 100;

/// One actionable control: its AX identity plus the backend node that locates
/// it for box resolution and coordinate clicks. `container_text` carries the
/// surrounding words (parent name, sibling labels) so duplicate controls —
/// three "Download" buttons in different cards — stay distinguishable.
///
/// Case is preserved verbatim for display (`0LWQXDWW` stays uppercase);
/// matching in `macro-engine` is case-insensitive, whitespace-normalized, and
/// fuzzy across items, so lowercased prompts (`0lwqxdww`) and split
/// attributes (`$4.00` / `Declined` / `June 12`) resolve without any
/// normalization here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AxElement {
    pub backend_node_id: i64,
    pub role: String,
    pub name: String,
    pub description: String,
    pub container_text: Vec<String>,
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
    use std::borrow::Borrow;
    use std::collections::HashMap;
    let index: HashMap<String, usize> = nodes
        .iter()
        .enumerate()
        .map(|(position, node)| {
            let id: &str = node.node_id.borrow();
            (id.to_owned(), position)
        })
        .collect();
    let mut elements = Vec::new();
    for (position, node) in nodes.iter().enumerate() {
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
        let name = truncate(
            ax_text(node.name.as_ref()).as_deref().unwrap_or(""),
            MAX_NAME_LEN,
        );
        elements.push(AxElement {
            backend_node_id: *backend.inner(),
            role,
            name: name.clone(),
            description: truncate(
                ax_text(node.description.as_ref()).as_deref().unwrap_or(""),
                MAX_DESCRIPTION_LEN,
            ),
            container_text: container_text(position, nodes, &index, &name),
        });
    }
    elements
}

/// Leaf-text rollup around an interactive node, resolved through the node
/// graph only: climb to the nearest structural container (at most three
/// links up), then recursively traverse ALL of its descendant nodes in
/// document order. CDP splits table-cell text across several shapes, so every
/// visited node contributes its `name` plus its `value`, which covers plain
/// text, `StaticText` / `InlineTextBox` leaves, input values, and ARIA
/// names alike. No tags, classes, or patterns — just `parent_id` /
/// `child_ids` links plus AX role checks. Bounded and deduplicated; the
/// element's own subtree is excluded (its text already feeds its name).
///
/// Leaf text is hygiene-normalized (non-breaking spaces become spaces,
/// whitespace runs collapse, edges trim) but case is preserved: matching
/// lowercases downstream in `macro-engine` and the semantic list displays
/// the portal's own casing.
fn container_text(
    position: usize,
    nodes: &[AxNode],
    index: &std::collections::HashMap<String, usize>,
    self_name: &str,
) -> Vec<String> {
    use std::borrow::Borrow;
    /// Row-level AX roles that bound a disambiguation set: every control in
    /// the row/card/item shares this container, so sibling-cell leaf text
    /// (IDs, amounts, dates) reaches each of its buttons. ARIA semantics
    /// reported by the tree — grouping markup such as `fieldset` surfaces
    /// here as `group`, so no separate entry exists for those spellings.
    const CONTAINER_ROLES: &[&str] = &["row", "listitem", "article", "group", "tr"];
    /// Cell-level AX roles (plus Chrome's `cell`/header spellings). A
    /// button's own cell never holds its siblings' evidence, so these stay
    /// transparent while climbing: the walk passes through them toward the
    /// row. When no row-level container exists above (card grids), the
    /// nearest cell still bounds the rollup instead of the page top.
    const CELL_ROLES: &[&str] = &["gridcell", "cell", "columnheader", "rowheader"];
    const MAX_CLIMB: usize = 3;
    // Walk up at most three links, remembering the nearest row-level
    // container and the nearest cell-level fallback. Row-level wins: it is
    // the disambiguation boundary whose whole subtree must be aggregated.
    let mut container = position;
    let mut row_container: Option<usize> = None;
    let mut cell_container: Option<usize> = None;
    for _ in 0..MAX_CLIMB {
        let Some(parent_id) = nodes[container].parent_id.as_ref() else {
            break;
        };
        let parent_key: &str = parent_id.borrow();
        let Some(&parent_position) = index.get(parent_key) else {
            break;
        };
        container = parent_position;
        if let Some(role) = ax_text(nodes[container].role.as_ref()) {
            let role = role.to_ascii_lowercase();
            if CONTAINER_ROLES.contains(&role.as_str()) {
                row_container = Some(container);
                break;
            }
            if cell_container.is_none() && CELL_ROLES.contains(&role.as_str()) {
                cell_container = Some(container);
            }
        }
    }
    if let Some(row) = row_container {
        container = row;
    } else if let Some(cell) = cell_container {
        container = cell;
    }
    if container == position {
        return Vec::new();
    }
    let excluded = subtree_positions(position, nodes, index);
    let mut context = Vec::new();
    let push = |text: &str, context: &mut Vec<String>| {
        let text = truncate(&normalize_leaf_text(text), MAX_CONTAINER_ITEM_LEN);
        if !text.is_empty() && text != self_name && !context.contains(&text) {
            context.push(text);
        }
    };
    // The container's own accessible name and value seed the rollup, then
    // every descendant contributes its `name` plus its `value`: this is what
    // pulls `StaticText` / `InlineTextBox` cell leaves and input values in.
    if let Some(name) = ax_text(nodes[container].name.as_ref()) {
        push(&name, &mut context);
    }
    if let Some(value) = ax_text(nodes[container].value.as_ref()) {
        push(&value, &mut context);
    }
    let mut visited = vec![container];
    let mut stack = child_positions(container, nodes, index);
    stack.reverse();
    while context.len() < MAX_CONTAINER_ITEMS {
        let Some(current) = stack.pop() else {
            break;
        };
        if visited.contains(&current) || excluded.contains(&current) {
            continue;
        }
        visited.push(current);
        if let Some(name) = ax_text(nodes[current].name.as_ref()) {
            push(&name, &mut context);
        }
        if let Some(value) = ax_text(nodes[current].value.as_ref()) {
            push(&value, &mut context);
        }
        let mut children = child_positions(current, nodes, index);
        children.reverse();
        stack.extend(children);
    }
    context
}

/// Shape raw DOM `innerText` into container snippets with the same bounds
/// and hygiene as the AX rollup: split rendered lines, normalize each,
/// drop empties and duplicates, cap count and length. Shared with the
/// opt-in DOM fallback in `session.rs` so both sources feed matching the
/// same diet. Case is preserved; `macro-engine` lowercases at match time.
pub(crate) fn dom_container_items(inner_text: &str) -> Vec<String> {
    let mut items = Vec::new();
    for line in inner_text.split(['\n', '\r']) {
        if items.len() >= MAX_CONTAINER_ITEMS {
            break;
        }
        let text = truncate(&normalize_leaf_text(line), MAX_CONTAINER_ITEM_LEN);
        if !text.is_empty() && !items.contains(&text) {
            items.push(text);
        }
    }
    items
}

/// Collection hygiene for one leaf string: non-breaking spaces become plain
/// spaces, whitespace runs collapse, edges trim. Case is intentionally kept:
/// `macro-engine` lowercases at match time and the semantic list shows the
/// portal's own casing.
fn normalize_leaf_text(text: &str) -> String {
    text.replace('\u{a0}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// All positions in the subtree rooted at `root` (inclusive), cycle-guarded.
fn subtree_positions(
    root: usize,
    nodes: &[AxNode],
    index: &std::collections::HashMap<String, usize>,
) -> Vec<usize> {
    let mut seen = vec![root];
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        for child in child_positions(current, nodes, index) {
            if !seen.contains(&child) {
                seen.push(child);
                stack.push(child);
            }
        }
    }
    seen
}

/// Resolved child positions of one node in document order.
fn child_positions(
    position: usize,
    nodes: &[AxNode],
    index: &std::collections::HashMap<String, usize>,
) -> Vec<usize> {
    use std::borrow::Borrow;
    let mut out = Vec::new();
    if let Some(children) = nodes[position].child_ids.as_ref() {
        for child_id in children {
            let child_key: &str = child_id.borrow();
            if let Some(&child_position) = index.get(child_key) {
                out.push(child_position);
            }
        }
    }
    out
}

/// Token-efficient semantic list for planners and previews:
/// `[index] role — name — description [in: context]`. The bracketed context
/// is what disambiguates duplicate controls for both humans and matchers.
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
        if !element.container_text.is_empty() {
            let _ = write!(out, " [in: {}]", element.container_text.join(" / "));
        }
        out.push('\n');
    }
    out
}

impl ManagedBrowser {
    /// Capture the live interactive-element snapshot for `origin`. Discovery
    /// itself stays on the AX graph (no tags, no classes); afterwards the
    /// opt-in DOM fallback in `session.rs` may fill empties when its env
    /// gate is set, and is a silent no-op otherwise.
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
        let mut elements = interactive_elements(&tree.result.nodes);
        self.enrich_empty_containers(&mut elements).await;
        Ok(elements)
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
        linked("1", role, name, None, &[], ignored, backend)
    }

    fn linked(
        id: &str,
        role: Option<&str>,
        name: Option<&str>,
        parent: Option<&str>,
        children: &[&str],
        ignored: bool,
        backend: Option<i64>,
    ) -> Result<AxNode, Box<dyn std::error::Error>> {
        linked_full(id, role, name, None, parent, children, ignored, backend)
    }

    #[allow(clippy::too_many_arguments)]
    fn linked_full(
        id: &str,
        role: Option<&str>,
        name: Option<&str>,
        value_text: Option<&str>,
        parent: Option<&str>,
        children: &[&str],
        ignored: bool,
        backend: Option<i64>,
    ) -> Result<AxNode, Box<dyn std::error::Error>> {
        let value = |text: &str| serde_json::json!({"type": "string", "value": text});
        let mut json = serde_json::json!({"nodeId": id, "ignored": ignored});
        if let Some(role) = role {
            json["role"] = value(role);
        }
        if let Some(name) = name {
            json["name"] = value(name);
        }
        if let Some(value_text) = value_text {
            json["value"] = value(value_text);
        }
        if let Some(parent) = parent {
            json["parentId"] = serde_json::json!(parent);
        }
        if !children.is_empty() {
            json["childIds"] = serde_json::json!(children);
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
    fn container_text_comes_from_the_parent_graph() -> Result<(), Box<dyn std::error::Error>> {
        // Two identical "Download" buttons in one card: the card name and the
        // adjacent text node tell them apart. No tags or classes involved —
        // only parent_id / child_ids links.
        let nodes = vec![
            linked(
                "card",
                None,
                Some("Statements"),
                None,
                &["t1", "b1", "b2"],
                false,
                None,
            )?,
            linked(
                "t1",
                Some("StaticText"),
                Some("Statement #42"),
                Some("card"),
                &[],
                false,
                None,
            )?,
            linked(
                "b1",
                Some("button"),
                Some("Download"),
                Some("card"),
                &[],
                false,
                Some(31),
            )?,
            linked(
                "b2",
                Some("button"),
                Some("Download"),
                Some("card"),
                &[],
                false,
                Some(32),
            )?,
            linked(
                "orphan",
                Some("link"),
                Some("Home"),
                None,
                &[],
                false,
                Some(33),
            )?,
            linked(
                "ghost",
                Some("link"),
                Some("Nowhere"),
                Some("missing"),
                &[],
                false,
                Some(34),
            )?,
        ];
        let elements = interactive_elements(&nodes);
        assert_eq!(elements.len(), 4);
        // Parent name first, then sibling text; the element's own name and
        // the twin button's identical name are excluded.
        assert_eq!(
            elements[0].container_text,
            vec!["Statements".to_owned(), "Statement #42".to_owned()]
        );
        // Orphans and dangling parents yield no context, never an error.
        assert!(elements[2].container_text.is_empty());
        assert!(elements[3].container_text.is_empty());
        Ok(())
    }

    #[test]
    fn ancestor_row_cells_attach_to_their_button() -> Result<(), Box<dyn std::error::Error>> {
        // A table row: the ID cell's text must reach the button in the
        // sibling cell, while content outside the row stays out.
        let nodes = vec![
            linked("table", Some("table"), None, None, &["row"], false, None)?,
            linked(
                "row",
                Some("row"),
                None,
                Some("table"),
                &["c1", "c2"],
                false,
                None,
            )?,
            linked("c1", None, None, Some("row"), &["t1"], false, None)?,
            linked(
                "t1",
                Some("StaticText"),
                Some("0LWQXDWW"),
                Some("c1"),
                &[],
                false,
                None,
            )?,
            linked("c2", None, None, Some("row"), &["b1"], false, None)?,
            linked(
                "b1",
                Some("button"),
                Some("Download"),
                Some("c2"),
                &[],
                false,
                Some(41),
            )?,
        ];
        let elements = interactive_elements(&nodes);
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].container_text, vec!["0LWQXDWW".to_owned()]);
        Ok(())
    }

    #[test]
    fn row_amount_status_date_and_id_reach_the_button() -> Result<(), Box<dyn std::error::Error>> {
        // An invoice row: amount, status, date, and uppercase ID cells must
        // all reach the row button verbatim (case preserved) so fuzzy,
        // case-insensitive container matching can scope `$4 declined June 12`
        // or lowercase `0lwqxdww` prompts without normalization here.
        let nodes = vec![
            linked("table", Some("table"), None, None, &["row"], false, None)?,
            linked(
                "row",
                Some("row"),
                None,
                Some("table"),
                &["c1", "c2", "c3", "c4", "c5"],
                false,
                None,
            )?,
            linked("c1", None, None, Some("row"), &["t1"], false, None)?,
            linked(
                "t1",
                Some("StaticText"),
                Some("$4.00"),
                Some("c1"),
                &[],
                false,
                None,
            )?,
            linked("c2", None, None, Some("row"), &["t2"], false, None)?,
            linked(
                "t2",
                Some("StaticText"),
                Some("Declined"),
                Some("c2"),
                &[],
                false,
                None,
            )?,
            linked("c3", None, None, Some("row"), &["t3"], false, None)?,
            linked(
                "t3",
                Some("StaticText"),
                Some("June 12"),
                Some("c3"),
                &[],
                false,
                None,
            )?,
            linked("c4", None, None, Some("row"), &["t4"], false, None)?,
            linked(
                "t4",
                Some("StaticText"),
                Some("0LWQXDWW"),
                Some("c4"),
                &[],
                false,
                None,
            )?,
            linked("c5", None, None, Some("row"), &["b1"], false, None)?,
            linked(
                "b1",
                Some("button"),
                Some("Download"),
                Some("c5"),
                &[],
                false,
                Some(41),
            )?,
        ];
        let elements = interactive_elements(&nodes);
        assert_eq!(elements.len(), 1);
        assert_eq!(
            elements[0].container_text,
            vec![
                "$4.00".to_owned(),
                "Declined".to_owned(),
                "June 12".to_owned(),
                "0LWQXDWW".to_owned(),
            ]
        );
        Ok(())
    }

    #[test]
    fn tr_gridcell_inline_leaves_and_values_roll_up() -> Result<(), Box<dyn std::error::Error>> {
        // A `tr` row with `gridcell` children: cell text arrives split
        // across `StaticText` names, an `InlineTextBox` leaf, and an input
        // `value`. The recursive rollup must surface every piece verbatim.
        let nodes = vec![
            linked("table", Some("table"), None, None, &["row"], false, None)?,
            linked_full(
                "row",
                Some("tr"),
                None,
                None,
                Some("table"),
                &["c1", "c2", "c3"],
                false,
                None,
            )?,
            linked_full(
                "c1",
                Some("gridcell"),
                None,
                None,
                Some("row"),
                &["t1"],
                false,
                None,
            )?,
            linked_full(
                "t1",
                Some("StaticText"),
                Some("0LWQXDWW"),
                None,
                Some("c1"),
                &[],
                false,
                None,
            )?,
            linked_full(
                "c2",
                Some("gridcell"),
                None,
                None,
                Some("row"),
                &["t2"],
                false,
                None,
            )?,
            linked_full(
                "t2",
                Some("InlineTextBox"),
                Some("Visa ending in 2919"),
                None,
                Some("c2"),
                &[],
                false,
                None,
            )?,
            linked_full(
                "c3",
                Some("gridcell"),
                None,
                None,
                Some("row"),
                &["t3", "b1"],
                false,
                None,
            )?,
            linked_full(
                "t3",
                Some("textbox"),
                Some("Amount"),
                Some("$4.00"),
                Some("c3"),
                &[],
                false,
                None,
            )?,
            linked_full(
                "b1",
                Some("button"),
                Some("Download"),
                None,
                Some("c3"),
                &[],
                false,
                Some(41),
            )?,
        ];
        // The value-carrying textbox has no backend id, so only the row
        // button surfaces — with the whole row rolled into its context.
        let elements = interactive_elements(&nodes);
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].backend_node_id, 41);
        assert_eq!(
            elements[0].container_text,
            vec![
                "0LWQXDWW".to_owned(),
                "Visa ending in 2919".to_owned(),
                "Amount".to_owned(),
                "$4.00".to_owned(),
            ]
        );
        Ok(())
    }

    #[test]
    fn deep_grandchild_cell_text_attaches_to_row_button() -> Result<(), Box<dyn std::error::Error>>
    {
        // Identifier and status leaves nested two levels deep inside sibling
        // cells: the recursive subtree walk must still attach them to the
        // row button, or row disambiguation goes blind.
        let nodes = vec![
            linked("table", Some("table"), None, None, &["row"], false, None)?,
            linked_full(
                "row",
                Some("row"),
                None,
                None,
                Some("table"),
                &["c1", "c2"],
                false,
                None,
            )?,
            linked_full("c1", None, None, None, Some("row"), &["inner"], false, None)?,
            linked_full("inner", None, None, None, Some("c1"), &["t1"], false, None)?,
            linked_full(
                "t1",
                Some("StaticText"),
                Some("1TUEWZUA"),
                None,
                Some("inner"),
                &[],
                false,
                None,
            )?,
            linked_full(
                "c2",
                None,
                None,
                None,
                Some("row"),
                &["t2", "b1"],
                false,
                None,
            )?,
            linked_full(
                "t2",
                Some("InlineTextBox"),
                Some("Paid"),
                None,
                Some("c2"),
                &[],
                false,
                None,
            )?,
            linked_full(
                "b1",
                Some("button"),
                Some("Download"),
                None,
                Some("c2"),
                &[],
                false,
                Some(41),
            )?,
        ];
        let elements = interactive_elements(&nodes);
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].backend_node_id, 41);
        assert_eq!(
            elements[0].container_text,
            vec!["1TUEWZUA".to_owned(), "Paid".to_owned()]
        );
        Ok(())
    }

    #[test]
    fn dom_inner_text_shapes_into_bounded_snippets() {
        // Rendered innerText lines become deduplicated container items with
        // the same hygiene as the AX rollup: NBSP collapses, empties drop.
        assert_eq!(
            dom_container_items("0LWQXDWW\nVisa ending in 2919\n\n$4.00\u{a0} "),
            vec![
                "0LWQXDWW".to_owned(),
                "Visa ending in 2919".to_owned(),
                "$4.00".to_owned(),
            ]
        );
        assert_eq!(
            dom_container_items("Declined\nDeclined\nJune 12"),
            vec!["Declined".to_owned(), "June 12".to_owned()]
        );
        assert!(dom_container_items("").is_empty());
        assert!(dom_container_items("   \n \r\n  ").is_empty());
    }

    #[test]
    fn leaf_text_normalizes_nbsp_and_whitespace() {
        assert_eq!(normalize_leaf_text("June\u{a0}  12"), "June 12");
        assert_eq!(normalize_leaf_text("  $4.00\u{a0} "), "$4.00");
        assert_eq!(normalize_leaf_text(""), "");
        // Case survives collection: matching lowercases downstream.
        assert_eq!(normalize_leaf_text("0LWQXDWW"), "0LWQXDWW");
    }

    #[test]
    fn climb_stops_at_three_levels_and_structural_roles() -> Result<(), Box<dyn std::error::Error>>
    {
        // Five anonymous levels: context settles on the great-grandparent,
        // never the page top.
        let nodes = vec![
            linked("d1", None, Some("L1"), None, &["d2"], false, None)?,
            linked("d2", None, Some("L2"), Some("d1"), &["d3"], false, None)?,
            linked("d3", None, Some("L3"), Some("d2"), &["d4"], false, None)?,
            linked("d4", None, Some("L4"), Some("d3"), &["go"], false, None)?,
            linked(
                "go",
                Some("button"),
                Some("Go"),
                Some("d4"),
                &[],
                false,
                Some(42),
            )?,
        ];
        let elements = interactive_elements(&nodes);
        assert_eq!(elements.len(), 1);
        assert_eq!(
            elements[0].container_text,
            vec!["L2".to_owned(), "L3".to_owned(), "L4".to_owned()]
        );
        // A structural role stops the climb early: the page name above the
        // group never leaks into the button's context.
        let nodes = vec![
            linked(
                "page",
                None,
                Some("Whole page"),
                None,
                &["sec"],
                false,
                None,
            )?,
            linked(
                "sec",
                Some("group"),
                Some("Statements"),
                Some("page"),
                &["dl"],
                false,
                None,
            )?,
            linked(
                "dl",
                Some("button"),
                Some("Download"),
                Some("sec"),
                &[],
                false,
                Some(43),
            )?,
        ];
        let elements = interactive_elements(&nodes);
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].container_text, vec!["Statements".to_owned()]);
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
                container_text: Vec::new(),
            },
            AxElement {
                backend_node_id: 2,
                role: "textbox".into(),
                name: String::new(),
                description: "Email address".into(),
                container_text: Vec::new(),
            },
            AxElement {
                backend_node_id: 3,
                role: "button".into(),
                name: "Download".into(),
                description: String::new(),
                container_text: vec!["Statements".into(), "Statement #42".into()],
            },
        ];
        let list = render_semantic_list(&elements);
        assert!(list.contains("[0] button — Sign in\n"));
        assert!(list.contains("[1] textbox — Email address\n"));
        assert!(list.contains("[2] button — Download [in: Statements / Statement #42]\n"));
        assert!(render_semantic_list(&[]).is_empty());
    }
}
