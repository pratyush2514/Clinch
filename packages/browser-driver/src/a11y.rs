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
use chromiumoxide::cdp::browser_protocol::{
    accessibility::{AxNode, AxValue, EnableParams, GetFullAxTreeParams},
    target::GetTargetsParams,
};
use serde::{Deserialize, Serialize};

/// Interactive AX roles surfaced to planners, normalized to lowercase at
/// discovery (`Tab` → `tab`). Roles stay plain strings — the equivalent of
/// an `AxRole` enum without churning the serialized `AxElement` wire shape
/// (`deny_unknown_fields` payloads, previews, TS types) for zero behavior
/// gain. `tab` covers tabbed navigation; its `tablist` parent bounds
/// context (see `CONTAINER_ROLES`), it never needs its own entry here.
const INTERACTIVE_ROLES: &[&str] = &["button", "link", "textbox", "combobox", "menuitem", "tab"];
/// Upper bound on returned elements; CDP document order keeps the visible
/// controls first, so truncation drops deep hidden subtrees, not the page.
pub const MAX_ELEMENTS: usize = 300;
pub const MAX_NAME_LEN: usize = 200;
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
///
/// `landmark` names the nearest landmark-role ancestor (`navigation`,
/// `banner`, `contentinfo`, `complementary`) when the climb passes one, so
/// batch collection can tell page chrome apart from data rows. `None`
/// otherwise. Single-intent grounding ignores it — nav links stay
/// resolvable on their own.
/// `LANDMARK_ROLES` is mirrored (duplicated, not imported) by
/// `macro-engine`'s batch exclusion, so each side's set can evolve alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AxElement {
    pub backend_node_id: i64,
    pub role: String,
    pub name: String,
    pub description: String,
    pub container_text: Vec<String>,
    /// Added after `container_text`: `#[serde(default)]` keeps older
    /// snapshots and payloads parsing with no landmark recorded.
    #[serde(default)]
    pub landmark: Option<String>,
}

/// ARIA landmark roles that mark page chrome (nav, header, footer,
/// sidebar equivalents). Recorded — never used to bound the rollup itself.
pub(crate) const LANDMARK_ROLES: &[&str] =
    &["navigation", "banner", "contentinfo", "complementary"];

/// Extract display text from an AX value. Accepts both the plain-string shape
/// (`{"type":"string","value":"Sign in"}`) and the wrapped shape some roles
/// use (`{"type":"role","value":{"value":"button"}}`); anything else fails
/// closed to `None`.
pub fn ax_text(value: Option<&AxValue>) -> Option<String> {
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
    interactive_elements_inner(nodes, MAX_ELEMENTS)
}

/// Flatten a full AX tree into interactive elements without the
/// [`MAX_ELEMENTS`] head truncation. The account-home worker's post-click
/// re-snapshot uses this: a revealed menu typically renders at the end of
/// the document (React portal), past the 300-element head the navigator
/// slice keeps, so the truncated view reports "no new controls" for a
/// menu that plainly opened (caught live on Reddit: `actionable 300 → 300`
/// after opening the user menu). The model navigator keeps the truncated
/// slice; only the worker's revealed-candidate search sees the full list.
#[must_use]
pub fn interactive_elements_all(nodes: &[AxNode]) -> Vec<AxElement> {
    interactive_elements_inner(nodes, usize::MAX)
}

fn interactive_elements_inner(nodes: &[AxNode], limit: usize) -> Vec<AxElement> {
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
        if elements.len() >= limit || node.ignored {
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
        let (container_text, landmark) = container_text(position, nodes, &index, &name);
        elements.push(AxElement {
            backend_node_id: *backend.inner(),
            role,
            name: name.clone(),
            description: truncate(
                ax_text(node.description.as_ref()).as_deref().unwrap_or(""),
                MAX_DESCRIPTION_LEN,
            ),
            container_text,
            landmark,
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
/// Surroundings plus the nearest landmark ancestor, if the climb passes
/// one. Landmarks never bound the rollup — they only label where the
/// element lives, for batch chrome filtering downstream.
fn container_text(
    position: usize,
    nodes: &[AxNode],
    index: &std::collections::HashMap<String, usize>,
    self_name: &str,
) -> (Vec<String>, Option<String>) {
    use std::borrow::Borrow;
    /// Row-level AX roles that bound a disambiguation set: every control in
    /// the row/card/item shares this container, so sibling-cell leaf text
    /// (IDs, amounts, dates) reaches each of its buttons. ARIA semantics
    /// reported by the tree — grouping markup such as `fieldset` surfaces
    /// here as `group`, and tab strips surface as `tablist`, so no separate
    /// entries exist for those spellings.
    const CONTAINER_ROLES: &[&str] = &["row", "listitem", "article", "group", "tr", "tablist"];
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
    let mut landmark: Option<String> = None;
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
            if landmark.is_none() && LANDMARK_ROLES.contains(&role.as_str()) {
                landmark = Some(role);
            }
        }
    }
    if let Some(row) = row_container {
        container = row;
    } else if let Some(cell) = cell_container {
        container = cell;
    }
    if container == position {
        return (Vec::new(), landmark);
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
    (context, landmark)
}

/// Shape raw DOM `innerText` into container snippets with the same bounds
/// and hygiene as the AX rollup: split rendered lines, normalize each,
/// drop empties and duplicates, cap count and length. Shared with the
/// opt-in DOM fallback in `session.rs` so both sources feed matching the
/// same diet. Case is preserved; `macro-engine` lowercases at match time.
pub fn dom_container_items(inner_text: &str) -> Vec<String> {
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
pub fn normalize_leaf_text(text: &str) -> String {
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

/// Session-activity line journaled (by the desktop service) for every
/// zero-node accessibility resync. Single source so the driver policy and
/// the journaled text cannot drift apart.
pub const AX_TARGET_RESYNC_LINE: &str =
    "ax_target_resync: re-enabled accessibility after 0-node tree";

/// Pure resync decision: an empty tree on a real page means a renderer swap
/// deactivated the Accessibility domain; an empty tree on `about:blank`
/// (or an unreadable URL) is expected and needs no resync.
pub fn needs_ax_resync(node_count: usize, page_url: Option<&url::Url>) -> bool {
    node_count == 0 && page_url.is_some_and(|url| matches!(url.scheme(), "https" | "http"))
}

/// Debug record of one snapshot's resync decision, returned inline from
/// [`ManagedBrowser::ax_snapshot`] alongside the elements. Instrumentation
/// only: building a check never changes snapshot behavior.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AxResyncCheck {
    /// Nodes the first tree fetch returned (0 on fetch error).
    pub node_count: usize,
    /// Page URL at observation time, or empty when the URL read failed.
    pub url: String,
    /// The [`needs_ax_resync`] verdict for these inputs.
    pub predicate: bool,
    /// Caught CDP failure text (e.g. `"Session detached"`, `"Target
    /// closed"`), `None` when every CDP call in the snapshot succeeded.
    pub cdp_error: Option<String>,
}

impl AxResyncCheck {
    /// Build from an observed tree size plus the page URL read (`None`
    /// when the read failed). The predicate is computed here so checks
    /// carry the verdict instead of recomputing it downstream. No error
    /// yet — callers attach one when a CDP call fails.
    #[must_use]
    pub fn new(node_count: usize, page_url: Option<&url::Url>) -> Self {
        Self {
            predicate: needs_ax_resync(node_count, page_url),
            node_count,
            url: page_url.map_or_else(String::new, |url| url.as_str().to_owned()),
            cdp_error: None,
        }
    }

    /// Journal line, e.g.
    /// `ax_resync_check: nodes=0 url='https://github.com/x' predicate=true`.
    /// A failed URL read renders as `url=''`. A caught CDP failure travels
    /// separately as `ax_snapshot_cdp_error: '<e>'`.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "ax_resync_check: nodes={} url='{}' predicate={}",
            self.node_count, self.url, self.predicate
        )
    }
}

impl ManagedBrowser {
    /// Arm the Accessibility domain on the live CDP session. Unconditional
    /// by contract: every tree fetch — initial and resync retry alike —
    /// re-arms first, because cross-origin renderer swaps deactivate the
    /// domain on the new process.
    ///
    /// # Errors
    /// Reports connection failure or timeout.
    async fn enable_accessibility(&self) -> Result<(), BrowserError> {
        tokio::time::timeout(IO_TIMEOUT, self.page.execute(EnableParams {}))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?;
        Ok(())
    }

    /// Single tree fetch with the CDP failure preserved as text for
    /// [`AxResyncCheck::cdp_error`].
    async fn fetch_ax_nodes(&self) -> Result<Vec<AxNode>, String> {
        match tokio::time::timeout(
            IO_TIMEOUT,
            self.page.execute(GetFullAxTreeParams::builder().build()),
        )
        .await
        {
            Err(_) => Err(BrowserError::Timeout.to_string()),
            Ok(Err(error)) => Err(error.to_string()),
            Ok(Ok(tree)) => Ok(tree.result.nodes),
        }
    }

    /// Best-effort re-attachment for the targeted retry: re-query
    /// `Target.getTargets`, select the live page target for `page_url`,
    /// ensure a foreign owner is attached, then re-arm Accessibility.
    /// Fail-open by design — all errors swallowed — because the retry
    /// fetch below still runs either way. Shared by snapshots and
    /// post-navigation target refreshes.
    pub(crate) async fn reattach_active_target(&self, page_url: Option<&url::Url>) {
        let targets =
            tokio::time::timeout(IO_TIMEOUT, self.page.execute(GetTargetsParams::default()))
                .await
                .ok()
                .and_then(Result::ok)
                .map(|response| response.result.target_infos)
                .unwrap_or_default();
        if let Some(url) = page_url
            && let Some(active) = crate::select_active_page_target(&targets, url)
            && active != *self.page.target_id()
        {
            let _ = self.browser.get_page(active).await;
        }
        let _ = self.enable_accessibility().await;
    }

    /// Capture the live interactive-element snapshot for `origin`, with its
    /// resync diagnostics inline: `(elements, check, resyncs)`. Discovery
    /// itself stays on the AX graph (no tags, no classes); afterwards the
    /// opt-in DOM fallback in `session.rs` may fill empties when its env
    /// gate is set, and is a silent no-op otherwise.
    ///
    /// Single-exit pipeline: every path — success, zero-node tree, CDP
    /// error — funnels into the one return below carrying full diagnostics.
    /// Confinement honors intentional navigation: drift holds only when the
    /// live page matches neither the requested origin nor the anchored
    /// portal, and the error names the active anchor. Unsolicited drift
    /// fails closed here with no targeted retry.
    pub async fn ax_snapshot(&self, origin: &url::Url) -> (Vec<AxElement>, AxResyncCheck, u64) {
        let (nodes, check, resyncs) = self.ax_fetched_nodes(origin).await;
        let mut elements = interactive_elements(&nodes);
        self.enrich_empty_containers(&mut elements).await;
        (elements, check, resyncs)
    }

    /// Post-click re-snapshot for the account-home worker: the full
    /// interactive-element list without the [`MAX_ELEMENTS`] head
    /// truncation. A revealed menu renders at the end of the document,
    /// past the head the navigator slice keeps — the truncated view
    /// cannot see it. Every other caller keeps the truncated slice.
    pub async fn ax_snapshot_untruncated(
        &self,
        origin: &url::Url,
    ) -> (Vec<AxElement>, AxResyncCheck, u64) {
        let (nodes, check, resyncs) = self.ax_fetched_nodes(origin).await;
        let mut elements = interactive_elements_all(&nodes);
        self.enrich_empty_containers(&mut elements).await;
        (elements, check, resyncs)
    }

    /// Raw AX node fetch behind [`ax_snapshot`] and
    /// [`ax_snapshot_untruncated`]: drift guard, accessibility enablement,
    /// and the targeted re-attach retry. Separated so both snapshot flavors
    /// share one fetch policy.
    async fn ax_fetched_nodes(
        &self,
        origin: &url::Url,
    ) -> (Vec<crate::AxNode>, AxResyncCheck, u64) {
        let page_url = self.current_url().await.ok().flatten();
        let anchor = self.portal_anchor();
        let drifted = match &page_url {
            Some(url) => crate::is_anchored_drift(url, origin, anchor.as_ref()),
            None => true,
        };
        let (nodes, check, resyncs) = if drifted {
            let effective = anchor.as_ref().unwrap_or(origin);
            let live: &str = page_url.as_ref().map_or("unreadable", |url| url.as_str());
            let mut check = AxResyncCheck::new(0, page_url.as_ref());
            check.cdp_error = Some(crate::drift_error_line(effective, live));
            (Vec::new(), check, 0)
        } else if let Err(error) = self.enable_accessibility().await {
            let mut check = AxResyncCheck::new(0, page_url.as_ref());
            check.cdp_error = Some(error.to_string());
            (Vec::new(), check, 0)
        } else {
            fetch_with_targeted_retry(
                || self.fetch_ax_nodes(),
                || self.reattach_active_target(page_url.as_ref()),
                page_url.as_ref(),
            )
            .await
        };
        (nodes, check, resyncs)
    }
}

/// Fetch-then-retry core behind [`ManagedBrowser::ax_snapshot`]: run one
/// tree fetch; when it errors or returns zero nodes, re-attach and retry
/// exactly once. Returns the final nodes plus the first observation's
/// check and whether the retry ran. Separated so hermetic tests can drive
/// the policy over injected fetch/reattach fns without a browser.
pub async fn fetch_with_targeted_retry<F, Fut, R, Rf>(
    mut fetch: F,
    mut reattach: R,
    page_url: Option<&url::Url>,
) -> (Vec<AxNode>, AxResyncCheck, u64)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<AxNode>, String>>,
    R: FnMut() -> Rf,
    Rf: std::future::Future<Output = ()>,
{
    let first = fetch().await;
    let check = match &first {
        Ok(nodes) => AxResyncCheck::new(nodes.len(), page_url),
        Err(message) => {
            let mut check = AxResyncCheck::new(0, page_url);
            check.cdp_error = Some(message.clone());
            check
        }
    };
    let mut nodes = first.unwrap_or_default();
    if check.cdp_error.is_some() || check.node_count == 0 {
        reattach().await;
        if let Ok(fetched) = fetch().await {
            nodes = fetched;
        }
        return (nodes, check, 1);
    }
    (nodes, check, 0)
}
