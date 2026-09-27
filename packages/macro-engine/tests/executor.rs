//! Integration tests for `macro_engine::executor`.
//!
//! Moved out of `src/executor.rs` so the main source stays test-free.

use macro_engine::executor::*;

fn element(role: &str, name: &str) -> AxElement {
    AxElement {
        backend_node_id: 1,
        role: role.into(),
        name: name.into(),
        description: String::new(),
        container_text: Vec::new(),
        landmark: None,
    }
}

fn intent(role: &str, label: &str) -> SemanticIntent {
    SemanticIntent {
        role: role.into(),
        label_query: label.into(),
        container_query: None,
        // Legacy shapes predate the pass-through prompt: empty contributes
        // no coverage signal, keeping their totals exactly as before.
        raw_prompt: String::new(),
        ordinal_index: None,
        is_last: false,
        is_plural: false,
        entry_url: None,
        primary_target_noun: None,
    }
}

fn scoped_intent(role: &str, label: &str, container: &str) -> SemanticIntent {
    SemanticIntent {
        role: role.into(),
        label_query: label.into(),
        container_query: Some(container.into()),
        raw_prompt: String::new(),
        ordinal_index: None,
        is_last: false,
        is_plural: false,
        entry_url: None,
        primary_target_noun: None,
    }
}

/// Intent carrying the user's verbatim words for sub-token coverage.
fn prose_intent(role: &str, label: &str, container: Option<&str>, raw: &str) -> SemanticIntent {
    SemanticIntent {
        role: role.into(),
        label_query: label.into(),
        container_query: container.map(str::to_owned),
        raw_prompt: raw.into(),
        ordinal_index: None,
        is_last: false,
        is_plural: false,
        entry_url: None,
        primary_target_noun: None,
    }
}

/// Intent carrying an explicit position among eligible candidates.
fn ordinal_intent(role: &str, label: &str, index: Option<usize>, last: bool) -> SemanticIntent {
    SemanticIntent {
        role: role.into(),
        label_query: label.into(),
        container_query: None,
        raw_prompt: String::new(),
        ordinal_index: index,
        is_last: last,
        is_plural: false,
        entry_url: None,
        primary_target_noun: None,
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
    recovery.description = "Search reports".into();
    let Some(resolved) = resolve_intent(&[recovery], &intent("textbox", "reports")) else {
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
fn container_evidence_grounds_otherwise_bare_controls() {
    let plain = element("button", "Download");
    let contextual = AxElement {
        backend_node_id: 7,
        container_text: vec!["Statements".into(), "Statement #42".into()],
        ..element("button", "Download")
    };
    // Name alone cannot match: the container carries the query.
    let Some(resolved) =
        resolve_intent(&[plain, contextual.clone()], &intent("button", "statement"))
    else {
        panic!("container match resolves")
    };
    assert_eq!(resolved.score, 1);
    assert_eq!(resolved.element.backend_node_id, 7);
    // Equal name scores: container hits win over document order.
    let plain_file = AxElement {
        backend_node_id: 11,
        ..element("button", "Download statements file")
    };
    let contextual_file = AxElement {
        backend_node_id: 22,
        container_text: vec!["Statements".into()],
        ..element("button", "Download statements archive")
    };
    let Some(resolved) = resolve_intent(
        &[plain_file, contextual_file],
        &intent("button", "statements"),
    ) else {
        panic!("container tie-break resolves")
    };
    assert_eq!(resolved.element.backend_node_id, 22);
}

#[test]
fn container_scope_boosts_in_scope_controls_without_veto() {
    let in_scope = AxElement {
        backend_node_id: 11,
        container_text: vec!["0LWQXDWW".into()],
        ..element("link", "Download report")
    };
    let out_of_scope = AxElement {
        backend_node_id: 22,
        container_text: vec!["Other".into()],
        ..element("link", "Download report")
    };
    // Same labels: the scoped control wins on container overlap.
    let Some(resolved) = resolve_intent(
        &[out_of_scope.clone(), in_scope.clone()],
        &scoped_intent("link", "download", "0LWQXDWW"),
    ) else {
        panic!("scoped intent resolves")
    };
    assert_eq!(resolved.element.backend_node_id, 11);
    // No veto: a lone out-of-scope control still resolves best-effort
    // instead of failing closed.
    let Some(resolved) = resolve_intent(
        &[out_of_scope],
        &scoped_intent("link", "download", "0LWQXDWW"),
    ) else {
        panic!("partial evidence never vetoes")
    };
    assert_eq!(resolved.element.backend_node_id, 22);
    // Scopeless behavior is unchanged by the new field.
    assert!(resolve_intent(&[in_scope], &intent("link", "download")).is_some());
}

#[test]
fn partial_container_overlap_still_grounds() {
    // The anti-veto core: no candidate contains the full scope, yet the
    // partially matching row outscores the unrelated one and resolves.
    let partial = AxElement {
        backend_node_id: 61,
        container_text: vec!["0LWQXDWW".into()],
        ..element("button", "Download")
    };
    let unrelated = AxElement {
        backend_node_id: 62,
        container_text: vec!["Welcome tour".into()],
        ..element("button", "Download")
    };
    let scope = "0lwqxdww visa 2919";
    let Some(resolved) = resolve_intent(
        &[unrelated.clone(), partial.clone()],
        &scoped_intent("button", "download", scope),
    ) else {
        panic!("partial overlap resolves")
    };
    assert_eq!(resolved.element.backend_node_id, 61);
    // And the partial row resolves on its own: weak evidence lowers the
    // total (2.0 + 10.0 / 3) but never disqualifies.
    assert!(resolve_intent(&[partial], &scoped_intent("button", "download", scope)).is_some());
}

#[test]
fn below_threshold_resolves_to_nothing_with_diagnostic() {
    // Label miss plus no container backing: total 0.0 stays home.
    let elements = vec![element("button", "Download")];
    let intent = intent("button", "Sign in");
    assert!(resolve_intent(&elements, &intent).is_none());
    let diagnostic = grounding_diagnostic(&elements, &intent);
    assert!(diagnostic.contains("Grounding failed."));
    assert!(diagnostic.contains("Target container_query: 'none'"));
    assert!(diagnostic.contains("Evaluated 1 candidates"));
    assert!(diagnostic.contains("Candidate 0 text: 'Download'"));
    // Nothing role-matching: zero candidates, still a readable line.
    let empty = grounding_diagnostic(&[], &intent);
    assert!(empty.contains("Evaluated 0 candidates"));
    // Malformed intents diagnose instead of panicking.
    let broken = SemanticIntent {
        role: String::new(),
        label_query: String::new(),
        container_query: None,
        raw_prompt: String::new(),
        ordinal_index: None,
        is_last: false,
        is_plural: false,
        entry_url: None,
        primary_target_noun: None,
    };
    assert!(grounding_diagnostic(&elements, &broken).contains("malformed"));
}

#[test]
fn container_bounds_reject_empty_and_oversized_scopes() {
    let elements = vec![element("button", "Pay")];
    let empty = SemanticIntent {
        role: "button".into(),
        label_query: "Pay".into(),
        container_query: Some("   ".into()),
        raw_prompt: String::new(),
        ordinal_index: None,
        is_last: false,
        is_plural: false,
        entry_url: None,
        primary_target_noun: None,
    };
    assert!(empty.validate().is_err());
    assert!(resolve_intent(&elements, &empty).is_none());
    let huge = SemanticIntent {
        role: "button".into(),
        label_query: "Pay".into(),
        container_query: Some("x".repeat(513)),
        raw_prompt: String::new(),
        ordinal_index: None,
        is_last: false,
        is_plural: false,
        entry_url: None,
        primary_target_noun: None,
    };
    assert!(huge.validate().is_err());
    assert!(resolve_intent(&elements, &huge).is_none());
    let huge_raw = SemanticIntent {
        role: "button".into(),
        label_query: "Pay".into(),
        container_query: None,
        raw_prompt: "x".repeat(2001),
        ordinal_index: None,
        is_last: false,
        is_plural: false,
        entry_url: None,
        primary_target_noun: None,
    };
    assert!(huge_raw.validate().is_err());
    assert!(resolve_intent(&elements, &huge_raw).is_none());
}

#[test]
fn saved_payloads_without_the_key_still_parse() -> Result<(), Box<dyn std::error::Error>> {
    // Backward compatibility: envelopes written before `containerQuery`
    // existed deserialize with no scope.
    let parsed: SemanticIntent = serde_json::from_str(r#"{"role":"button","labelQuery":"Pay"}"#)?;
    assert_eq!(parsed.container_query, None);
    assert!(parsed.validate().is_ok());
    let rendered = serde_json::to_string(&scoped_intent("link", "download", "0LWQXDWW"))?;
    assert!(rendered.contains("\"containerQuery\":\"0LWQXDWW\""));
    let revived: SemanticIntent = serde_json::from_str(&rendered)?;
    assert_eq!(revived.container_query.as_deref(), Some("0LWQXDWW"));
    assert!(
        serde_json::from_str::<SemanticIntent>(
            r#"{"role":"button","labelQuery":"Pay","containerQuery":7}"#
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn intent_shape_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_json::to_string(&intent("button", "Sign in"))?;
    let parsed: SemanticIntent = serde_json::from_str(&json)?;
    assert_eq!(parsed.role, "button");
    assert!(serde_json::from_str::<SemanticIntent>(r#"{"role":"button"}"#).is_err());
    Ok(())
}

#[test]
fn container_matching_is_case_insensitive_and_whitespace_normalized() {
    // Uppercase DOM text matches a lowercased user prompt with full
    // token overlap, whitespace and case normalized.
    let uppercase = AxElement {
        backend_node_id: 11,
        container_text: vec!["0LWQXDWW".into()],
        ..element("link", "Download report")
    };
    let other = AxElement {
        backend_node_id: 22,
        container_text: vec!["Other".into()],
        ..element("link", "Download report")
    };
    let Some(resolved) = resolve_intent(
        &[other, uppercase],
        &scoped_intent("link", "download", "0lwqxdww"),
    ) else {
        panic!("lowercase scope matches uppercase container");
    };
    assert_eq!(resolved.element.backend_node_id, 11);
    // Whitespace-collapsed scopes match padded container text.
    let padded = AxElement {
        backend_node_id: 31,
        container_text: vec!["  June   12  ".into()],
        ..element("link", "Download")
    };
    assert!(container_overlap("June 12", &padded) > 0.99);
    assert!(container_overlap("  june   12 ", &padded) > 0.99);
}

#[test]
fn textbox_never_steals_click_intents_despite_perfect_overlap() {
    // Input protection is structural: a search box named exactly
    // "Download" sitting inside the scoped row would total 13.0, yet a
    // click intent (`button`) must still act on the plainer button.
    let search_box = AxElement {
        backend_node_id: 81,
        description: String::new(),
        container_text: vec!["0LWQXDWW".into()],
        ..element("textbox", "Download")
    };
    let plain_button = AxElement {
        backend_node_id: 82,
        container_text: Vec::new(),
        ..element("button", "Download")
    };
    let Some(resolved) = resolve_intent(
        &[search_box, plain_button],
        &scoped_intent("button", "download", "0lwqxdww"),
    ) else {
        panic!("click intent resolves to a button")
    };
    assert_eq!(resolved.element.backend_node_id, 82);
    assert_eq!(resolved.element.role, "button");
}

#[test]
fn tab_roles_admitted_and_grounded_for_click_intents() {
    // Tabs carry no inferred intent role, yet `open the Analytics` (role
    // `link`) must ground on the Analytics tab — admitted one-way into
    // the clickable pool and scored normally from there.
    let analytics = AxElement {
        backend_node_id: 91,
        container_text: vec!["Views".into()],
        ..element("tab", "Analytics")
    };
    let deployments = AxElement {
        backend_node_id: 92,
        container_text: vec!["Views".into()],
        ..element("tab", "Deployments")
    };
    for (label, winner) in [("analytics", 91), ("deployments", 92)] {
        let Some(resolved) = resolve_intent(
            &[analytics.clone(), deployments.clone()],
            &intent("link", label),
        ) else {
            panic!("tab grounds for click intent: {label}")
        };
        assert_eq!(resolved.element.backend_node_id, winner);
        assert_eq!(resolved.element.role, "tab");
        assert_eq!(resolved.score, 3);
    }
    // Button intents admit tabs too; fill intents never do, and the
    // reverse direction stays closed (tabs match nothing for textboxes).
    assert!(
        resolve_intent(
            std::slice::from_ref(&analytics),
            &intent("button", "analytics")
        )
        .is_some()
    );
    assert!(resolve_intent(&[analytics], &intent("textbox", "analytics")).is_none());
    // One-directional means legacy distinctions hold: buttons still
    // never match link intents.
    assert!(resolve_intent(&[element("button", "Pricing")], &intent("link", "pricing")).is_none());
}

#[test]
fn lowercase_id_queries_win_decisively_over_unmatched_rows() {
    // The acceptance spelling: a lowercased `1tuewzua` scope against an
    // uppercase container resolves to that row — the 10x container boost
    // puts it far above the scopeless-strength alternatives.
    let target = AxElement {
        backend_node_id: 71,
        container_text: vec!["1TUEWZUA".into(), "Visa ending in 2919".into()],
        ..element("button", "Download")
    };
    let decoy = AxElement {
        backend_node_id: 72,
        container_text: vec!["0FLI32JA".into()],
        ..element("button", "Download")
    };
    let Some(resolved) = resolve_intent(
        &[decoy, target],
        &scoped_intent("button", "download", "1tuewzua"),
    ) else {
        panic!("lowercase ID scope resolves")
    };
    assert_eq!(resolved.element.backend_node_id, 71);
}

#[test]
fn container_matching_is_fuzzy_across_split_attributes() {
    // One row carries amount, status, and date in separate items; the
    // multi-attribute scope matches them in any order or case.
    let row = AxElement {
        backend_node_id: 41,
        container_text: vec!["$4.00".into(), "Declined".into(), "June 12".into()],
        ..element("button", "Download")
    };
    let other = AxElement {
        backend_node_id: 42,
        container_text: vec!["$9.00".into(), "Paid".into(), "June 13".into()],
        ..element("button", "Download")
    };
    for scope in [
        "$4 declined June 12",
        "declined $4 june 12",
        "  $4   DECLINED june 12 ",
    ] {
        let Some(resolved) = resolve_intent(
            &[other.clone(), row.clone()],
            &scoped_intent("button", "download", scope),
        ) else {
            panic!("fuzzy scope resolves: {scope}");
        };
        assert_eq!(resolved.element.backend_node_id, 41, "{scope}");
    }
    // Soft scoring never drops on partial attributes: the wrong-amount
    // row still resolves best-effort (3/4 overlap), while the exact row
    // outranks it head-to-head.
    assert!(
        resolve_intent(
            std::slice::from_ref(&row),
            &scoped_intent("button", "download", "$9 declined June 12")
        )
        .is_some()
    );
    let Some(resolved) = resolve_intent(
        &[row.clone(), other.clone()],
        &scoped_intent("button", "download", "$4 declined June 12"),
    ) else {
        panic!("exact scope outranks partial")
    };
    assert_eq!(resolved.element.backend_node_id, 41);
    // Plurals match singulars and noise words never veto.
    let invoices = AxElement {
        backend_node_id: 51,
        container_text: vec!["Invoice 0LWQXDWW".into()],
        ..element("button", "Download")
    };
    assert!(container_overlap("invoices", &invoices) > 0.99);
    assert!(container_overlap("second invoice", &invoices) > 0.99);
    assert!(
        container_overlap(
            "receipt for last week",
            &AxElement {
                backend_node_id: 52,
                container_text: vec!["Receipt".into()],
                ..element("button", "Download")
            }
        ) > 0.99
    );
}

#[test]
fn subtoken_coverage_counts_label_tokens_in_prompt() {
    // Pure set coverage, no dictionaries: verbose prose covers short
    // labels, unrelated pairs score nothing, empties stay zero.
    assert!(
        calculate_subtoken_coverage(
            "can you please go ahead and click on the submit application button for me",
            "Submit Application",
        ) > 0.99
    );
    assert!(calculate_subtoken_coverage("open the ANALYTICS page", "analytics") > 0.99);
    assert!(calculate_subtoken_coverage("open dashboard", "Analytics") < 0.01);
    assert!(calculate_subtoken_coverage("anything at all", "") < 0.01);
    assert!(calculate_subtoken_coverage("", "Download") < 0.01);
}

#[test]
fn long_prose_prompt_grounds_to_short_button_label() {
    // The stripped label (`button`) misses entirely, yet full label
    // coverage (100.0) grounds the verbose prompt with no verb lists.
    let raw = "can you please go ahead and click on the submit application button for me";
    let target = element("button", "Submit Application");
    let intent = prose_intent("button", "button", None, raw);
    let Some(resolved) = resolve_intent(std::slice::from_ref(&target), &intent) else {
        panic!("long prose grounds to the short label")
    };
    assert_eq!(resolved.element.name, "Submit Application");
    let diagnostic = grounding_diagnostic(std::slice::from_ref(&target), &intent);
    assert!(diagnostic.contains("(total 100.00)"), "{diagnostic}");
}

#[test]
fn search_input_container_text_never_steals_link_click() {
    // The search box's container matches, but the Tier 1 gate admits no
    // textbox into a link intent — so it loses alone (no match) and
    // head-to-head, regardless of overlap scores.
    let raw = "open the analytics page";
    let intent = prose_intent("link", "analytics", None, raw);
    let link = element("link", "Analytics");
    let search_box = AxElement {
        backend_node_id: 102,
        container_text: vec!["Analytics dashboard".into()],
        ..element("textbox", "Search")
    };
    assert!(resolve_intent(std::slice::from_ref(&search_box), &intent).is_none());
    let Some(resolved) = resolve_intent(&[search_box, link], &intent) else {
        panic!("link wins over the matching-container input")
    };
    assert_eq!(resolved.element.role, "link");
    assert_eq!(resolved.element.name, "Analytics");
}

#[test]
fn identical_buttons_disambiguated_by_container_context() {
    // Same names, same coverage: the row whose surroundings mention the
    // target wins by exactly the overlap boost (+10.0).
    let raw = "download invoice 0lwqxdww";
    let intent = prose_intent("button", "download", Some("0lwqxdww"), raw);
    let in_scope = AxElement {
        backend_node_id: 111,
        container_text: vec!["Invoice 0LWQXDWW".into()],
        ..element("button", "Download")
    };
    let out_of_scope = AxElement {
        backend_node_id: 112,
        container_text: vec!["Other".into()],
        ..element("button", "Download")
    };
    let elements = [out_of_scope, in_scope];
    let Some(resolved) = resolve_intent(&elements, &intent) else {
        panic!("scoped row wins")
    };
    assert_eq!(resolved.element.backend_node_id, 111);
    let diagnostic = grounding_diagnostic(&elements, &intent);
    assert!(diagnostic.contains("(total 113.00)"), "{diagnostic}");
    assert!(diagnostic.contains("(total 103.00)"), "{diagnostic}");
}

#[test]
fn ordinal_queries_select_nth_visual_candidate() {
    // Three identical controls in document order (the fixture's visual
    // order — true viewport sorting needs geometry plumbing): `2nd`
    // takes index 1, `last` takes the final one, out-of-range fails
    // closed instead of falling back to Candidate 0.
    let rows = vec![
        AxElement {
            backend_node_id: 121,
            ..element("button", "Invoice")
        },
        AxElement {
            backend_node_id: 122,
            ..element("button", "Invoice")
        },
        AxElement {
            backend_node_id: 123,
            ..element("button", "Invoice")
        },
    ];
    let Some(second) = resolve_intent(&rows, &ordinal_intent("button", "invoice", Some(1), false))
    else {
        panic!("ordinal 1 selects the second row")
    };
    assert_eq!(second.element.backend_node_id, 122);
    let Some(last) = resolve_intent(&rows, &ordinal_intent("button", "invoice", None, true)) else {
        panic!("is_last selects the final row")
    };
    assert_eq!(last.element.backend_node_id, 123);
    assert!(resolve_intent(&rows, &ordinal_intent("button", "invoice", Some(7), false)).is_none());
    // Without ordinals the top scorer still wins (unchanged default).
    let Some(top) = resolve_intent(&rows, &intent("button", "invoice")) else {
        panic!("scopeless default resolves")
    };
    assert_eq!(top.element.backend_node_id, 121);
}

#[test]
fn fast_path_replay_bypasses_resolver_in_sub_100ms() {
    // The stored intent is matched directly — this test builds it by
    // hand, so no resolver runs anywhere on this path — in microseconds,
    // with zero model cost by construction.
    let elements = vec![element("button", "Pay now"), element("button", "Cancel")];
    let intent = intent("button", "Pay now");
    let (resolved, metrics) = resolve_fast(&elements, &intent);
    let Some(resolved) = resolved else {
        panic!("stored signature matches directly")
    };
    assert_eq!(resolved.element.name, "Pay now");
    assert!(metrics.duration_ms < 100, "{}", metrics.duration_ms);
    // Bit-exact zero: the cost is assigned, never computed.
    assert_eq!(metrics.cost_usd.to_bits(), RESOLVE_COST_USD.to_bits());
    assert_eq!(RESOLVE_COST_USD.to_bits(), 0.0f64.to_bits());
}

#[test]
fn batch_execution_collects_and_returns_all_threshold_matches() {
    // Three identical controls, one plural intent: all three come back
    // in document order, capped rather than vetoed. A non-plural intent
    // refuses the batch path fail-closed.
    let rows = vec![
        AxElement {
            backend_node_id: 131,
            ..element("button", "Download")
        },
        AxElement {
            backend_node_id: 132,
            ..element("button", "Download")
        },
        AxElement {
            backend_node_id: 133,
            ..element("button", "Download")
        },
    ];
    let mut plural = intent("button", "download");
    plural.is_plural = true;
    let ResolveOutcome::BatchMatch(batch) = resolve_batch(&rows, &plural) else {
        panic!("plural intent batches every match")
    };
    assert_eq!(
        batch
            .iter()
            .map(|element| element.backend_node_id)
            .collect::<Vec<_>>(),
        vec![131, 132, 133]
    );
    assert!(batch.len() <= MAX_BATCH_CLICKS);
    let single = intent("button", "download");
    assert!(matches!(
        resolve_batch(&[], &single),
        ResolveOutcome::NoMatch(_)
    ));
}

#[test]
fn entry_url_triggers_navigation_if_route_mismatched() -> Result<(), Box<dyn std::error::Error>> {
    // Pure route decision: identical URLs stay put, any drift navigates.
    // Live firing (`goto` + load wait) needs Chromium, so the async
    // helper stays compile-checked while this locks the contract.
    let here = url::Url::parse("https://portal.example.com/invoices")?;
    let same = url::Url::parse("https://portal.example.com/invoices")?;
    let away = url::Url::parse("https://portal.example.com/settings")?;
    assert!(!entry_url_mismatched(&here, &same));
    assert!(entry_url_mismatched(&here, &away));
    Ok(())
}

#[test]
fn resolve_batch_ignores_sidebar_navigation_links() {
    // Three table-row controls plus three sidebar links with identical
    // labels: the batch must hold exactly the data rows. Sidebar links
    // carry container text too, so only the landmark — never emptiness —
    // may exclude them.
    let table = ["INV-001", "INV-002", "INV-003"];
    let mut elements = Vec::new();
    for (index, id) in table.iter().enumerate() {
        elements.push(AxElement {
            backend_node_id: i64::try_from(index + 1).unwrap_or(1),
            container_text: vec![(*id).into()],
            landmark: None,
            ..element("link", "Download")
        });
    }
    for (index, name) in ["Docs", "Billing", "Home"].iter().enumerate() {
        elements.push(AxElement {
            backend_node_id: i64::try_from(index + 11).unwrap_or(11),
            container_text: vec!["Primary".into(), (*name).into()],
            landmark: Some("navigation".into()),
            ..element("link", "Download")
        });
    }
    let mut plural = intent("link", "download");
    plural.is_plural = true;
    let ResolveOutcome::BatchMatch(batch) = resolve_batch(&elements, &plural) else {
        panic!("table rows batch together")
    };
    assert_eq!(
        batch
            .iter()
            .map(|element| element.backend_node_id)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[test]
fn drift_guard_allows_query_param_and_blob_changes() -> Result<(), Box<dyn std::error::Error>> {
    let entry = url::Url::parse("https://portal.example.com/invoices")?;
    // Query strings, hash fragments, and trailing-slash normalization
    // are not drift.
    for same in [
        "https://portal.example.com/invoices?sort=date",
        "https://portal.example.com/invoices#row-3",
        "https://portal.example.com/invoices?sort=date#row-3",
    ] {
        assert!(!url_drifted(&entry, &url::Url::parse(same)?), "{same}");
    }
    // Download handoffs never count as drift either.
    assert!(!url_drifted(
        &entry,
        &url::Url::parse("blob:https://portal.example.com/1")?
    ));
    // Origin and path changes do.
    for moved in [
        "https://portal.example.com/settings",
        "https://other.example.com/invoices",
        "http://portal.example.com/invoices",
    ] {
        assert!(url_drifted(&entry, &url::Url::parse(moved)?), "{moved}");
    }
    Ok(())
}

#[test]
fn drift_guard_halts_and_reports_failed_candidate_details_on_path_change()
-> Result<(), Box<dyn std::error::Error>> {
    // The halt payload names the failing control and the diverged page;
    // the live loop that builds it stays behind Chromium-gated try paths.
    let diverged = url::Url::parse("https://portal.example.com/settings")?;
    let outcome = halted_early(2, 2, &element("button", "Download"), &diverged);
    let ExecuteOutcome::HaltedEarly {
        reason,
        clicks_completed,
        failed_candidate_index,
        failed_candidate_label,
        diverged_url,
    } = outcome
    else {
        panic!("halt payload builds");
    };
    assert_eq!(reason, "UrlDriftDetected");
    assert_eq!(clicks_completed, 2);
    assert_eq!(failed_candidate_index, 2);
    assert_eq!(failed_candidate_label, "Download");
    assert_eq!(diverged_url, "https://portal.example.com/settings");
    Ok(())
}

#[test]
fn resolve_batch_requires_primary_noun_and_filters_modifier_only_matches() {
    // Modifier-only matches (`All issues` via `all`) score on coverage
    // alone; the noun anchor (`invoice`) keeps them out while row
    // controls — named or merely surrounded — stay in.
    let elements = vec![
        element("link", "All issues"),
        element("link", "All pull requests"),
        element("link", "Download invoice 1"),
        AxElement {
            backend_node_id: 4,
            container_text: vec!["INV-002".into()],
            ..element("link", "Download invoice 2")
        },
    ];
    let mut intent = prose_intent("link", "invoices", None, "download all my invoices");
    intent.is_plural = true;
    intent.primary_target_noun = Some("invoice".into());
    let ResolveOutcome::BatchMatch(batch) = resolve_batch(&elements, &intent) else {
        panic!("invoice rows batch together")
    };
    // Exactly the two invoice controls; modifier-only matches stay out.
    assert_eq!(
        batch
            .iter()
            .map(|element| element.name.clone())
            .collect::<Vec<_>>(),
        vec![
            "Download invoice 1".to_owned(),
            "Download invoice 2".to_owned()
        ]
    );
    // Without the anchor the modifier matches would flood back in.
    intent.primary_target_noun = None;
    let ResolveOutcome::BatchMatch(batch) = resolve_batch(&elements, &intent) else {
        panic!("ungated batch collects")
    };
    assert_eq!(batch.len(), 4);
}

#[test]
fn unmatched_intent_fails_closed_without_candidate_zero() {
    // Neither control shares a token with the prompt: both are evaluated
    // (the log names Candidate 0 and Candidate 1) and neither is acted
    // on — no document-order fallback click.
    let raw = "launch the quantum hyperdrive";
    let intent = prose_intent("button", "hyperdrive", None, raw);
    let elements = vec![
        element("button", "Download"),
        AxElement {
            backend_node_id: 2,
            ..element("button", "Settings")
        },
    ];
    assert!(resolve_intent(&elements, &intent).is_none());
    let diagnostic = grounding_diagnostic(&elements, &intent);
    assert!(
        diagnostic.contains("Evaluated 2 candidates"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("Candidate 0 text:"), "{diagnostic}");
    assert!(diagnostic.contains("Candidate 1 text:"), "{diagnostic}");
}

/// Plural invoice intent mirroring the billing-history run: link role,
/// `download` label, `invoice` noun anchor, batch collection on.
fn invoice_batch_intent() -> SemanticIntent {
    let mut intent = prose_intent("link", "download", None, "download all my invoices");
    intent.is_plural = true;
    intent.primary_target_noun = Some("invoice".into());
    intent
}

/// Payment-history rows as the AX tree reports them once the async table
/// renders: download links carrying invoice evidence in name or
/// surroundings, plus one sidebar link the landmark gate must exclude.
fn payment_history_rows() -> Vec<AxElement> {
    let mut rows = Vec::new();
    for (index, id) in ["INV-001", "INV-002", "INV-003"].iter().enumerate() {
        rows.push(AxElement {
            backend_node_id: i64::try_from(index + 1).unwrap_or(1),
            container_text: vec![(*id).into(), "Invoices".into()],
            landmark: None,
            ..element("link", "Download")
        });
    }
    rows.push(AxElement {
        backend_node_id: 11,
        container_text: vec!["Primary".into()],
        landmark: Some("navigation".into()),
        ..element("link", "Download")
    });
    rows
}

#[test]
fn settle_cadence_is_explicit_state_polling() {
    // Regression guard on the contracted cadence: 250 ms polls, 5000 ms
    // ceiling. Production waits reuse these; hermetic tests below pass
    // scaled values to stay fast.
    assert_eq!(SETTLE_POLL_MS, 250);
    assert_eq!(SETTLE_TIMEOUT_MS, 5000);
    // Probe text is the prompt-derived noun, falling back to the label.
    let anchored = invoice_batch_intent();
    assert_eq!(settle_probe_text(&anchored), "invoice");
    let bare = intent("link", "download");
    assert_eq!(settle_probe_text(&bare), "download");
}

/// Static header-only tree: what the AX snapshot holds before the async
/// table renders. The `Invoice` column header exists from page load, but
/// as a non-interactive `columnheader` it can never satisfy
/// [`resolve_batch`] — readiness needs row candidates, not header text.
fn header_only_tree() -> Vec<AxElement> {
    vec![AxElement {
        backend_node_id: 99,
        role: "columnheader".into(),
        name: "Invoice".into(),
        description: String::new(),
        container_text: Vec::new(),
        landmark: None,
    }]
}

#[tokio::test]
async fn settle_polling_waits_past_headers_for_row_candidates() {
    // The reported failure: `<th>Invoice</th>` exists on page load, so a
    // body-text check returns in <50 ms while the rows are still absent.
    // The loop must keep polling past header-only trees until row
    // candidate nodes land (header snapshots stand in for the first
    // ~10 ms of GitHub's ~500 ms async render; production cadence is
    // 250 ms polls / 5000 ms ceiling).
    use std::sync::{Arc, Mutex};
    let polls = Arc::new(Mutex::new(0_usize));
    let intent = invoice_batch_intent();
    // Headers alone never settle: no actionable candidate exists yet.
    assert!(matches!(
        resolve_batch(&header_only_tree(), &intent),
        ResolveOutcome::NoMatch(_)
    ));
    let seen = wait_for_candidates_with(
        {
            let polls = Arc::clone(&polls);
            move || {
                let polls = Arc::clone(&polls);
                async move {
                    let mut count = polls
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *count += 1;
                    // Rows render after the second poll; earlier polls
                    // see the header-only tree.
                    Some(if *count > 2 {
                        payment_history_rows()
                    } else {
                        header_only_tree()
                    })
                }
            }
        },
        &intent,
        std::time::Duration::from_millis(5),
        std::time::Duration::from_millis(500),
    )
    .await;
    assert!(seen, "polling observes the delayed rows");
    assert!(
        *polls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            > 1,
        "more than one poll ran before candidates landed"
    );
    let ResolveOutcome::BatchMatch(batch) = resolve_batch(&payment_history_rows(), &intent) else {
        panic!("settled rows batch together")
    };
    assert_eq!(
        batch
            .iter()
            .map(|element| element.backend_node_id)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[tokio::test]
async fn settle_timeout_fails_closed_when_rows_never_render() {
    // Headers forever, rows never: the loop must expire at its timeout
    // (scaled down here; production waits the full 5000 ms) and report
    // unready, and the field must fail closed with a diagnostic — never
    // an empty batch, never a blind navigation.
    let intent = invoice_batch_intent();
    let seen = wait_for_candidates_with(
        || async { Some(header_only_tree()) },
        &intent,
        std::time::Duration::from_millis(5),
        std::time::Duration::from_millis(50),
    )
    .await;
    assert!(!seen, "expiry reports unready");
    let empty: Vec<AxElement> = Vec::new();
    assert!(matches!(
        resolve_batch(&empty, &intent),
        ResolveOutcome::NoMatch(_)
    ));
    let diagnostic = grounding_diagnostic(&empty, &intent);
    assert!(
        diagnostic.contains("Evaluated 0 candidates"),
        "{diagnostic}"
    );
}

#[test]
fn batch_scoring_matches_recorded_replay_for_download_files() {
    // Replay-target alignment: the fast replay path (`resolve_fast`,
    // what recorded `Download files` steps use) and the batch collector
    // share one scoring model, so the replay winner must sit inside the
    // batch set — never a control the batch would refuse.
    let intent = invoice_batch_intent();
    let rows = payment_history_rows();
    let (replayed, metrics) = resolve_fast(&rows, &intent);
    assert_eq!(metrics.cost_usd.to_bits(), 0.0f64.to_bits());
    let Some(winner) = replayed else {
        panic!("replay resolves a winner")
    };
    let ResolveOutcome::BatchMatch(batch) = resolve_batch(&rows, &intent) else {
        panic!("batch collects")
    };
    assert_eq!(batch.len(), 3);
    assert!(
        batch
            .iter()
            .any(|element| element.backend_node_id == winner.element.backend_node_id),
        "replay winner is a batch member"
    );
}
