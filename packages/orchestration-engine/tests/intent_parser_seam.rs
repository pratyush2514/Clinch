#![deny(unsafe_code)]
//! Confidence gate, fenced parser seam, bounded degradation, learning loop.
//!
//! Four properties, proven without a model, a network, or a browser:
//!
//! 1. Crisp commands never pay for a parser call.
//! 2. Irregular phrasing that grammar misreads defers to the seam, and the
//!    returned slots are what search-and-follow then grounds on.
//! 2. A stalled or absent parser degrades to an honest miss inside the
//!    timeout instead of hanging or failing the run.
//! 4. A saved run turns its prompt into a tier-1 hit on the next invocation.

use browser_driver::AxElement;
use orchestration_engine::{
    CommandMatch, Confidence, IntentParser, PARSER_TIMEOUT_MS, ParsedSlots, ResolutionContext,
    SlotSource, StubIntentParser, TestDoubleIntentParser,
};
use playbook_store::{PlaybookStore, Step};
use std::{sync::Arc, time::Duration};

/// A crisp, unambiguous command: verb, object, destination.
const CRISP: &str = "download invoices from github";
/// Irregular phrasing. Grammar matches the `on` cue and fills both slots,
/// but the artifact lands on the clause verb `owe` rather than a real
/// artifact — a misparse the confidence gate must catch.
const IRREGULAR: &str = "pull up what I owe on aws";

fn slots(action: &str, artifact: Option<&str>, site: Option<&str>) -> ParsedSlots {
    ParsedSlots {
        action: action.into(),
        artifact_noun: artifact.map(str::to_owned),
        site_context: site.map(str::to_owned),
    }
}

/// One AX link candidate, as `interactive_elements` would report it.
fn link(backend_node_id: i64, name: &str) -> AxElement {
    AxElement {
        backend_node_id,
        role: "link".into(),
        name: name.into(),
        description: String::new(),
        container_text: Vec::new(),
        landmark: None,
    }
}

fn ctx_with(parser: Option<&Arc<dyn IntentParser>>) -> ResolutionContext<'_> {
    ResolutionContext {
        account_dir: None,
        llm: None,
        parser,
        shortcuts: None,
        site_search: None,
        domain_grounder: None,
        region_hint: "",
    }
}

async fn store() -> Result<(tempfile::TempDir, PlaybookStore), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let pool = playbook_store::initialize(&dir.path().join("clinch.db")).await?;
    Ok((dir, PlaybookStore::new(pool)))
}

#[test]
fn test_high_confidence_grammar_bypasses_parser() {
    let double = Arc::new(TestDoubleIntentParser::answering(slots(
        "link",
        Some("wrong"),
        Some("wrong"),
    )));
    let parser: Arc<dyn IntentParser> = double.clone();
    let ctx = ctx_with(Some(&parser));
    // Crisp commands are answered by grammar alone: zero tokens, no call.
    let resolved = orchestration_engine::resolve_slots(CRISP, None, &ctx);
    assert_eq!(resolved.source, SlotSource::GrammarFastPath);
    assert_eq!(resolved.grammar.confidence, Confidence::High);
    assert_eq!(resolved.grammar.artifact_noun.as_deref(), Some("invoice"));
    assert_eq!(resolved.grammar.site_context.as_deref(), Some("github"));
    assert_eq!(double.calls(), 0, "fast path must not consult the parser");
    // The whole crisp corpus stays on the fast path, including the
    // direct-action shape with no destination complement.
    for prompt in [
        "download all my invoices from github",
        "grab my statements on zzyzxbank",
        "open amazon for me",
        "click pay now",
        "toggle dark mode",
    ] {
        let resolved = orchestration_engine::resolve_slots(prompt, None, &ctx);
        assert_eq!(
            resolved.source,
            SlotSource::GrammarFastPath,
            "{prompt} is crisp"
        );
    }
    assert_eq!(double.calls(), 0, "still no parser calls");
}

#[test]
fn test_low_confidence_prompt_defers_to_parser() {
    // Grammar alone misreads this: it fills both slots, so only the
    // confidence gate separates it from a crisp command.
    let unaided = orchestration_engine::parse_grammar(IRREGULAR, None);
    assert_eq!(unaided.confidence, Confidence::Low);
    assert_eq!(
        unaided.artifact_noun.as_deref(),
        Some("owe"),
        "the misparse the gate exists to catch"
    );
    let double = Arc::new(TestDoubleIntentParser::answering(slots(
        "link",
        Some("bills"),
        Some("aws"),
    )));
    let parser: Arc<dyn IntentParser> = double.clone();
    let resolved = orchestration_engine::resolve_slots(IRREGULAR, None, &ctx_with(Some(&parser)));
    assert_eq!(resolved.source, SlotSource::IntentParser);
    assert_eq!(double.calls(), 1, "exactly one bounded shot");
    // Parser slots normalize exactly like deterministic ones: `bills` stems
    // to `bill`, so one vocabulary reaches the matcher.
    assert_eq!(resolved.grammar.artifact_noun.as_deref(), Some("bill"));
    assert_eq!(resolved.grammar.site_context.as_deref(), Some("aws"));
    // And those slots are what search-and-follow grounds on: the AWS result
    // wins over the billing article that merely mentions the artifact.
    let intent = macro_engine::SemanticIntent {
        role: "link".into(),
        label_query: "owe".into(),
        container_query: None,
        raw_prompt: IRREGULAR.into(),
        ordinal_index: None,
        is_last: false,
        is_plural: false,
        entry_url: None,
        primary_target_noun: Some("owe".into()),
    };
    let noun = macro_engine::search_follow_noun(&intent, resolved.grammar.site_context.as_deref());
    assert_eq!(noun, "aws");
    let elements = vec![
        link(1, "What you owe: understanding cloud bills"),
        link(2, "AWS Billing and Cost Management Console"),
    ];
    let Some(picked) = macro_engine::select_search_result(&elements, noun) else {
        panic!("parser slots ground a result");
    };
    assert_eq!(picked.backend_node_id, 2);
}

#[test]
fn test_parser_timeout_degrades_to_honest_miss() {
    // A wedged provider costs the timeout and nothing more. The run neither
    // hangs nor fails: entry resolution degrades to an honest miss — there
    // is no search page to land on anymore.
    let double = Arc::new(TestDoubleIntentParser::stalling(Duration::from_millis(
        PARSER_TIMEOUT_MS * 4,
    )));
    let parser: Arc<dyn IntentParser> = double.clone();
    let started = std::time::Instant::now();
    let resolved = orchestration_engine::resolve_slots(IRREGULAR, None, &ctx_with(Some(&parser)));
    let elapsed = started.elapsed();
    assert_eq!(double.calls(), 1);
    assert_eq!(resolved.source, SlotSource::Ungrounded);
    assert!(
        elapsed < Duration::from_millis(PARSER_TIMEOUT_MS * 3),
        "bounded wait, took {elapsed:?}"
    );
    // Tier 2C: no entry URL is produced, so no navigation happens.
    let ctx = ctx_with(Some(&parser));
    assert_eq!(
        orchestration_engine::resolve_entry_url(IRREGULAR, None, &ctx),
        None,
        "degradation is a miss, not a search page"
    );
    // Offline in the other sense — no parser configured at all, and the
    // shipped stub — behave identically. Degradation is the default, not an
    // error path.
    let stub: Arc<dyn IntentParser> = Arc::new(StubIntentParser);
    for context in [ctx_with(None), ctx_with(Some(&stub))] {
        let resolved = orchestration_engine::resolve_slots(IRREGULAR, None, &context);
        assert_eq!(resolved.source, SlotSource::Ungrounded);
        // Low-confidence grammar slots still travel: they are weak evidence,
        // but discarding them would be worse than keeping them.
        assert_eq!(resolved.grammar.confidence, Confidence::Low);
    }
}

#[tokio::test]
async fn test_learning_loop_converts_parsed_run_to_tier_1_cache_hit()
-> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store) = store().await?;
    let portal = url::Url::parse("https://aws.amazon.com/")?;
    // First run: nothing stored answers this phrasing, so it resolves
    // dynamically (and, in the live lane, through the parser seam).
    let saved = store.list_playbooks().await?;
    assert!(
        matches!(
            orchestration_engine::resolve_command(IRREGULAR, Some(&portal), &saved),
            Some(CommandMatch::Ephemeral { .. })
        ),
        "first run is dynamic"
    );
    // The user accepts the save card. The run's prompt becomes the key.
    let key = orchestration_engine::prompt_key(IRREGULAR).ok_or("prompt is keyable")?;
    let learned = playbook_store::Playbook::new(
        "aws-bills".into(),
        portal.clone(),
        vec![Step::Semantic {
            intent: macro_engine::SemanticIntent {
                role: "link".into(),
                label_query: "bill".into(),
                container_query: None,
                raw_prompt: IRREGULAR.into(),
                ordinal_index: None,
                is_last: false,
                is_plural: false,
                entry_url: Some("https://aws.amazon.com/billing/".into()),
                primary_target_noun: Some("bill".into()),
            },
        }],
    )?
    .with_prompt_key(Some(key.clone()));
    let id = store.save_playbook(&learned).await?;
    // Second run: the same phrasing is now a tier-1 hit — no parser, no
    // search, no tokens.
    let saved = store.list_playbooks().await?;
    assert_eq!(
        orchestration_engine::resolve_command(IRREGULAR, Some(&portal), &saved),
        Some(CommandMatch::Saved { id: id.clone() }),
        "learned phrasing replays from storage"
    );
    // Casing and spacing are normalized by the key, so the same command
    // typed a little differently still hits.
    assert_eq!(
        orchestration_engine::resolve_command(
            "  Pull up   what I owe ON aws ",
            Some(&portal),
            &saved
        ),
        Some(CommandMatch::Saved { id: id.clone() })
    );
    // The key round-trips through storage and stays replayable.
    let revived = store.load_playbook(&id).await?;
    assert_eq!(revived.prompt_key.as_deref(), Some(key.as_str()));
    assert_eq!(revived, learned);
    // Learning one phrasing never captures a neighbouring one. Same shape,
    // different destination: the key must not answer, so this prompt gets
    // its own resolution instead of silently replaying the AWS workflow.
    assert!(
        matches!(
            orchestration_engine::resolve_command(
                "pull up what I owe on azure",
                Some(&portal),
                &saved
            ),
            Some(CommandMatch::Ephemeral { .. })
        ),
        "an unlearned phrasing must not ride the learned key"
    );
    Ok(())
}

#[tokio::test]
async fn seeded_playbook_ships_with_a_pre_learned_prompt_key()
-> Result<(), Box<dyn std::error::Error>> {
    // The invoice route is keyed out of the box, so the prompt the deleted
    // route table used to answer is a tier-1 hit on a fresh install.
    let (_dir, store) = store().await?;
    let saved = store.list_playbooks().await?;
    let portal = url::Url::parse("https://github.com/")?;
    let Some(CommandMatch::Saved { id }) =
        orchestration_engine::resolve_command("Download GitHub invoices", Some(&portal), &saved)
    else {
        return Err("seeded key answers the prompt".into());
    };
    let playbook = store.load_playbook(&id).await?;
    assert_eq!(playbook.name, "github-invoices");
    assert_eq!(
        playbook.prompt_key.as_deref(),
        Some("download github invoices")
    );
    Ok(())
}
