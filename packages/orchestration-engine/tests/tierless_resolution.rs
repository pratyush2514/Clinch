#![deny(unsafe_code)]
//! Resolution without a static portal route table.
//!
//! The curated `(portal, intent class) → URL` table and its `PORTALS`
//! whitelist are gone. This suite proves both jobs it used to do are still
//! covered end to end, hermetically, with no browser and no model:
//!
//! * **Tier 1** — the proven GitHub invoice route now lives in `SQLite` as a
//!   seeded playbook, so `"download github invoices"` routes to a saved
//!   workflow and that workflow loads back with its billing-history entry
//!   URL intact.
//! * **Direct-open ladder** — `"open amazon for me"` no longer scrapes a
//!   search page. The destination comes from the prompt's own explicit
//!   domain, a user-saved shortcut, or a structured directory — and an
//!   ungrounded site is a miss the caller turns into ask-and-learn guidance,
//!   never a fabricated destination.
//! * **Search-and-follow** — prompts that are not direct opens (retrieval
//!   verbs like `"find"`) still advance to the grounded search template,
//!   and the prompt's own grammar (never a site list) supplies the noun
//!   that picks the destination link out of a live results tree.

use browser_driver::AxElement;
use orchestration_engine::{CommandMatch, ResolutionContext, RouteSource};
use playbook_store::{PlaybookStore, Step};

/// One AX link candidate, as `interactive_elements` would report it.
fn link(backend_node_id: i64, name: &str, landmark: Option<&str>) -> AxElement {
    AxElement {
        backend_node_id,
        role: "link".into(),
        name: name.into(),
        description: String::new(),
        container_text: Vec::new(),
        landmark: landmark.map(str::to_owned),
    }
}

async fn store() -> Result<(tempfile::TempDir, PlaybookStore), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let pool = playbook_store::initialize(&dir.path().join("clinch.db")).await?;
    Ok((dir, PlaybookStore::new(pool)))
}

fn offline_ctx() -> ResolutionContext<'static> {
    // Production wiring: no account directory, no URL adapter, no parser.
    ResolutionContext {
        account_dir: None,
        llm: None,
        parser: None,
        shortcuts: None,
        site_search: None,
        domain_grounder: None,
        region_hint: "",
    }
}

#[tokio::test]
async fn tier_one_loads_seeded_playbooks_without_a_route_table()
-> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    let saved = store.list_playbooks().await?;
    let portal = url::Url::parse("https://github.com/")?;
    // The prompt the deleted table answered now resolves to a stored
    // workflow — tier 1, zero tokens, no browser.
    let Some(CommandMatch::Saved { id }) =
        orchestration_engine::resolve_command("download github invoices", Some(&portal), &saved)
    else {
        return Err("seeded playbook claims the invoice prompt".into());
    };
    // Loading re-validates, so a seeded row can never be unrunnable.
    let playbook = store.load_playbook(&id).await?;
    assert_eq!(playbook.name, "github-invoices");
    assert_eq!(playbook.origin, portal);
    let [Step::Semantic { intent }] = playbook.steps.as_slice() else {
        return Err("seeded playbook is one semantic step".into());
    };
    // The canonical billing-history route the static table used to hold now
    // travels with the workflow itself, so no tier has to invent it.
    assert_eq!(
        intent.entry_url.as_deref(),
        Some("https://github.com/account/billing/history")
    );
    assert_eq!(intent.primary_target_noun.as_deref(), Some("invoice"));
    // Single-target because this seed promises one invoice. `execute_step`
    // does honor `is_plural` (routing those steps through the batch gate), so
    // the flag is a statement about this workflow, not about the lane.
    assert!(!intent.is_plural);
    let elements = vec![
        link(1, "Invoices", Some("navigation")),
        link(2, "Invoice INV-001", None),
        link(3, "Invoice INV-002", None),
        link(4, "All issues", None),
    ];
    // Replay resolution: the seeded signature grounds an invoice link.
    let resolved =
        macro_engine::resolve_intent(&elements, intent).ok_or("seeded intent grounds a control")?;
    assert_eq!(resolved.element.role, "link");
    assert!(
        resolved.element.name.to_lowercase().contains("invoice"),
        "grounded {:?}",
        resolved.element.name
    );
    // Settle readiness runs through the batch gate, which is where the noun
    // earns its keep: `All issues` never counts as a rendered invoice row.
    let macro_engine::ResolveOutcome::BatchMatch(ready) =
        macro_engine::resolve_batch(&elements, intent)
    else {
        return Err("invoice rows satisfy settle readiness".into());
    };
    assert!(
        ready
            .iter()
            .all(|element| element.name.to_lowercase().contains("invoice")),
        "noun gate admits only invoice rows, got {:?}",
        ready
            .iter()
            .map(|element| element.name.clone())
            .collect::<Vec<_>>()
    );
    Ok(())
}

#[tokio::test]
async fn direct_open_miss_replaces_search_scrape() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    let saved = store.list_playbooks().await?;
    let search_origin = url::Url::parse("https://www.google.com/")?;
    let prompt = "open amazon for me";
    // No stored workflow claims this prompt, so it resolves as ephemeral.
    let Some(CommandMatch::Ephemeral { .. }) =
        orchestration_engine::resolve_command(prompt, Some(&search_origin), &saved)
    else {
        return Err("unclaimed prompt resolves ephemeral".into());
    };
    // With no explicit domain, no saved shortcut, and no directory key, the
    // ladder misses — and a miss is `None`, not a scraped search page. The
    // dispatcher turns this into "try the full domain or save a site
    // shortcut" instead of a fabricated destination.
    assert_eq!(
        orchestration_engine::resolve_entry_url(prompt, None, &offline_ctx()),
        None
    );
    // The prompt's own explicit domain grounds without any of that: no
    // shortcut, no key, no search.
    let Some(route) =
        orchestration_engine::resolve_entry_url("open amazon.in", None, &offline_ctx())
    else {
        return Err("explicit domain grounds".into());
    };
    assert_eq!(route.source, RouteSource::ExplicitDomain);
    assert_eq!(route.url.as_str(), "https://amazon.in/");
    Ok(())
}

#[tokio::test]
async fn search_and_follow_survives_for_non_direct_opens() -> Result<(), Box<dyn std::error::Error>>
{
    let (_dir, store) = store().await?;
    let saved = store.list_playbooks().await?;
    let search_origin = url::Url::parse("https://www.google.com/")?;
    // "find" is a retrieval verb, not an open verb, so this prompt is not a
    // direct open and keeps the grounded search path.
    let prompt = "find amazon";
    // No stored workflow claims this prompt, so it resolves as ephemeral.
    let Some(CommandMatch::Ephemeral { intent }) =
        orchestration_engine::resolve_command(prompt, Some(&search_origin), &saved)
    else {
        return Err("unclaimed prompt resolves ephemeral".into());
    };
    // The fixed search template, never an invented host.
    let Some(route) = orchestration_engine::resolve_entry_url(prompt, None, &offline_ctx()) else {
        return Err("grounded search resolves".into());
    };
    assert_eq!(route.source, RouteSource::SearchFallback);
    assert_eq!(
        route.url.as_str(),
        "https://www.google.com/search?q=find+amazon"
    );
    for guess in ["amazon.com", "amazon.in"] {
        assert!(!route.url.as_str().contains(guess), "no TLD guessing");
    }
    // Stage 2 noun comes from the prompt's grammar. This prompt has no
    // prepositional complement, so the direct object drives the follow.
    let site_context = orchestration_engine::parse_grammar(&intent.raw_prompt, None).site_context;
    assert_eq!(site_context, None);
    let noun = macro_engine::search_follow_noun(&intent, site_context.as_deref());
    assert_eq!(noun, "amazon");
    // Candidate selection over the results tree: engine chrome is skipped
    // even though it matches, and the non-matching competitor ahead of the
    // answer is skipped by the noun gate.
    let elements = vec![
        link(1, "Amazon", Some("navigation")),
        link(2, "Flipkart Online Shopping", None),
        link(3, "Amazon.in - Online Shopping", None),
    ];
    let picked =
        macro_engine::select_search_result(&elements, noun).ok_or("a result is selected")?;
    assert_eq!(picked.backend_node_id, 3);
    assert!(picked.landmark.is_none());
    // The destination is read from the click, never predicted: an absent
    // noun fails closed rather than clicking the first link on the page.
    assert!(macro_engine::select_search_result(&elements, "nonexistentbrand").is_none());
    assert!(macro_engine::select_search_result(&elements, "").is_none());
    Ok(())
}

#[tokio::test]
async fn prepositional_prompts_follow_the_site_and_batch_the_artifact()
-> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    let saved = store.list_playbooks().await?;
    let search_origin = url::Url::parse("https://www.google.com/")?;
    // A site nobody enumerated: the same two-slot grammar applies, which is
    // the point of deleting the whitelist.
    let prompt = "download all my invoices from acmecorp";
    let Some(CommandMatch::Ephemeral { intent }) =
        orchestration_engine::resolve_command(prompt, Some(&search_origin), &saved)
    else {
        return Err("plural prompt resolves ephemeral".into());
    };
    // Artifact noun rides the intent for batch gating…
    assert_eq!(intent.primary_target_noun.as_deref(), Some("invoice"));
    assert!(intent.is_plural);
    // …while the complement names the destination Stage 2 follows.
    let site_context = orchestration_engine::parse_grammar(&intent.raw_prompt, None).site_context;
    assert_eq!(site_context.as_deref(), Some("acmecorp"));
    let noun = macro_engine::search_follow_noun(&intent, site_context.as_deref());
    assert_eq!(noun, "acmecorp");
    // Stage 2 therefore lands on the site, not on an invoice article.
    let elements = vec![
        link(1, "Invoice templates and guides", None),
        link(2, "AcmeCorp — Billing portal", None),
    ];
    let picked =
        macro_engine::select_search_result(&elements, noun).ok_or("a result is selected")?;
    assert_eq!(picked.backend_node_id, 2);
    // The two slots stay distinct all the way down: the word Stage 2 follows
    // is never the word the batch gate anchors on. Swapping them would make
    // Stage 2 click an invoice article and the batch admit site chrome — the
    // exact collision a single noun slot caused.
    assert_ne!(noun, intent.primary_target_noun.as_deref().unwrap_or(""));
    // Batch gating on the artifact noun is covered by the seeded-playbook
    // test above, where the label and noun agree as they do after a real
    // Stage-2 landing.
    Ok(())
}
