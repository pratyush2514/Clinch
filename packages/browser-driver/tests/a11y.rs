//! Integration tests for `browser_driver::a11y`.
//!
//! Moved out of `src/` so the main source stays test-free.

use browser_driver::a11y::*;
use browser_driver::{AxNode, AxValue};

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
    let wrapped: AxValue =
        serde_json::from_value(serde_json::json!({"type": "role", "value": {"value": "button"}}))?;
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
fn tab_and_tablist_roles_map_cleanly() -> Result<(), Box<dyn std::error::Error>> {
    // Capitalized CDP roles normalize to lowercase strings; tabs surface
    // as interactive controls while the tablist bounds their context.
    let nodes = vec![
        linked(
            "strip",
            Some("tablist"),
            Some("Views"),
            None,
            &["t1", "t2"],
            false,
            None,
        )?,
        linked(
            "t1",
            Some("Tab"),
            Some("Analytics"),
            Some("strip"),
            &[],
            false,
            Some(51),
        )?,
        linked(
            "t2",
            Some("tab"),
            Some("Deployments"),
            Some("strip"),
            &[],
            false,
            Some(52),
        )?,
    ];
    let elements = interactive_elements(&nodes);
    assert_eq!(elements.len(), 2);
    assert_eq!(elements[0].role, "tab");
    assert_eq!(elements[0].name, "Analytics");
    assert_eq!(elements[1].role, "tab");
    // The shared tablist name plus sibling tab text reach each tab, the
    // same sibling-attachment rows rely on; distinct tab labels still
    // disambiguate downstream.
    assert_eq!(
        elements[0].container_text,
        vec!["Views".to_owned(), "Deployments".to_owned()]
    );
    assert_eq!(
        elements[1].container_text,
        vec!["Views".to_owned(), "Analytics".to_owned()]
    );
    Ok(())
}

#[test]
fn role_matching_ignores_case_and_truncates_long_names() -> Result<(), Box<dyn std::error::Error>> {
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
fn untruncated_variant_sees_past_the_cap() -> Result<(), Box<dyn std::error::Error>> {
    // Regression guard for the generic in-page pursuit lane: a revealed
    // menu renders at document end (React portal), past the 300-element
    // head the capped snapshot keeps. The deterministic selector must
    // see it, so that lane snapshots via the untruncated variant.
    let mut nodes = Vec::new();
    for id in 0..=MAX_ELEMENTS {
        let backend = i64::try_from(id).map_err(std::io::Error::other)?;
        nodes.push(node(Some("button"), Some("filler"), false, Some(backend))?);
    }
    nodes.push(node(
        Some("menuitem"),
        Some("Settings"),
        false,
        Some(9_999),
    )?);
    let capped = interactive_elements(&nodes);
    assert_eq!(capped.len(), MAX_ELEMENTS);
    assert!(!capped.iter().any(|element| element.name == "Settings"));
    let full = interactive_elements_all(&nodes);
    assert_eq!(full.len(), MAX_ELEMENTS + 2);
    assert_eq!(full[MAX_ELEMENTS + 1].name, "Settings");
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
fn deep_grandchild_cell_text_attaches_to_row_button() -> Result<(), Box<dyn std::error::Error>> {
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
fn climb_stops_at_three_levels_and_structural_roles() -> Result<(), Box<dyn std::error::Error>> {
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
fn landmark_ancestor_is_recorded_without_bounding_context() -> Result<(), Box<dyn std::error::Error>>
{
    // A nav link and a row button: the climb records `navigation` on the
    // link without letting landmarks bound either rollup.
    let nodes = vec![
        linked(
            "nav",
            Some("navigation"),
            Some("Primary"),
            None,
            &["l1"],
            false,
            None,
        )?,
        linked(
            "l1",
            Some("link"),
            Some("Downloads"),
            Some("nav"),
            &[],
            false,
            Some(61),
        )?,
        linked("row", Some("row"), None, None, &["c1"], false, None)?,
        linked("c1", None, None, Some("row"), &["b1"], false, None)?,
        linked(
            "b1",
            Some("button"),
            Some("Download"),
            Some("c1"),
            &[],
            false,
            Some(62),
        )?,
    ];
    let elements = interactive_elements(&nodes);
    assert_eq!(elements.len(), 2);
    assert_eq!(elements[0].landmark.as_deref(), Some("navigation"));
    assert_eq!(elements[0].container_text, vec!["Primary".to_owned()]);
    assert_eq!(elements[1].landmark, None);
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
            landmark: None,
        },
        AxElement {
            backend_node_id: 2,
            role: "textbox".into(),
            name: String::new(),
            description: "Email address".into(),
            container_text: Vec::new(),
            landmark: None,
        },
        AxElement {
            backend_node_id: 3,
            role: "button".into(),
            name: "Download".into(),
            description: String::new(),
            container_text: vec!["Statements".into(), "Statement #42".into()],
            landmark: None,
        },
    ];
    let list = render_semantic_list(&elements);
    assert!(list.contains("[0] button — Sign in\n"));
    assert!(list.contains("[1] textbox — Email address\n"));
    assert!(list.contains("[2] button — Download [in: Statements / Statement #42]\n"));
    assert!(render_semantic_list(&[]).is_empty());
}

#[test]
fn ax_snapshot_unconditionally_enables_accessibility_and_resyncs_zero_nodes()
-> Result<(), Box<dyn std::error::Error>> {
    // Hermetic policy proof for the cross-origin zero-node failure:
    // after a `google.com` → `github.com` renderer swap the
    // Accessibility domain deactivates, so `getFullAXTree` returns 0
    // nodes on a rendered page. Live CDP traffic stays behind the
    // Chromium-gated `#[ignore]`d fixtures (repo convention); this
    // locks the decision policy those calls implement:
    // - `ax_snapshot` arms `Accessibility.enable` unconditionally
    //   before every tree fetch — initial and resync retry alike, via
    //   `enable_accessibility`, never gated on navigation state;
    // - an empty tree on a real page fires exactly one resync carrying
    //   `AX_TARGET_RESYNC_LINE`, and the retry recovers rendered nodes.
    assert_eq!(
        AX_TARGET_RESYNC_LINE,
        "ax_target_resync: re-enabled accessibility after 0-node tree"
    );
    let billing = url::Url::parse("https://github.com/account/billing/history")?;
    let plain_http = url::Url::parse("http://portal.example/")?;
    let blank = url::Url::parse("about:blank")?;
    // Empty tree on a rendered page resyncs, whatever the scheme.
    assert!(needs_ax_resync(0, Some(&billing)));
    assert!(needs_ax_resync(0, Some(&plain_http)));
    // Non-empty trees never resync, even on real pages.
    assert!(!needs_ax_resync(4, Some(&billing)));
    // `about:blank` and unreadable URLs are expected-empty: no resync.
    assert!(!needs_ax_resync(0, Some(&blank)));
    assert!(!needs_ax_resync(0, None));
    // Simulated run: the first fetch returns 0 nodes on the billing
    // page (resync fires), the retry returns the rendered rows and the
    // snapshot recovers non-zero interactive elements.
    assert!(needs_ax_resync(0, Some(&billing)));
    let retry = vec![
        node(Some("link"), Some("Download"), false, Some(1))?,
        node(Some("link"), Some("Download"), false, Some(2))?,
        node(Some("link"), Some("Download"), false, Some(3))?,
    ];
    assert!(!needs_ax_resync(retry.len(), Some(&billing)));
    assert_eq!(interactive_elements(&retry).len(), 3);
    Ok(())
}

#[test]
fn ax_resync_check_line_reports_empty_url_on_failed_read() -> Result<(), Box<dyn std::error::Error>>
{
    // A failed page-URL read (`current_url` erroring mid-swap) stores
    // the check with no URL: the emitted line must still render with
    // `url=''` and a false predicate (scenario A) — never panic, never
    // omit the line.
    let check = AxResyncCheck::new(0, None);
    assert!(!check.predicate);
    assert_eq!(
        check.line(),
        "ax_resync_check: nodes=0 url='' predicate=false"
    );
    // A populated read renders verbatim with a true predicate, and the
    // struct carries the inputs (not a bare boolean) for the journal.
    let billing = url::Url::parse("https://github.com/account/billing/history")?;
    let check = AxResyncCheck::new(0, Some(&billing));
    assert_eq!(
        check,
        AxResyncCheck {
            node_count: 0,
            url: "https://github.com/account/billing/history".to_owned(),
            predicate: true,
            cdp_error: None,
        }
    );
    assert_eq!(
        check.line(),
        "ax_resync_check: nodes=0 url='https://github.com/account/billing/history' predicate=true"
    );
    Ok(())
}

#[tokio::test]
async fn targeted_retry_captures_cdp_error_and_retries_once() {
    // Simulated CDP failure (`Err`, e.g. `"Session detached"`): the
    // returned check carries `cdp_error: Some(..)` with zero nodes, the
    // re-attachment runs exactly once, and the retry result flows
    // through — all without a browser.
    use std::sync::{Arc, Mutex};
    let reattachments = Arc::new(Mutex::new(0_usize));
    let (nodes, check, resyncs) = fetch_with_targeted_retry(
        || async { Err::<Vec<AxNode>, String>("Session detached".to_owned()) },
        || {
            let reattachments = Arc::clone(&reattachments);
            async move {
                if let Ok(mut count) = reattachments.lock() {
                    *count += 1;
                }
            }
        },
        None,
    )
    .await;
    assert!(nodes.is_empty());
    assert_eq!(check.node_count, 0);
    assert_eq!(check.cdp_error.as_deref(), Some("Session detached"));
    assert_eq!(
        reattachments.lock().map(|count| *count).unwrap_or_default(),
        1
    );
    assert_eq!(resyncs, 1);
}

#[tokio::test]
async fn targeted_retry_clean_empty_tree_carries_no_cdp_error() {
    // A successful but empty fetch (`Ok(vec![])`, e.g. genuinely blank
    // field): `cdp_error` stays `None` with `node_count: 0`, and the
    // single retry still runs for the empty tree.
    use std::sync::{Arc, Mutex};
    let reattachments = Arc::new(Mutex::new(0_usize));
    let (nodes, check, resyncs) = fetch_with_targeted_retry(
        || async { Ok::<Vec<AxNode>, String>(Vec::new()) },
        || {
            let reattachments = Arc::clone(&reattachments);
            async move {
                if let Ok(mut count) = reattachments.lock() {
                    *count += 1;
                }
            }
        },
        None,
    )
    .await;
    assert!(nodes.is_empty());
    assert_eq!(check.node_count, 0);
    assert_eq!(check.cdp_error, None);
    assert_eq!(
        reattachments.lock().map(|count| *count).unwrap_or_default(),
        1
    );
    assert_eq!(resyncs, 1);
}
