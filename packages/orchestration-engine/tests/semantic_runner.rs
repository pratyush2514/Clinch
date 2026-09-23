#![deny(unsafe_code)]
//! Opt-in live-Chromium proof for Playbook step dispatch (legacy replay plus
//! semantic intents). Requires `CLINCH_CHROMIUM_PATH`; launches an isolated
//! headless profile and a loopback fixture portal. No personal profile or
//! real portal is touched.
use browser_driver::{Action, LaunchOptions, ManagedBrowser, WaitCondition};
use macro_engine::SemanticIntent;
use orchestration_engine::{
    SequencePhase, SequenceStatus, StepError, execute_step, run_playbook_sequence,
};
use playbook_store::Step;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use url::Url;

const HOME_HTML: &str = "<!doctype html><html><body><a id=\"archive\" href=\"/archive\">File archive</a><button data-testid=\"pay-btn\">Pay now</button><script>document.querySelector('button').addEventListener('click', function(){document.body.setAttribute('data-clicked','yes');});</script></body></html>";
const ARCHIVE_HTML: &str = "<!doctype html><html><body><a class=\"report\" href=\"/report.pdf\">Download report</a></body></html>";

async fn fixture_portal() -> Result<(Url, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>>
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = Url::parse(&format!("http://{}/", listener.local_addr()?))?;
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut head = [0; 4096];
                let _ = socket.read(&mut head).await;
                let body = if head.starts_with(b"GET /archive ") {
                    ARCHIVE_HTML
                } else {
                    HOME_HTML
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    Ok((url, task))
}

async fn headless_browser(
    dir: &tempfile::TempDir,
) -> Result<ManagedBrowser, Box<dyn std::error::Error>> {
    Ok(ManagedBrowser::launch_with_options(
        Path::new(&std::env::var("CLINCH_CHROMIUM_PATH")?),
        &dir.path().join("profile"),
        LaunchOptions::replay(),
    )
    .await?)
}

struct Harness {
    portal: Url,
    server: tokio::task::JoinHandle<()>,
    output: std::path::PathBuf,
    browser: ManagedBrowser,
    // Held for drop order only: the profile directory must outlive the browser.
    _profile_dir: tempfile::TempDir,
}

async fn harness() -> Result<Harness, Box<dyn std::error::Error>> {
    let (portal, server) = fixture_portal().await?;
    let profile_dir = tempfile::tempdir()?;
    let output = profile_dir.path().join("downloads");
    let browser = headless_browser(&profile_dir).await?;
    browser.navigate(&portal).await?;
    Ok(Harness {
        portal,
        server,
        output,
        browser,
        _profile_dir: profile_dir,
    })
}

async fn stop(harness: Harness) {
    let _ = harness.browser.shutdown().await;
    harness.server.abort();
}

fn legacy_click() -> Step {
    Step::LegacySelector {
        action: Action::Click {
            selector: "#archive".into(),
        },
        wait: Some(WaitCondition {
            selector: "a.report".into(),
            timeout_ms: 5_000,
        }),
    }
}

fn pay_intent() -> Step {
    Step::Semantic {
        intent: SemanticIntent {
            role: "button".into(),
            label_query: "Pay now".into(),
            container_query: None,
            raw_prompt: String::new(),
            ordinal_index: None,
            is_last: false,
            is_plural: false,
            entry_url: None,
            primary_target_noun: None,
        },
    }
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated headless fixture"]
async fn dispatch_replays_legacy_steps() -> Result<(), Box<dyn std::error::Error>> {
    // Recorded clicks replay through the saved selector and honor the
    // postcondition wait (proving navigation happened).
    let harness = harness().await?;
    let highlights: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let outcome = execute_step(
        &harness.browser,
        &harness.portal,
        &harness.output,
        0,
        &legacy_click(),
        |target| {
            highlights
                .lock()
                .map(|mut guard| guard.push(target.selector))
                .ok();
        },
        |_, _| async { true },
        |_, _| async { panic!("legacy path never asks intent consent") },
    )
    .await
    .map_err(|error| format!("legacy dispatch failed: {error:?}"))?;
    assert!(outcome.highlight.is_some());
    assert!(
        highlights
            .lock()
            .is_ok_and(|guard| guard.iter().any(|selector| selector == "#archive"))
    );
    stop(harness).await;
    Ok(())
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated headless fixture"]
async fn dispatch_executes_semantic_intents() -> Result<(), Box<dyn std::error::Error>> {
    let harness = harness().await?;
    let outcome = execute_step(
        &harness.browser,
        &harness.portal,
        &harness.output,
        1,
        &pay_intent(),
        |_| {},
        |_, _| async { panic!("semantic path never asks action consent") },
        |_, _| async { true },
    )
    .await
    .map_err(|error| format!("semantic dispatch failed: {error:?}"))?;
    assert!(
        outcome
            .highlight
            .is_some_and(|target| target.selector.starts_with("ax:"))
    );
    harness
        .browser
        .resolve("body[data-clicked='yes']", false)
        .await?;
    harness.browser.clear_marks().await?;
    stop(harness).await;
    Ok(())
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated headless fixture"]
async fn dispatch_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
    let harness = harness().await?;
    // Denied approvals fail closed on both paths without touching the page.
    let denied = execute_step(
        &harness.browser,
        &harness.portal,
        &harness.output,
        2,
        &pay_intent(),
        |_| {},
        |_, _| async { true },
        |_, _| async { false },
    )
    .await;
    assert!(matches!(denied, Err(StepError::ApprovalRequired)));
    let denied = execute_step(
        &harness.browser,
        &harness.portal,
        &harness.output,
        3,
        &legacy_click(),
        |_| {},
        |_, _| async { false },
        |_, _| async { true },
    )
    .await;
    assert!(matches!(denied, Err(StepError::ApprovalRequired)));

    // Unresolvable intents and broken selectors report precisely.
    let missing = Step::Semantic {
        intent: SemanticIntent {
            role: "button".into(),
            label_query: "No such control anywhere".into(),
            container_query: None,
            raw_prompt: String::new(),
            ordinal_index: None,
            is_last: false,
            is_plural: false,
            entry_url: None,
            primary_target_noun: None,
        },
    };
    let result = execute_step(
        &harness.browser,
        &harness.portal,
        &harness.output,
        4,
        &missing,
        |_| {},
        |_, _| async { true },
        |_, _| async { true },
    )
    .await;
    assert!(matches!(result, Err(StepError::NoMatch(_))));
    let broken = Step::LegacySelector {
        action: Action::Click {
            selector: "#no-longer-exists".into(),
        },
        wait: None,
    };
    let result = execute_step(
        &harness.browser,
        &harness.portal,
        &harness.output,
        7,
        &broken,
        |_| {},
        |_, _| async { true },
        |_, _| async { true },
    )
    .await;
    match result {
        Err(StepError::NeedsRepair(request)) => assert_eq!(request.step_index, 7),
        other => panic!("expected repair, got {other:?}"),
    }

    stop(harness).await;
    Ok(())
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated headless fixture"]
async fn sequence_runs_in_order_and_stops_at_repair() -> Result<(), Box<dyn std::error::Error>> {
    let harness = harness().await?;
    let phases: Arc<Mutex<Vec<(usize, SequencePhase)>>> = Arc::new(Mutex::new(Vec::new()));
    // Semantic click first (home page), then the legacy replay navigates on.
    let steps = vec![
        pay_intent(),
        legacy_click(),
        Step::LegacySelector {
            action: Action::Click {
                selector: "#no-longer-exists".into(),
            },
            wait: None,
        },
    ];
    let outcome = run_playbook_sequence(
        &harness.browser,
        &harness.portal,
        &harness.output,
        &steps,
        |event| {
            phases
                .lock()
                .map(|mut guard| guard.push((event.step_index, event.phase)))
                .ok();
        },
        |_, _| async { true },
        |_, _| async { true },
    )
    .await;
    assert_eq!(outcome.total_steps, 3);
    assert_eq!(outcome.completed_steps, 2);
    assert_eq!(outcome.status, SequenceStatus::NeedsRepair);
    assert_eq!(outcome.stopped_at, Some(2));
    // Progress streamed in order with a terminal block on the broken step.
    let phases = phases.lock().map_err(|_| "phase log poisoned")?.clone();
    assert!(phases.contains(&(0, SequencePhase::Started)));
    assert!(phases.contains(&(0, SequencePhase::Completed)));
    assert!(phases.contains(&(2, SequencePhase::Blocked)));
    assert!(!phases.iter().any(|(index, _)| *index > 2));
    harness.browser.clear_marks().await?;
    stop(harness).await;
    Ok(())
}

/// Three download links on one page, each in a row carrying invoice
/// evidence, plus a nav landmark whose link also mentions invoices. Page
/// chrome must never join a batch, so a run that clicks four controls — or
/// navigates away via the sidebar — fails this fixture.
const INVOICES_HTML: &str = "<!doctype html><html><body>\
<nav><a id=\"nav\" href=\"/archive\">All invoices</a></nav>\
<table>\
<tr><td>Invoice INV-001 <a class=\"dl\" href=\"#a\">Download</a></td></tr>\
<tr><td>Invoice INV-002 <a class=\"dl\" href=\"#b\">Download</a></td></tr>\
<tr><td>Invoice INV-003 <a class=\"dl\" href=\"#c\">Download</a></td></tr>\
</table>\
<script>window.clicked=0;document.querySelectorAll('a.dl').forEach(function(link){\
link.addEventListener('click',function(event){event.preventDefault();window.clicked++;\
document.body.setAttribute('data-clicked',String(window.clicked));});});</script>\
</body></html>";

async fn invoices_portal() -> Result<(Url, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>>
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = Url::parse(&format!("http://{}/", listener.local_addr()?))?;
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut head = [0; 4096];
                let _ = socket.read(&mut head).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{INVOICES_HTML}",
                    INVOICES_HTML.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    Ok((url, task))
}

fn plural_invoice_step() -> Step {
    Step::Semantic {
        intent: SemanticIntent {
            role: "link".into(),
            label_query: "download".into(),
            container_query: None,
            raw_prompt: "download all my invoices".into(),
            ordinal_index: None,
            is_last: false,
            is_plural: true,
            entry_url: None,
            primary_target_noun: Some("invoice".into()),
        },
    }
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated headless fixture"]
async fn saved_plural_step_batches_every_candidate_behind_one_gate()
-> Result<(), Box<dyn std::error::Error>> {
    // The live half of `test_saved_playbook_plural_batch_execution`: a saved
    // step with `is_plural` must click every eligible control through
    // `execute_batch`, and the gate must have seen those controls before the
    // first click. Ordering matters as much as the count — an approval that
    // cannot name what it is approving is not a gate.
    let (portal, server) = invoices_portal().await?;
    let profile_dir = tempfile::tempdir()?;
    let output = profile_dir.path().join("downloads");
    let browser = headless_browser(&profile_dir).await?;
    browser.navigate(&portal).await?;

    let previewed: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let clicks: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let seen = Arc::clone(&previewed);
    let counted = Arc::clone(&clicks);
    let outcome = execute_step(
        &browser,
        &portal,
        &output,
        0,
        &plural_invoice_step(),
        |_| {
            counted.lock().map(|mut guard| *guard += 1).ok();
        },
        |_, _| async { panic!("the semantic path never asks action consent") },
        |_, request: orchestration_engine::IntentApproval| {
            // Recorded inside the gate, so the assertions below prove the
            // candidates were known at consent time, not reconstructed after.
            seen.lock()
                .map(|mut guard| {
                    guard.push(
                        request
                            .candidates
                            .iter()
                            .map(|candidate| candidate.name.clone())
                            .collect(),
                    );
                })
                .ok();
            async move { request.is_batch() }
        },
    )
    .await
    .map_err(|error| format!("plural dispatch failed: {error:?}"))?;

    let previewed = previewed
        .lock()
        .map_err(|_| "preview log poisoned")?
        .clone();
    assert_eq!(previewed.len(), 1, "exactly one gate for the whole batch");
    assert_eq!(
        previewed[0],
        vec!["Download".to_owned(); 3],
        "the gate itemized all three row controls and excluded the nav link"
    );
    // Every click streamed its own highlight, so a batch of three is visible
    // as three targets rather than one opaque step.
    assert_eq!(*clicks.lock().map_err(|_| "click log poisoned")?, 3);
    assert!(outcome.highlight.is_some(), "the last target is reported");
    // The page itself is the witness: three handlers fired, not one.
    browser.resolve("body[data-clicked='3']", false).await?;
    browser.clear_marks().await?;
    let _ = browser.shutdown().await;
    server.abort();
    Ok(())
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated headless fixture"]
async fn rejected_plural_step_denies_the_sequence_without_clicking()
-> Result<(), Box<dyn std::error::Error>> {
    // Fail-closed, and closed means untouched: a denied batch leaves the
    // page exactly as it was.
    let (portal, server) = invoices_portal().await?;
    let profile_dir = tempfile::tempdir()?;
    let output = profile_dir.path().join("downloads");
    let browser = headless_browser(&profile_dir).await?;
    browser.navigate(&portal).await?;
    let steps = vec![plural_invoice_step()];
    let outcome = run_playbook_sequence(
        &browser,
        &portal,
        &output,
        &steps,
        |_| {},
        |_, _| async { panic!("the semantic path never asks action consent") },
        |_, _| async { false },
    )
    .await;
    assert_eq!(outcome.status, SequenceStatus::Denied);
    assert_eq!(outcome.completed_steps, 0);
    assert_eq!(outcome.stopped_at, Some(0));
    assert!(
        browser
            .resolve("body[data-clicked='1']", false)
            .await
            .is_err(),
        "no control was clicked"
    );
    let _ = browser.shutdown().await;
    server.abort();
    Ok(())
}
