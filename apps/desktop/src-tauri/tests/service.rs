//! Integration tests for `clinch_desktop::service`.
//!
//! Moved out of `src/service.rs` so the main source stays test-free.

mod common;

use browser_driver::WindowMode;
use clinch_desktop::service::*;
use common::ChromiumEnvGuard;
use orchestration_engine::{Task, TaskId, TaskRequest};
use std::{path::PathBuf, sync::Arc};
#[test]
fn remembered_href_validation_bars_bad_destinations() {
    // The settings memory fast path navigates without pursuit, so the
    // validation gate is the only thing standing between a remembered
    // row and the browser. It is pure — proven here, no browser.
    let portal = url::Url::parse("https://www.reddit.com/").unwrap();
    // Happy path: same-site settings URL; a `www.` portal folds to
    // the bare remembered host the same way the account-home path
    // already did.
    assert!(
        AppService::validate_remembered_href("https://www.reddit.com/settings/", &portal, None)
            .is_some()
    );
    let bare_portal = url::Url::parse("https://reddit.com/").unwrap();
    assert!(
        AppService::validate_remembered_href(
            "https://www.reddit.com/settings/",
            &bare_portal,
            None
        )
        .is_some()
    );
    // Scheme bar: http and non-navigable schemes are rejected.
    for href in [
        "http://www.reddit.com/settings/",
        "javascript:void(0)",
        "/settings",
        "not a url",
    ] {
        assert!(
            AppService::validate_remembered_href(href, &portal, None).is_none(),
            "rejected: {href}"
        );
    }
    // Credentials embedded in a remembered href are rejected, never
    // sent.
    for href in [
        "https://user@www.reddit.com/settings/",
        "https://user:secret@www.reddit.com/settings/",
    ] {
        assert!(
            AppService::validate_remembered_href(href, &portal, None).is_none(),
            "rejected: {href}"
        );
    }
    // Cross-site and root-path destinations are rejected: a
    // remembered settings URL is never a fresh portal open, and
    // never the site root.
    assert!(
        AppService::validate_remembered_href("https://www.microsoft.com/settings/", &portal, None)
            .is_none()
    );
    assert!(
        AppService::validate_remembered_href("https://www.reddit.com/", &portal, None).is_none()
    );
    // Identity rows still require the remembered username in the
    // path — the account-home fast path keeps its stricter check.
    assert!(
        AppService::validate_remembered_href(
            "https://www.reddit.com/user/someone/",
            &portal,
            Some("MangoTree-1233")
        )
        .is_none()
    );
    assert!(
        AppService::validate_remembered_href(
            "https://www.reddit.com/user/mangotree-1233/",
            &portal,
            Some("MangoTree-1233")
        )
        .is_some()
    );
}
#[test]
fn final_frame_failure_labels_are_static_and_sanitized() {
    use browser_driver::BrowserError;
    assert_eq!(
        AppService::final_frame_failure_label(&BrowserError::Timeout),
        "timeout"
    );
    assert_eq!(
        AppService::final_frame_failure_label(&BrowserError::Connection),
        "connection"
    );
    assert_eq!(
        AppService::final_frame_failure_label(&BrowserError::PageChanging),
        "page changing"
    );
    assert_eq!(
        AppService::final_frame_failure_label(&BrowserError::Launch),
        "launch"
    );
    assert_eq!(
        AppService::final_frame_failure_label(&BrowserError::InvalidAction),
        "invalid action"
    );
    // Payload-carrying variants never leak into the label.
    assert_eq!(
        AppService::final_frame_failure_label(&BrowserError::Navigation),
        "unexpected"
    );
}
#[test]
fn session_lend_registry_evicts_oldest_past_the_cap() -> Result<(), Box<dyn std::error::Error>> {
    let service = AppService::new(PathBuf::new(), PathBuf::new());
    for n in 0..(MAX_COMPLETED_RUNS + 5) {
        service.remember_session_lend(
            format!("run-{n}"),
            "https://example.com/".to_owned(),
            SessionLendOrigin::GuestLanding,
        );
    }
    let lends = service.session_lends.lock().map_err(|_| "session lends")?;
    assert_eq!(lends.len(), MAX_COMPLETED_RUNS);
    assert_eq!(lends.front().map(|(id, _)| id.as_str()), Some("run-5"));
    Ok(())
}
#[test]
fn session_lend_reregistration_refreshes_entry_and_resets_outcome()
-> Result<(), Box<dyn std::error::Error>> {
    // A run that settles lendable twice (e.g. a re-run) refreshes the
    // registry entry instead of duplicating it, and clears any
    // recorded outcome so the new tap runs fresh.
    let service = AppService::new(PathBuf::new(), PathBuf::new());
    service.remember_session_lend(
        "run-1".to_owned(),
        "https://a.example/".to_owned(),
        SessionLendOrigin::Challenge,
    );
    {
        let mut lends = service.session_lends.lock().map_err(|_| "session lends")?;
        let entry = lends
            .iter_mut()
            .find(|(id, _)| id == "run-1")
            .ok_or("run-1 must be registered")?;
        entry.1.outcome = Some(LendOutcome {
            cleared: true,
            cookies_lent: 3,
            reason: None,
            final_frame: None,
        });
    }
    service.remember_session_lend(
        "run-1".to_owned(),
        "https://b.example/".to_owned(),
        SessionLendOrigin::GuestLanding,
    );
    let lends = service.session_lends.lock().map_err(|_| "session lends")?;
    assert_eq!(lends.len(), 1);
    let (_, state) = lends.front().ok_or("run-1 must be registered")?;
    assert_eq!(state.page_url, "https://b.example/");
    assert_eq!(state.origin, SessionLendOrigin::GuestLanding);
    assert!(state.outcome.is_none());
    Ok(())
}
#[tokio::test]
async fn lend_session_rejects_unknown_run_before_consent() {
    // No registry entry, no bridge exchange: an unknown run id fails
    // before any consent journaling or bridge I/O.
    let service = AppService::new(PathBuf::new(), PathBuf::new());
    assert!(matches!(
        service
            .lend_session(LendRequest {
                lend_id: "ghost".to_owned(),
                source_connection_id: None,
            })
            .await,
        Err(AppError::InvalidInput(_))
    ));
}
#[tokio::test]
async fn lend_session_replays_recorded_outcome_without_a_second_bridge_request()
-> Result<(), Box<dyn std::error::Error>> {
    // A repeat tap replays the first attempt's recorded outcome. The
    // service has no database and no bridge server here, so any second
    // bridge request would fail — returning Ok proves the short-circuit.
    let service = AppService::new(PathBuf::new(), PathBuf::new());
    let outcome = LendOutcome {
        cleared: true,
        cookies_lent: 7,
        reason: None,
        final_frame: None,
    };
    service
        .session_lends
        .lock()
        .map_err(|_| "session lends")?
        .push_back((
            "run-1".to_owned(),
            SessionLendState {
                page_url: "https://www.reddit.com/login".to_owned(),
                origin: SessionLendOrigin::GuestLanding,
                outcome: Some(outcome),
            },
        ));
    let replayed = service
        .lend_session(LendRequest {
            lend_id: "run-1".to_owned(),
            source_connection_id: None,
        })
        .await
        .map_err(|_| "recorded lend outcome")?;
    assert!(replayed.cleared);
    assert_eq!(replayed.cookies_lent, 7);
    Ok(())
}
#[tokio::test]
async fn forget_site_session_rejects_a_non_host() {
    // The forget command takes a bare host; anything shaped like a URL
    // or carrying credentials is rejected before any browser I/O.
    let service = AppService::new(PathBuf::new(), PathBuf::new());
    for bad in [
        "https://example.com/",
        "example.com/path",
        "user@example.com",
        "example.com:8080",
        "",
    ] {
        assert!(
            matches!(
                service.forget_site_session(bad.to_owned()).await,
                Err(AppError::InvalidInput(_))
            ),
            "{bad} must be rejected"
        );
    }
}
#[tokio::test]
async fn task_run_requires_a_connected_session_and_exclusive_browser_access()
-> Result<(), Box<dyn std::error::Error>> {
    let service = AppService::new(PathBuf::new(), PathBuf::new());
    let request = TaskRequest {
        workflow: "reports".into(),
        portal_url: url::Url::parse("https://example.com/files")?,
        link_selector: None,
        download_selector: "a.report".into(),
    };
    assert!(matches!(
        service.run_task(&request, |_| {}).await,
        Err(AppError::SessionRequired)
    ));
    let _permit = service.operation.try_acquire()?;
    assert!(matches!(
        service.run_task(&request, |_| {}).await,
        Err(AppError::Busy)
    ));
    assert!(matches!(service.close_browser().await, Err(AppError::Busy)));
    Ok(())
}
#[tokio::test]
async fn file_actions_only_resolve_completed_task_downloads()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let root = dir.path().join(DOWNLOADS_DIR).join("1");
    tokio::fs::create_dir_all(&root).await?;
    let file = root.join("download.dat");
    let outside = dir.path().join("outside.dat");
    tokio::fs::write(&file, b"data").await?;
    tokio::fs::write(&outside, b"data").await?;
    let request = TaskRequest {
        workflow: "fixture".into(),
        portal_url: url::Url::parse("https://example.com")?,
        link_selector: None,
        download_selector: "a.report".into(),
    };
    let mut plan = request.plan()?;
    plan.steps[1]
        .output
        .files
        .push(browser_driver::DownloadedFile {
            path: file.to_string_lossy().into_owned(),
            bytes: 4,
        });
    let mut task = Task {
        id: TaskId(1),
        revision: 0,
        workflow: "fixture".into(),
        mode: orchestration_engine::RunMode::Record,
        state: orchestration_engine::TaskState::Completed,
        plan,
        repair: None,
        failure: None,
        elapsed_ms: 1,
    };
    let pool = service.database().await.map_err(|_| "database")?;
    for case in 0..4 {
        match case {
            1 => task.state = orchestration_engine::TaskState::Running,
            2 => {
                task.state = orchestration_engine::TaskState::Completed;
                task.plan.steps[1].output.files[0].path = outside.to_string_lossy().into_owned();
            }
            3 => {
                task.plan.steps[1].output.files[0].path =
                    root.join("missing.dat").to_string_lossy().into_owned();
            }
            _ => {}
        }
        sqlx::query("INSERT OR REPLACE INTO tasks(id, revision, snapshot) VALUES(1, 0, ?)")
            .bind(serde_json::to_string(&task)?)
            .execute(pool)
            .await?;
        let result = service.downloaded_file(TaskId(1), 0).await;
        assert_eq!(result.is_ok(), case == 0);
        assert!(service.downloaded_file(TaskId(1), 10).await.is_err());
    }
    pool.close().await;
    Ok(())
}

#[test]
fn dummy_approval_is_explicit_and_single_use() -> Result<(), AppError> {
    let service = AppService::new(PathBuf::new(), PathBuf::new());
    assert!(matches!(
        service.resolve_approval(1, true),
        Err(AppError::StaleApproval)
    ));
    let preview = service.preview_approval()?;
    assert!(matches!(service.preview_approval(), Err(AppError::Busy)));
    assert!(!service.resolve_approval(preview.id, false)?);
    assert!(matches!(
        service.resolve_approval(preview.id, true),
        Err(AppError::StaleApproval)
    ));
    let preview = service.preview_approval()?;
    assert!(service.resolve_approval(preview.id, true)?);
    Ok(())
}

#[tokio::test]
async fn embedded_auth_panel_fails_closed_without_pending_state() -> Result<(), AppError> {
    let service = AppService::new(PathBuf::new(), PathBuf::new());
    assert!(service.auth_status()?.is_none());
    assert!(matches!(
        service.complete_embedded_auth().await,
        Err(AppError::SessionRequired)
    ));
    assert!(matches!(
        service
            .begin_embedded_auth("http://insecure.example/")
            .await,
        Err(AppError::InvalidInput(_))
    ));
    assert!(service.auth_status()?.is_none());
    Ok(())
}

#[tokio::test]
async fn picker_commands_require_a_managed_browser() -> Result<(), AppError> {
    let service = AppService::new(PathBuf::new(), PathBuf::new());
    // No browser connected: status reports not-ready instead of arming an
    // overlay nobody can click.
    assert!(!service.picker_status()?.ready);
    assert!(matches!(
        service.picker_enable().await,
        Err(AppError::SessionRequired)
    ));
    assert!(matches!(
        service.picker_pick(1_000).await,
        Err(AppError::SessionRequired)
    ));
    assert!(matches!(
        service.picker_disable().await,
        Err(AppError::SessionRequired)
    ));
    Ok(())
}

#[test]
fn background_acquisition_never_requests_a_visible_window() {
    // The window-visibility contract, provable without Chromium.
    // 1. Background launches off-screen headed (a real headed browser,
    //    OS-hidden — never `--headless=new`, the most fingerprinted
    //    mode); interactive launches headed; escalation launches
    //    off-screen headed (hidden, never handed to the user).
    assert_eq!(
        BrowserIntent::Background.launch_options().mode,
        WindowMode::Offscreen
    );
    assert_eq!(
        BrowserIntent::Interactive.launch_options().mode,
        WindowMode::Headed
    );
    assert_eq!(
        BrowserIntent::ChallengeEscalation.launch_options().mode,
        WindowMode::Offscreen
    );
    // 2. Dormant: every intent launches, each under its own mode.
    for intent in [
        BrowserIntent::Background,
        BrowserIntent::Interactive,
        BrowserIntent::ChallengeEscalation,
    ] {
        assert_eq!(
            acquire_action(intent, None),
            AcquireAction::Launch,
            "{intent:?} must launch when dormant"
        );
    }
    // 3. Background NEVER restarts, in either direction: it cannot
    //    promote an off-screen context into a window, and it cannot demote
    //    a window the user opened with Take Control.
    for attached in [WindowMode::Headed, WindowMode::Offscreen] {
        assert_eq!(
            acquire_action(BrowserIntent::Background, Some(attached)),
            AcquireAction::Reuse,
            "background must reuse an attached session ({attached:?})"
        );
    }
    // 4. Interactive restarts only when the live session shows no
    //    visible window, and reuses an already-visible one.
    assert_eq!(
        acquire_action(BrowserIntent::Interactive, Some(WindowMode::Offscreen)),
        AcquireAction::Restart
    );
    assert_eq!(
        acquire_action(BrowserIntent::Interactive, Some(WindowMode::Headed)),
        AcquireAction::Reuse
    );
    // 5. Escalation reuses any attached session — headed-visible or
    //    off-screen — instead of relaunching it.
    assert_eq!(
        acquire_action(
            BrowserIntent::ChallengeEscalation,
            Some(WindowMode::Offscreen)
        ),
        AcquireAction::Reuse
    );
    assert_eq!(
        acquire_action(BrowserIntent::ChallengeEscalation, Some(WindowMode::Headed)),
        AcquireAction::Reuse
    );
}

#[tokio::test]
async fn background_dispatch_fails_closed_without_opening_a_window()
-> Result<(), Box<dyn std::error::Error>> {
    // Ad-hoc dispatch with no session and no usable Chromium: the run
    // must fail closed on launch rather than falling back to a headed
    // window. The guard below also proves the launch was attempted
    // (`BrowserUnavailable`, not `SessionRequired`) and that nothing
    // stayed attached afterwards.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let _chromium = ChromiumEnvGuard::hold_bogus();
    assert!(matches!(
        service.browser(BrowserIntent::Background).await,
        Err(AppError::BrowserUnavailable)
    ));
    assert!(!service.context_status().map_err(|_| "status")?.attached);
    Ok(())
}

#[tokio::test]
async fn browser_context_lifecycle_stays_dormant_until_acquired()
-> Result<(), Box<dyn std::error::Error>> {
    // Dormant by default: status reads and releases attach nothing —
    // no Chrome process exists until a task or acquire call needs one.
    let service = AppService::new(PathBuf::new(), PathBuf::new());
    assert!(!service.context_status().map_err(|_| "status")?.attached);
    service.release_context().await.map_err(|_| "release")?;
    assert!(!service.context_status().map_err(|_| "status")?.attached);
    // Failed acquisition leaves no lingering handle: the launch fails
    // fast, the status stays detached, and a second release is still a
    // clean no-op.
    let _chromium = ChromiumEnvGuard::hold_bogus();
    assert!(matches!(
        service.acquire_context(|_| {}, |_| {}).await,
        Err(AppError::BrowserUnavailable)
    ));
    assert!(!service.context_status().map_err(|_| "status")?.attached);
    service.release_context().await.map_err(|_| "release")?;
    Ok(())
}

#[test]
fn bridge_status_reports_stopped_before_first_use() -> Result<(), AppError> {
    let service = AppService::new(PathBuf::new(), PathBuf::new());
    // Reading status never starts the listener as a side effect.
    let status = service.bridge_status()?;
    let json = serde_json::to_string(&status).map_err(|_| AppError::Internal)?;
    assert!(json.contains("\"running\":false"));
    Ok(())
}

#[tokio::test]
async fn concurrent_bridge_startup_shares_one_listener() -> Result<(), AppError> {
    // The Tauri setup hook and a fast `bridge_sync` click race by design;
    // both callers must end up on the same listener, never `AddrInUse`.
    let service = AppService::new(PathBuf::new(), PathBuf::new());
    let (first, second) = tokio::join!(service.bridge_server_on(0), service.bridge_server_on(0));
    let (first, second) = (first?, second?);
    assert!(Arc::ptr_eq(&first, &second));
    // The setup-hook entry point reuses the same listener and reports it.
    let port = first.local_port().ok_or(AppError::Internal)?;
    assert_eq!(service.ensure_bridge().await?, port);
    Ok(())
}

#[tokio::test]
async fn intent_preview_requires_a_connected_session() -> Result<(), AppError> {
    let service = AppService::new(PathBuf::new(), PathBuf::new());
    // No portal connected and no browser: fails before any CDP traffic.
    assert!(matches!(
        service.preview_intent("button".into(), "Pay".into()).await,
        Err(AppError::SessionRequired)
    ));
    Ok(())
}

#[tokio::test]
async fn playbook_commands_validate_and_gate_without_a_browser()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    // Invalid portal and empty steps fail before touching storage.
    assert!(matches!(
        service
            .save_playbook("x".into(), "http://insecure.example/".into(), Vec::new())
            .await,
        Err(AppError::InvalidInput(_))
    ));
    assert!(
        service
            .save_playbook(
                "run".into(),
                "https://example.com/".into(),
                vec![playbook_store::Step::Semantic {
                    intent: macro_engine::SemanticIntent {
                        role: "button".into(),
                        label_query: "Pay".into(),
                        container_query: None,
                        raw_prompt: String::new(),
                        ordinal_index: None,
                        is_last: false,
                        is_plural: false,
                        entry_url: None,
                        primary_target_noun: None,
                    },
                }],
            )
            .await
            .is_ok()
    );
    let listed = service.list_playbooks().await.map_err(|_| "list")?;
    // Seeded defaults share the list with user saves, so match by name.
    let saved_row = listed
        .iter()
        .find(|summary| summary.name == "run")
        .ok_or("saved row listed")?;
    assert_eq!(saved_row.step_count, 1);
    // No portal connected: execution is rejected before any browser I/O.
    assert!(matches!(
        service.execute_playbook(saved_row.id.clone(), |_| {}).await,
        Err(AppError::SessionRequired)
    ));
    // No pending gate: decisions fail closed without side effects.
    assert!(matches!(
        service.decide_playbook(1, 0, true),
        Err(AppError::StaleApproval)
    ));
    assert!(matches!(
        service.execute_playbook("999".into(), |_| {}).await,
        Err(AppError::InvalidInput(_))
    ));
    Ok(())
}

#[tokio::test]
async fn service_routes_plural_prompt_to_batch_execution() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let portal = url::Url::parse("https://github.com/")?;
    // A saved 1-step legacy workflow whose name token-matches the prompt:
    // without the plural bypass this is exactly what would replay.
    service
        .save_playbook(
            "download_invoice_link".into(),
            "https://github.com/".into(),
            vec![playbook_store::Step::LegacySelector {
                action: browser_driver::Action::Click {
                    selector: "#dl".into(),
                },
                wait: None,
            }],
        )
        .await
        .map_err(|_| "save")?;
    *service.session_origin.lock().map_err(|_| "session")? = Some(portal.clone());
    let saved = service
        .playbooks()
        .await
        .map_err(|_| "store")?
        .list_playbooks()
        .await
        .map_err(|_| "list")?;
    // The problem-statement prompt bypasses the saved replay entirely.
    let matched = orchestration_engine::resolve_command(
        "download all my invoices from github",
        Some(&portal),
        &saved,
    );
    let Some(ref routed) = matched else {
        panic!("plural prompt resolves");
    };
    assert_eq!(dispatch_lane(routed), DispatchLane::Batch);
    let orchestration_engine::CommandMatch::Ephemeral { intent } = routed else {
        panic!("plural prompt never takes the saved lane")
    };
    assert!(intent.is_plural);
    // And the routed intent batches every candidate instead of
    // truncating to the first click — no browser needed for pure
    // resolution.
    let buttons = ["Download invoice", "Download invoice", "Download invoice"];
    let elements: Vec<browser_driver::AxElement> = buttons
        .iter()
        .enumerate()
        .map(|(index, name)| browser_driver::AxElement {
            backend_node_id: i64::try_from(index + 1).unwrap_or(1),
            role: "link".into(),
            name: (*name).into(),
            description: String::new(),
            container_text: Vec::new(),
            landmark: None,
        })
        .collect();
    let macro_engine::ResolveOutcome::BatchMatch(batch) =
        macro_engine::resolve_batch(&elements, intent)
    else {
        panic!("plural intent batches all matches");
    };
    assert_eq!(batch.len(), 3);
    // Control: the singular twin still takes the saved lane untouched.
    let control = orchestration_engine::resolve_command(
        "download invoice from github",
        Some(&portal),
        &saved,
    );
    let Some(ref control) = control else {
        panic!("singular prompt resolves");
    };
    assert_eq!(dispatch_lane(control), DispatchLane::Saved);
    Ok(())
}

#[test]
fn candidate_previews_map_labels_roles_landmarks_and_containers() {
    // Pure mapping: indices in document order, verbatim labels and
    // roles, landmark flags straight from the snapshot, container
    // fingerprints joined from surroundings (`None` when bare).
    let candidates = vec![
        browser_driver::AxElement {
            backend_node_id: 1,
            role: "link".into(),
            name: "Download PDF - June 2026".into(),
            description: String::new(),
            container_text: vec!["Invoices".into(), "INV-001".into()],
            landmark: None,
        },
        browser_driver::AxElement {
            backend_node_id: 2,
            role: "link".into(),
            name: "Downloads".into(),
            description: String::new(),
            container_text: vec!["Primary".into()],
            landmark: Some("navigation".into()),
        },
        browser_driver::AxElement {
            backend_node_id: 3,
            role: "button".into(),
            name: String::new(),
            description: String::new(),
            container_text: Vec::new(),
            landmark: None,
        },
    ];
    assert_eq!(
        candidate_previews(&candidates),
        vec![
            CandidatePreview {
                index: 0,
                label: "Download PDF - June 2026".into(),
                role: "link".into(),
                is_landmark: false,
                container: Some("Invoices INV-001".into()),
            },
            CandidatePreview {
                index: 1,
                label: "Downloads".into(),
                role: "link".into(),
                is_landmark: true,
                container: Some("Primary".into()),
            },
            CandidatePreview {
                index: 2,
                label: String::new(),
                role: "button".into(),
                is_landmark: false,
                container: None,
            },
        ]
    );
    assert!(candidate_previews(&[]).is_empty());
}

#[tokio::test]
async fn batch_approval_carries_candidate_previews_to_the_gate()
-> Result<(), Box<dyn std::error::Error>> {
    // The approval card payload — not just the count: decide through the
    // real gate and inspect the emitted event. No browser involved.
    let service = AppService::new(PathBuf::new(), PathBuf::new());
    let candidates = vec![browser_driver::AxElement {
        backend_node_id: 7,
        role: "link".into(),
        name: "Download".into(),
        description: String::new(),
        container_text: vec!["INV-007".into()],
        landmark: None,
    }];
    let intent = macro_engine::SemanticIntent {
        role: "link".into(),
        label_query: "download".into(),
        container_query: None,
        raw_prompt: String::new(),
        ordinal_index: None,
        is_last: false,
        is_plural: true,
        entry_url: None,
        primary_target_noun: None,
    };
    let mut captured: Vec<PlaybookEvent> = Vec::new();
    let mut push = |event: PlaybookEvent| captured.push(event);
    let events = std::sync::Mutex::new(&mut push);
    let (approved, ()) = tokio::join!(
        service.approve_batch(42, &candidates, &intent, "download all", &events),
        async {
            tokio::task::yield_now().await;
            let _ = service.decide_playbook(42, 0, true);
        }
    );
    assert!(approved);
    assert_eq!(captured.len(), 1);
    let approval = captured[0].approval.as_ref().ok_or("gate card emitted")?;
    assert_eq!(
        approval.summary,
        "Batch click: 1 link controls for download all"
    );
    assert_eq!(
        approval.candidates,
        vec![CandidatePreview {
            index: 0,
            label: "Download".into(),
            role: "link".into(),
            is_landmark: false,
            container: Some("INV-007".into()),
        }]
    );
    Ok(())
}

#[tokio::test]
async fn test_saved_playbook_plural_batch_execution() -> Result<(), Box<dyn std::error::Error>> {
    // The learning loop for a plural command, end to end and browserless:
    // an ad-hoc plural prompt resolves to an is_plural intent, that intent
    // is saved as a playbook, it reloads plural out of SQLite, and
    // replaying it raises the *batch* gate — itemizing every resolved
    // control before any CDP click — rather than silently clicking one.
    //
    // Candidates come from the shared hermetic billing-history fixture
    // through `resolve_batch`, the same collector `execute_batch` uses, and
    // the gate is the real one `decide_playbook` answers. What a live
    // Chromium adds on top is the clicking itself, covered by the
    // opt-in runner test in orchestration-engine.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let portal = url::Url::parse("https://github.com/")?;
    let prompt = "download all my invoices from github";
    let matched = orchestration_engine::resolve_command(prompt, Some(&portal), &[]);
    let Some(orchestration_engine::CommandMatch::Ephemeral { mut intent }) = matched else {
        panic!("plural prompt resolves to an ephemeral intent");
    };
    assert!(intent.is_plural, "the prompt is plural");
    // The entry a completed ad-hoc run would have proven and carried into
    // storage; the gate names it so a wrong route is visible pre-consent.
    intent.entry_url = Some("https://github.com/account/billing/history".into());

    let id = service
        .save_playbook(
            "github_invoices_all".into(),
            portal.to_string(),
            vec![playbook_store::Step::Semantic {
                intent: intent.clone(),
            }],
        )
        .await
        .map_err(|_| "save")?;
    let reloaded = service
        .playbooks()
        .await
        .map_err(|_| "store")?
        .load_playbook(&id)
        .await?;
    let [playbook_store::Step::Semantic { intent: saved }] = reloaded.steps.as_slice() else {
        panic!("the saved playbook holds exactly one semantic step");
    };
    assert!(
        saved.is_plural,
        "the plural flag survives the steps_json round-trip"
    );

    // Exactly what the runner hands the gate: the fixture's three invoice
    // rows, header excluded and page chrome never admitted.
    let candidates = billing_history_candidates(saved)?;
    assert_eq!(candidates.len(), 3, "every invoice row joins the batch");

    // Both decisions, through the real single-flight gate: approval is
    // granted only when given, and a rejection fails closed.
    let request = orchestration_engine::IntentApproval::batch(saved.clone(), candidates);
    assert!(request.is_batch());
    let expected_summary = format!(
        "Batch click: 3 {} controls for {prompt} @ github.com/account/billing/history",
        saved.role
    );
    for (run_id, decision) in [(70_u64, true), (71, false)] {
        let (granted, gate) = drive_one_gate(&service, run_id, &request, decision).await?;
        assert_eq!(granted, decision, "the gate returns the decision given");
        assert_eq!(gate.kind, "intent");
        // The count and the destination are both visible pre-consent, and
        // only the host and path of the entry route reach the summary.
        assert_eq!(gate.summary, expected_summary);
        assert_batch_previews(&gate.candidates);
    }

    // The contrast that keeps the two lanes legible: a single-target step
    // raises a candidate-free gate reading as it always has.
    let mut single = saved.clone();
    single.is_plural = false;
    let single =
        AppService::intent_approval_content(&orchestration_engine::IntentApproval::single(single));
    assert_eq!(
        single.summary,
        format!("{} · {}", saved.role, saved.label_query)
    );
    assert!(single.candidates.is_empty());
    Ok(())
}

/// Batch candidates the engine itself would collect from the shared
/// hermetic billing-history tree — no browser, no hand-built elements, so
/// the fixture and production agree on what a row candidate is.
fn billing_history_candidates(
    intent: &macro_engine::SemanticIntent,
) -> Result<Vec<browser_driver::AxElement>, Box<dyn std::error::Error>> {
    let nodes: Vec<browser_driver::AxNode> = serde_json::from_value(
        browser_driver::test_utils::fake_cdp::billing_history_tree()
            .get("nodes")
            .cloned()
            .unwrap_or_default(),
    )?;
    let elements = browser_driver::interactive_elements(&nodes);
    match macro_engine::resolve_batch(&elements, intent) {
        macro_engine::ResolveOutcome::BatchMatch(batch) => Ok(batch),
        other => Err(format!("the billing rows resolve as a batch, got {other:?}").into()),
    }
}

/// Raise one real gate and answer it, returning the decision the run saw
/// plus the card the UI was shown.
async fn drive_one_gate(
    service: &AppService,
    run_id: u64,
    request: &orchestration_engine::IntentApproval,
    decision: bool,
) -> Result<(bool, PlaybookApproval), Box<dyn std::error::Error>> {
    let mut captured: Vec<PlaybookEvent> = Vec::new();
    let mut push = |event: PlaybookEvent| captured.push(event);
    let events = std::sync::Mutex::new(&mut push);
    let (granted, ()) = tokio::join!(
        service.approve_playbook_step(
            run_id,
            0,
            1,
            AppService::intent_approval_content(request),
            &events,
        ),
        async {
            tokio::task::yield_now().await;
            let _ = service.decide_playbook(run_id, 0, decision);
        }
    );
    let gate = captured
        .first()
        .and_then(|event| event.approval.clone())
        .ok_or("one gate card is emitted")?;
    Ok((granted, gate))
}

/// Every preview carries what makes three identical "Download" labels
/// distinguishable: document position, role, chrome status, and the row
/// evidence naming its invoice.
fn assert_batch_previews(candidates: &[CandidatePreview]) {
    assert_eq!(candidates.len(), 3);
    assert!(
        candidates
            .iter()
            .enumerate()
            .all(|(index, candidate)| candidate.index == index
                && candidate.label == "Download"
                && candidate.role == "link"
                && !candidate.is_landmark),
        "previews carry position, label, role, and chrome status: {candidates:?}"
    );
    assert!(
        candidates.iter().all(|candidate| candidate
            .container
            .as_deref()
            .is_some_and(|text| text.contains("INV-"))),
        "each preview names the invoice its row belongs to: {candidates:?}"
    );
}

#[tokio::test]
async fn propose_entry_misses_honestly_with_no_static_route_table()
-> Result<(), Box<dyn std::error::Error>> {
    // No browser: the tiered proposer reads the shortcut store (empty
    // here) and otherwise resolves purely, journaling through `record`,
    // which fails open without observers.
    //
    // With the curated `(portal, class)` table deleted and the search
    // tier removed, a portal-shaped prompt the ladder cannot ground is
    // an honest miss: no deep link is invented, no search page is
    // proposed, and the journal names the miss — not a follow-up click.
    // Proven destinations come from saved playbooks (tier 1) instead,
    // resolved before dispatch.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let mut intent = macro_engine::SemanticIntent {
        role: "link".into(),
        label_query: "invoices".into(),
        container_query: None,
        raw_prompt: "download all my invoices from github".into(),
        ordinal_index: None,
        is_last: false,
        is_plural: true,
        entry_url: None,
        primary_target_noun: Some("invoice".into()),
    };
    let proposed = service
        .propose_entry_url("download all my invoices from github", &mut intent, None)
        .await;
    // The intent leaves no entry URL: there is no invented destination
    // and no search template to follow.
    assert_eq!(intent.entry_url, None, "no destination invented");
    assert!(proposed.source.is_none(), "no tier answered");
    assert!(
        proposed
            .log
            .as_deref()
            .is_some_and(|line| line.contains("route_resolution_miss")),
        "miss journaled, got {:?}",
        proposed.log
    );
    // No deep link is ever fabricated for the portal named in the prompt.
    assert!(
        !intent
            .entry_url
            .as_deref()
            .unwrap_or("")
            .contains("github.com/account")
    );
    // Unknown prompts behave identically — there is no privileged portal
    // vocabulary left to branch on.
    let mut other = intent.clone();
    other.label_query = "dashboard".into();
    other.primary_target_noun = None;
    other.entry_url = None;
    let proposed = service
        .propose_entry_url("find the dashboard", &mut other, None)
        .await;
    // A missing destination is a miss, never a search template.
    assert_eq!(other.entry_url, None, "no destination invented");
    assert!(proposed.source.is_none(), "no tier answered");
    assert!(
        proposed
            .log
            .as_deref()
            .is_some_and(|line| line.contains("route_resolution_miss")),
        "miss journaled, got {:?}",
        proposed.log
    );
    // An entry already present (a saved playbook's own route) proposes
    // nothing and is never overwritten.
    let mut preset = intent.clone();
    preset.entry_url = Some("https://github.com/account/billing/history".into());
    let proposed = service
        .propose_entry_url("download all my invoices from github", &mut preset, None)
        .await;
    assert_eq!(
        preset.entry_url.as_deref(),
        Some("https://github.com/account/billing/history")
    );
    assert!(proposed.log.is_none());
    Ok(())
}

#[tokio::test]
async fn direct_open_miss_is_guidance_not_search() -> Result<(), Box<dyn std::error::Error>> {
    // Service-level proof the ladder fails closed: "open amazon for me"
    // is a direct open with no saved shortcut, no typed domain, and no
    // directory key in the test env — so the proposal is a miss, never
    // a scraped SERP, and the dispatcher turns it into guidance.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let prompt = "open amazon for me";
    // Unconnected ad-hoc resolves against the same synthetic search
    // origin the production lane uses.
    let origin = url::Url::parse("https://www.google.com/").ok();
    let Some(orchestration_engine::CommandMatch::Ephemeral { mut intent }) =
        orchestration_engine::resolve_command(prompt, origin.as_ref(), &[])
    else {
        panic!("ad-hoc prompt resolves ephemeral");
    };
    let proposed = service.propose_entry_url(prompt, &mut intent, None).await;
    assert!(proposed.direct_open_miss, "ladder miss flagged");
    assert_eq!(intent.entry_url, None, "no destination invented");
    assert!(proposed.source.is_none(), "no tier answered");
    assert!(
        proposed
            .log
            .as_deref()
            .is_some_and(|line| line.contains("route_resolution_miss")),
        "miss journaled, got {:?}",
        proposed.log
    );
    // The dispatcher's next step is the guidance error, not a run.
    let err = AppService::direct_open_miss_error(&proposed).ok_or("guidance error")?;
    let message = format!("{err:?}");
    assert!(
        message.contains("amazon.in") && message.contains("shortcut"),
        "guidance names the way out: {message}"
    );
    Ok(())
}

#[tokio::test]
async fn direct_open_miss_names_why_the_ladder_missed() -> Result<(), Box<dyn std::error::Error>> {
    // The miss journal line must say which rungs were even live: an
    // unconfigured grounder is a setup problem, a configured one that
    // declined is a genuine miss. Same harness as the guidance test —
    // the assertions stay consistent with the flag rather than assuming
    // the ambient environment.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let prompt = "open amazon for me";
    let origin = url::Url::parse("https://www.google.com/").ok();
    let Some(orchestration_engine::CommandMatch::Ephemeral { mut intent }) =
        orchestration_engine::resolve_command(prompt, origin.as_ref(), &[])
    else {
        panic!("ad-hoc prompt resolves ephemeral");
    };
    let proposed = service.propose_entry_url(prompt, &mut intent, None).await;
    assert!(proposed.direct_open_miss, "ladder miss flagged");
    let line = proposed.log.clone().unwrap_or_default();
    if proposed.grounder_configured {
        assert!(
            line.contains("grounder attempted") || line.contains("grounder error:"),
            "configured grounder names the outcome, got: {line}"
        );
    } else {
        assert!(
            line.contains("grounder unconfigured"),
            "unconfigured grounder named as the cause: {line}"
        );
        assert!(
            line.contains("CLINCH_GROUNDER_PROVIDER"),
            "miss names the setup fix: {line}"
        );
    }
    Ok(())
}

#[test]
fn direct_open_miss_error_distinguishes_setup_from_miss() {
    // Pure unit coverage for the guidance split: the unconfigured case
    // points at setup, the configured case keeps the generic guidance,
    // and a non-miss proposes no error at all.
    let unconfigured = ProposedEntry {
        direct_open_miss: true,
        grounder_configured: false,
        ..ProposedEntry::default()
    };
    let message = match AppService::direct_open_miss_error(&unconfigured) {
        Some(AppError::InvalidInput(message)) => message,
        other => panic!("expected InvalidInput, got {other:?}"),
    };
    assert!(
        message.contains("isn't configured") && message.contains("CLINCH_GROUNDER_PROVIDER"),
        "setup hint: {message}"
    );
    let declined = ProposedEntry {
        direct_open_miss: true,
        grounder_configured: true,
        ..ProposedEntry::default()
    };
    let message = match AppService::direct_open_miss_error(&declined) {
        Some(AppError::InvalidInput(message)) => message,
        other => panic!("expected InvalidInput, got {other:?}"),
    };
    assert!(
        !message.contains("isn't configured"),
        "genuine miss keeps generic guidance: {message}"
    );
    assert!(
        AppService::direct_open_miss_error(&ProposedEntry::default()).is_none(),
        "non-miss proposes no error"
    );
}

#[test]
fn live_page_fallback_noun_targets_the_ladder_miss() {
    // Pure unit coverage for the live-page fallback gate: only a
    // high-confidence direct-open whose target the ladder could NOT
    // ground (a full miss) is tried in-page on the live origin.
    // Grounded prompts keep today's behavior; low-confidence prompts
    // and plurals never take this path.
    let miss = ProposedEntry {
        direct_open_miss: true,
        ..ProposedEntry::default()
    };
    assert_eq!(
        AppService::live_page_fallback_noun("open settings for me", false, &miss),
        Some("setting".to_string()),
        "full ladder miss tries the noun in-page"
    );
    let grounded = ProposedEntry {
        source: Some(orchestration_engine::RouteSource::Shortcut),
        ..ProposedEntry::default()
    };
    assert!(
        AppService::live_page_fallback_noun("open claude for me", false, &grounded).is_none(),
        "grounded direct open keeps the routing-ladder behavior"
    );
    assert!(
        AppService::live_page_fallback_noun("open settings for me", false, &grounded).is_none(),
        "grounded site-name prompt never falls back to the live page"
    );
    assert!(
        AppService::live_page_fallback_noun("open settings for me", true, &miss).is_none(),
        "plural prompts never take the fallback"
    );
    assert!(
        AppService::live_page_fallback_noun("what are the settings for me", false, &miss).is_none(),
        "low-confidence prompt never takes the fallback"
    );
}

#[tokio::test]
async fn ephemeral_dispatch_attaches_proposed_route_to_task_step()
-> Result<(), Box<dyn std::error::Error>> {
    // Hermetic replay of the command-bar Run path for an ad-hoc prompt
    // starting with no entry URL: resolve the ephemeral intent, propose
    // the route at the top (before steps exist), then build step 1
    // exactly like the dispatch lanes do.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let portal = url::Url::parse("https://github.com/")?;
    *service.session_origin.lock().map_err(|_| "session")? = Some(portal.clone());
    let prompt = "download all my invoices from github";
    let saved = service
        .playbooks()
        .await
        .map_err(|_| "store")?
        .list_playbooks()
        .await
        .map_err(|_| "list")?;
    let Some(orchestration_engine::CommandMatch::Ephemeral { mut intent }) =
        orchestration_engine::resolve_command(prompt, Some(&portal), &saved)
    else {
        panic!("ad-hoc prompt resolves ephemeral");
    };
    assert_eq!(intent.entry_url, None);
    service
        .propose_entry_url(prompt, &mut intent, Some(&portal))
        .await;
    // Unresolved proposals attach nothing: with no search tier the
    // intent carries no entry URL, and step 1 stays destination-free
    // instead of inheriting an invented search page.
    assert_eq!(intent.entry_url, None, "no destination invented");
    let steps = [playbook_store::Step::Semantic {
        intent: intent.clone(),
    }];
    let playbook_store::Step::Semantic {
        intent: step_intent,
    } = &steps[0]
    else {
        panic!("step 1 is semantic");
    };
    assert_eq!(step_intent.entry_url, None, "step 1 carries no route");
    // Session Activity carries the honest miss, never a search line.
    let pool = service.database().await.map_err(|_| "database")?;
    let rows: Vec<(String,)> = sqlx::query_as("SELECT outcome FROM session_events")
        .fetch_all(pool)
        .await
        .map_err(|_| "events")?;
    assert!(
        rows.iter()
            .any(|(outcome,)| outcome.contains("route_resolution_miss")),
        "miss journaled, got {rows:?}"
    );
    assert!(
        !rows
            .iter()
            .any(|(outcome,)| outcome.starts_with("route_fallback:")),
        "no search fallback journaled, got {rows:?}"
    );
    Ok(())
}

/// Shared front half for the persist/replay test: a service with a
/// connected portal, plus the invoice fixture saved through the IPC
/// command under test. Returns the playbook id and its saved intent.
async fn persist_invoice_fixture()
-> Result<(AppService, String, macro_engine::SemanticIntent), Box<dyn std::error::Error>> {
    // Real dispatch path, minus the browser: resolve the ephemeral
    // intent and propose its entry route purely.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let portal = url::Url::parse("https://github.com/")?;
    service
        .test_connect(portal.clone())
        .map_err(|_| "connect")?;
    let prompt = "download all my invoices from github";
    let saved = service
        .playbooks()
        .await
        .map_err(|_| "store")?
        .list_playbooks()
        .await
        .map_err(|_| "list")?;
    let Some(orchestration_engine::CommandMatch::Ephemeral { mut intent }) =
        orchestration_engine::resolve_command(prompt, Some(&portal), &saved)
    else {
        panic!("ad-hoc prompt resolves ephemeral");
    };
    service
        .propose_entry_url(prompt, &mut intent, Some(&portal))
        .await;
    // With no static route table and no search tier, the ladder misses
    // and the saved intent carries no destination — an ungrounded prompt
    // persists honestly instead of persisting a search template. Grammar
    // slots survive persistence: the artifact noun anchors the batch,
    // the `github` complement was the destination cue and is not the
    // batch anchor.
    assert_eq!(intent.entry_url, None, "no destination invented");
    assert!(intent.is_plural);
    assert_eq!(intent.primary_target_noun.as_deref(), Some("invoice"));
    // Terminal completion records the exact executed graph, as the batch
    // lane does on `Completed`.
    let steps = vec![playbook_store::Step::Semantic {
        intent: intent.clone(),
    }];
    service.remember_completed_run("run-1-test", &portal, &steps, prompt);
    // Persist through the IPC command under test, with a memo. Unknown
    // ids fail closed without touching storage.
    let playbook_id = service
        .save_run_as_workflow(
            "run-1-test".into(),
            "github-download-invoices".into(),
            Some("Monthly run".into()),
        )
        .await
        .map_err(|_| "save")?;
    assert!(matches!(
        service
            .save_run_as_workflow("missing".into(), "x".into(), None)
            .await,
        Err(AppError::InvalidInput(_))
    ));
    let playbook = service
        .playbooks()
        .await
        .map_err(|_| "store")?
        .load_playbook(&playbook_id)
        .await
        .map_err(|_| "load")?;
    let [playbook_store::Step::Semantic { intent: saved }] = playbook.steps.as_slice() else {
        panic!("single semantic step");
    };
    Ok((service, playbook_id, saved.clone()))
}

#[tokio::test]
async fn test_persist_ephemeral_run_to_playbook_and_replay()
-> Result<(), Box<dyn std::error::Error>> {
    use browser_driver::test_utils::fake_cdp::{FakeCdpClient, FakeCdpServer, ScriptStep};
    use std::time::Duration;
    let (service, playbook_id, saved) = persist_invoice_fixture().await?;
    // SQLite persistence: origin, entry route, noun, and memo.
    let portal = url::Url::parse("https://github.com/")?;
    let playbook = service
        .playbooks()
        .await
        .map_err(|_| "store")?
        .load_playbook(&playbook_id)
        .await
        .map_err(|_| "load")?;
    assert_eq!(playbook.origin, portal);
    assert_eq!(saved.entry_url, None, "no destination invented");
    assert!(saved.is_plural);
    assert_eq!(saved.primary_target_noun.as_deref(), Some("invoice"));
    let listed = service
        .playbooks()
        .await
        .map_err(|_| "store")?
        .list_playbooks()
        .await
        .map_err(|_| "list")?;
    let summary = listed
        .iter()
        .find(|summary| summary.id == playbook_id)
        .ok_or("listed")?;
    assert_eq!(summary.description.as_deref(), Some("Monthly run"));
    // Replay end-to-end against scripted billing traffic: the saved
    // intent deterministically batches all three invoice rows — no
    // prompt router, no model, pure scoring over the fake's tree.
    let fake = FakeCdpServer::start(vec![ScriptStep::reply(
        "Accessibility.getFullAXTree",
        browser_driver::test_utils::fake_cdp::billing_history_tree(),
    )])
    .await
    .map_err(|error| format!("fake server failed to start: {error}"))?;
    let run = async {
        let mut client = FakeCdpClient::connect(fake.url()).await?;
        let tree = client
            .call("Accessibility.getFullAXTree", serde_json::json!({}))
            .await?;
        let nodes: Vec<browser_driver::AxNode> =
            serde_json::from_value(tree.get("nodes").cloned().unwrap_or_default())
                .map_err(|error| format!("bad tree: {error}"))?;
        let elements = browser_driver::interactive_elements(&nodes);
        let macro_engine::ResolveOutcome::BatchMatch(batch) =
            macro_engine::resolve_batch(&elements, &saved)
        else {
            panic!("saved intent replays every invoice row");
        };
        assert_eq!(
            batch
                .iter()
                .map(|element| element.backend_node_id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(fake.received_methods(), vec!["Accessibility.getFullAXTree"]);
        assert!(fake.violations().is_empty());
        client.close().await;
        Ok::<_, Box<dyn std::error::Error>>(())
    };
    let outcome = tokio::time::timeout(Duration::from_secs(10), run).await;
    fake.shutdown();
    outcome.map_err(|_| "fake CDP roundtrip timed out")??;
    Ok(())
}

#[tokio::test]
async fn test_consent_gated_playbook_saving() -> Result<(), Box<dyn std::error::Error>> {
    // The learning loop only closes on an explicit click. A completed run
    // is *offered* for saving; declining it — which in the UI means simply
    // not pressing the button — must leave the store untouched, or every
    // throwaway prompt would accumulate as a workflow.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let portal = url::Url::parse("https://aws.amazon.com/")?;
    let prompt = "pull up what I owe on aws";
    let steps = vec![playbook_store::Step::Semantic {
        intent: macro_engine::SemanticIntent {
            role: "link".into(),
            label_query: "bill".into(),
            container_query: None,
            raw_prompt: prompt.into(),
            ordinal_index: None,
            is_last: false,
            is_plural: false,
            entry_url: Some("https://aws.amazon.com/billing/".into()),
            primary_target_noun: Some("bill".into()),
        },
    }];
    let baseline = service.list_playbooks().await.map_err(|_| "list")?.len();
    // Run completes and is remembered in session memory.
    service.remember_completed_run("run-consent", &portal, &steps, prompt);
    // Declined: the card was shown and not accepted. Nothing persists.
    assert_eq!(
        service.list_playbooks().await.map_err(|_| "list")?.len(),
        baseline,
        "remembering a run must not persist it"
    );
    // Accepted: the explicit save command is the consent.
    let id = service
        .save_run_as_workflow(
            "run-consent".into(),
            "aws-bills".into(),
            Some("Monthly AWS".into()),
        )
        .await
        .map_err(|_| "save")?;
    let listed = service.list_playbooks().await.map_err(|_| "list")?;
    assert_eq!(listed.len(), baseline + 1);
    let row = listed
        .iter()
        .find(|summary| summary.id == id)
        .ok_or("saved row listed")?;
    // Saved exactly what replays today — origin, entry route, slots — plus
    // the prompt key that closes the loop. No imagined macro recording.
    assert_eq!(
        row.prompt_key.as_deref(),
        Some("pull up what i owe on aws"),
        "the prompt becomes the key"
    );
    assert_eq!(row.description.as_deref(), Some("Monthly AWS"));
    let playbook = service
        .playbooks()
        .await
        .map_err(|_| "store")?
        .load_playbook(&id)
        .await
        .map_err(|_| "load")?;
    assert_eq!(playbook.origin, portal);
    let [playbook_store::Step::Semantic { intent }] = playbook.steps.as_slice() else {
        return Err("single semantic step".into());
    };
    assert_eq!(
        intent.entry_url.as_deref(),
        Some("https://aws.amazon.com/billing/")
    );
    // Second invocation of the same phrasing is now a tier-1 hit: the
    // command router matches the stored key instead of re-resolving.
    let saved = service
        .playbooks()
        .await
        .map_err(|_| "store")?
        .list_playbooks()
        .await
        .map_err(|_| "list")?;
    assert_eq!(
        orchestration_engine::resolve_command(prompt, Some(&portal), &saved),
        Some(orchestration_engine::CommandMatch::Saved { id: id.clone() }),
        "learned phrasing replays from storage"
    );
    // An unknown or evicted run still fails closed rather than inventing
    // a workflow to save.
    assert!(matches!(
        service
            .save_run_as_workflow("no-such-run".into(), "ghost".into(), None)
            .await,
        Err(AppError::InvalidInput(_))
    ));
    Ok(())
}

#[test]
fn snapshot_telemetry_counts_noun_mentions_case_insensitively() {
    // Pure line shape: exact example format, mixed-case evidence all
    // counting toward the noun.
    assert_eq!(
        AppService::snapshot_telemetry_line(142, 3, "invoice"),
        "ax_snapshot_telemetry: total_nodes=142, target_noun_matches=3 (noun='invoice')"
    );
    let elements = vec![
        browser_driver::AxElement {
            backend_node_id: 1,
            role: "link".into(),
            name: "Download".into(),
            description: String::new(),
            container_text: vec!["Invoices".into(), "INV-001".into()],
            landmark: None,
        },
        browser_driver::AxElement {
            backend_node_id: 2,
            role: "link".into(),
            name: "INVOICE-2".into(),
            description: String::new(),
            container_text: Vec::new(),
            landmark: None,
        },
        browser_driver::AxElement {
            backend_node_id: 3,
            role: "link".into(),
            name: "Settings".into(),
            description: "Preferences".into(),
            container_text: vec!["General".into()],
            landmark: None,
        },
    ];
    assert_eq!(AppService::count_noun_matches(&elements, "invoice"), 2);
    assert_eq!(AppService::count_noun_matches(&elements, "INVOICE"), 2);
    assert_eq!(AppService::count_noun_matches(&elements, ""), 0);
    assert_eq!(AppService::count_noun_matches(&[], "invoice"), 0);
}

#[test]
fn telemetry_log_combines_full_diagnostic_stream() {
    // `telemetryLog` carries every line journaled for the run — counter,
    // check, resync action, stats — in journal order, so Session
    // Activity shows predicate debugging without polling the store.
    let lines = vec![
        "ax_resync_counter: before_discard=0 after_drain=1".to_owned(),
        "ax_resync_check: nodes=0 url='https://github.com/account/billing/history' predicate=true"
            .to_owned(),
        "ax_target_resync: re-enabled accessibility after 0-node tree".to_owned(),
        "ax_snapshot_telemetry: total_nodes=142, target_noun_matches=3 (noun='invoice')".to_owned(),
    ];
    let Some(combined) = AppService::combine_telemetry(&lines) else {
        panic!("non-empty stream combines");
    };
    let mut cursor = 0;
    for line in &lines {
        let position = combined[cursor..]
            .find(line.as_str())
            .unwrap_or_else(|| panic!("combined holds {line}"));
        cursor += position + line.len();
    }
    assert_eq!(combined.lines().count(), lines.len());
    // Paths that never snapshot stay silent instead of emitting empties.
    assert_eq!(AppService::combine_telemetry(&[]), None);
}

#[test]
fn batch_outcome_keys_registry_only_for_completed_runs() {
    // The UI's Save button keys off `runId`: completed batches carry
    // the journal id straight to `save_run_as_workflow`, while every
    // other terminal state offers no save (nothing was remembered).
    let completed = AppService::batch_outcome(
        "invoices".into(),
        vec![],
        orchestration_engine::SequenceStatus::Completed,
        1,
        None,
        "run-7-test",
        None,
        None,
    );
    assert_eq!(completed.run_id.as_deref(), Some("run-7-test"));
    for status in [
        orchestration_engine::SequenceStatus::Failed,
        orchestration_engine::SequenceStatus::Denied,
    ] {
        let outcome = AppService::batch_outcome(
            "invoices".into(),
            vec![],
            status,
            0,
            Some(0),
            "run-7-test",
            None,
            None,
        );
        assert_eq!(outcome.run_id, None);
    }
}

#[test]
fn snapshot_journal_lines_follow_strict_order_without_side_channels() {
    // Inline construction from owned snapshot values — no drains, no
    // locks — generates every journal line in exact sequence: counter,
    // check, CDP error, resync action, stats.
    let elements = vec![browser_driver::AxElement {
        backend_node_id: 1,
        role: "link".into(),
        name: "Download".into(),
        description: String::new(),
        container_text: vec!["Invoices".into()],
        landmark: None,
    }];
    let intent = macro_engine::SemanticIntent {
        role: "link".into(),
        label_query: "download".into(),
        container_query: None,
        raw_prompt: "download all my invoices".into(),
        ordinal_index: None,
        is_last: false,
        is_plural: true,
        entry_url: None,
        primary_target_noun: Some("invoice".into()),
    };
    let mut check = browser_driver::AxResyncCheck::new(0, None);
    check.cdp_error = Some("Session detached".to_owned());
    let lines = AppService::snapshot_journal_lines(&elements, &check, 1, &intent);
    assert_eq!(
        lines,
        vec![
            "ax_resync_counter: before_discard=0 after_drain=1".to_owned(),
            "ax_resync_check: nodes=0 url='' predicate=false".to_owned(),
            "ax_snapshot_cdp_error: 'Session detached'".to_owned(),
            "ax_target_resync: re-enabled accessibility after 0-node tree".to_owned(),
            "ax_snapshot_telemetry: total_nodes=1, target_noun_matches=1 (noun='invoice')"
                .to_owned(),
        ]
    );
    // Healthy snapshots journal only counter plus stats: no check, no
    // error, no resync action.
    let healthy = browser_driver::AxResyncCheck::new(3, None);
    let lines = AppService::snapshot_journal_lines(&elements, &healthy, 0, &intent);
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("ax_resync_counter: "));
    assert!(lines[1].starts_with("ax_snapshot_telemetry: "));
}

#[tokio::test]
async fn snapshot_telemetry_logs_empty_field_and_fails_closed()
-> Result<(), Box<dyn std::error::Error>> {
    // Page where the target rows never appear: no node mentions the
    // noun, the field fails closed with no batch, and the telemetry
    // line still lands in Session Activity for diagnosis.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let intent = macro_engine::SemanticIntent {
        role: "link".into(),
        label_query: "download".into(),
        container_query: None,
        raw_prompt: "download all my invoices".into(),
        ordinal_index: None,
        is_last: false,
        is_plural: true,
        entry_url: None,
        primary_target_noun: Some("invoice".into()),
    };
    let elements = vec![
        browser_driver::AxElement {
            backend_node_id: 1,
            role: "link".into(),
            name: "Settings".into(),
            description: String::new(),
            container_text: vec!["General".into()],
            landmark: None,
        },
        browser_driver::AxElement {
            backend_node_id: 2,
            role: "link".into(),
            name: "Profile".into(),
            description: String::new(),
            container_text: vec!["Account".into()],
            landmark: None,
        },
    ];
    let check = browser_driver::AxResyncCheck::new(0, None);
    for line in AppService::snapshot_journal_lines(&elements, &check, 0, &intent) {
        service.journal_line(line).await;
    }
    assert!(matches!(
        macro_engine::resolve_batch(&elements, &intent),
        macro_engine::ResolveOutcome::NoMatch(_)
    ));
    let events = service.test_session_events().await.map_err(|_| "events")?;
    assert!(
        events.iter().any(|outcome| outcome
            == "ax_snapshot_telemetry: total_nodes=2, target_noun_matches=0 (noun='invoice')"),
        "telemetry logged, got {events:?}"
    );
    Ok(())
}

#[tokio::test]
async fn batch_approval_names_resolved_entry_destination() -> Result<(), Box<dyn std::error::Error>>
{
    // The gate card surfaces host+path (never query) when the batch
    // will navigate first — same decide-gate choreography as before.
    let service = AppService::new(PathBuf::new(), PathBuf::new());
    let candidates = vec![browser_driver::AxElement {
        backend_node_id: 7,
        role: "link".into(),
        name: "Download".into(),
        description: String::new(),
        container_text: vec!["INV-007".into()],
        landmark: None,
    }];
    let intent = macro_engine::SemanticIntent {
        role: "link".into(),
        label_query: "download".into(),
        container_query: None,
        raw_prompt: String::new(),
        ordinal_index: None,
        is_last: false,
        is_plural: true,
        entry_url: Some("https://github.com/settings/billing?tab=x".into()),
        primary_target_noun: None,
    };
    let mut captured: Vec<PlaybookEvent> = Vec::new();
    let mut push = |event: PlaybookEvent| captured.push(event);
    let events = std::sync::Mutex::new(&mut push);
    let (approved, ()) = tokio::join!(
        service.approve_batch(43, &candidates, &intent, "download all", &events),
        async {
            tokio::task::yield_now().await;
            let _ = service.decide_playbook(43, 0, true);
        }
    );
    assert!(approved);
    let approval = captured[0].approval.as_ref().ok_or("gate card emitted")?;
    assert_eq!(
        approval.summary,
        "Batch click: 1 link controls for download all @ github.com/settings/billing"
    );
    Ok(())
}

#[tokio::test]
async fn natural_commands_reject_empties_and_auto_acquire_adhoc()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    // Empty prompts fail before touching storage.
    assert!(matches!(
        service.dispatch_natural_command("   ".into(), |_| {}).await,
        Err(AppError::InvalidInput(_))
    ));
    // No connected portal: an ungroundable non-direct prompt fails
    // honestly instead of being routed to a search page. With no
    // Chromium present the lane would fail even earlier, but the
    // browser is never reached — the proposal misses before any
    // auto-acquire attempt, and the session origin stays unset.
    let _chromium = ChromiumEnvGuard::hold_bogus();
    assert!(matches!(
        service
            .dispatch_natural_command("download my report".into(), |_| {})
            .await,
        Err(AppError::InvalidInput(_))
    ));
    let events = service.test_session_events().await.map_err(|_| "events")?;
    assert!(
        events
            .iter()
            .any(|outcome| outcome.contains("route_resolution_miss")),
        "honest miss journaled, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|outcome| outcome.starts_with("route_fallback:")),
        "no search fallback journaled, got {events:?}"
    );
    let connected = service
        .session_origin
        .lock()
        .map_err(|_| "session")?
        .clone();
    assert!(connected.is_none(), "no portal invented");
    Ok(())
}

#[tokio::test]
async fn funnel_preempts_old_proposal_in_connected_dispatch()
-> Result<(), Box<dyn std::error::Error>> {
    // Work item 6b: the B4 regression pinned the ad-hoc lane, but the
    // live incident ran on a warm browser — the connected
    // single-ephemeral lane (`dispatch_single_ephemeral`). A
    // funnel-claimed prompt must journal the funnel's slots before,
    // and instead of, any old `propose_entry_url` machinery there too.
    // Uses the exact live prompt: the funnel must claim it for
    // `reddit` (never `log`) and take AlreadyOnOrigin on the reddit
    // portal, so no browser or network is touched.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let portal = url::Url::parse("https://www.reddit.com/").map_err(|_| "portal")?;
    service.test_connect(portal).map_err(|_| "connect")?;
    let outcome = service
        .dispatch_natural_command("open reddit for me i want to log in".to_owned(), |_| {})
        .await;
    assert!(
        outcome.is_ok(),
        "already-on-origin completes without a browser"
    );
    let events = service.test_session_events().await.map_err(|_| "events")?;
    let first_funnel = events
        .iter()
        .position(|line| line.starts_with("funnel_"))
        .ok_or("the funnel must journal on a funnel-claimed prompt")?;
    assert!(
        events[first_funnel].starts_with("funnel_slots:"),
        "the funnel's first line must be its slots, got: {}",
        events[first_funnel]
    );
    assert!(
        events[first_funnel].contains("site=Some(\"reddit\")"),
        "the site slot is reddit, never log: {}",
        events[first_funnel]
    );
    for line in &events {
        // The funnel's own route lines carry the `funnel` marker;
        // any other line with these prefixes is the old proposal
        // machinery running on a funnel-claimed prompt.
        assert!(
            !line.starts_with("route_proposed:") || line.contains("funnel"),
            "old proposal machinery ran on a funnel-claimed prompt: {line}"
        );
        assert!(
            !line.starts_with("route_fallback:"),
            "old search fallback ran on a funnel-claimed prompt: {line}"
        );
        assert!(
            !line.starts_with("route_resolution_miss:"),
            "old resolution miss ran on a funnel-claimed prompt: {line}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn adhoc_proposal_never_invents_a_destination() -> Result<(), Box<dyn std::error::Error>> {
    // Hermetic proof: unknown prompts are an honest miss — no invented
    // amazon.com / amazon.in and no search template. No browser needed —
    // pure tiered resolution plus journaling. ("find amazon", not "open
    // amazon for me": a bare direct open is a ladder miss — see
    // `direct_open_miss_is_guidance_not_search`.)
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let mut intent = macro_engine::SemanticIntent {
        role: "link".into(),
        label_query: "amazon".into(),
        container_query: None,
        raw_prompt: "find amazon".into(),
        ordinal_index: None,
        is_last: false,
        is_plural: false,
        entry_url: None,
        primary_target_noun: Some("amazon".into()),
    };
    let proposed = service
        .propose_entry_url("find amazon", &mut intent, None)
        .await;
    // No destination attached: neither a guessed TLD nor a search page.
    assert_eq!(intent.entry_url, None, "no destination invented");
    assert!(proposed.source.is_none(), "no tier answered");
    let line = proposed.log.ok_or("route line")?;
    assert!(
        line.contains("route_resolution_miss"),
        "honest miss journaled, got {line:?}"
    );
    assert!(
        !intent
            .entry_url
            .as_deref()
            .unwrap_or("")
            .contains("amazon.com")
    );
    assert!(
        !intent
            .entry_url
            .as_deref()
            .unwrap_or("")
            .contains("amazon.in")
    );
    Ok(())
}

#[tokio::test]
async fn adhoc_dispatch_fails_honestly_when_ungrounded() -> Result<(), Box<dyn std::error::Error>> {
    // Hermetic ad-hoc proof without a real Chromium binary:
    // - `dispatch_natural_command` with no session never launches the
    //   browser for an ungroundable prompt — the proposal misses before
    //   any auto-acquire attempt,
    // - the run fails with the honest "not runnable" input error, never
    //   by navigating a search page and calling it complete,
    // - the session is never anchored to an invented origin.
    // ("find amazon", not "open amazon for me": a bare direct open
    // stops earlier as a direct-open miss — see
    // `direct_open_miss_is_guidance_not_search`.)
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let _chromium = ChromiumEnvGuard::hold_bogus();
    assert!(matches!(
        service
            .dispatch_natural_command("find amazon".into(), |_| {})
            .await,
        Err(AppError::InvalidInput(_))
    ));
    let events = service.test_session_events().await.map_err(|_| "events")?;
    assert!(
        events
            .iter()
            .any(|outcome| outcome.contains("route_resolution_miss")),
        "honest miss journaled, got {events:?}"
    );
    // No search navigation was ever attempted, so no destination was
    // claimed and nothing re-anchored the session.
    for prefix in ["route_fallback:", "search_followed:", "portal_reanchored:"] {
        assert!(
            !events.iter().any(|outcome| outcome.starts_with(prefix)),
            "no {prefix} without a destination, got {events:?}"
        );
    }
    let connected = service
        .session_origin
        .lock()
        .map_err(|_| "session")?
        .clone();
    assert!(connected.is_none(), "no portal invented");
    Ok(())
}

#[tokio::test]
async fn initialize_records_startup_build_first() -> Result<(), Box<dyn std::error::Error>> {
    // Startup telemetry: the build line is the first `session_events`
    // row of the boot, and the `initialize` response carries the exact
    // same string the UI renders at the top of Session Activity.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    let status = service.initialize().await.map_err(|_| "initialize")?;
    assert!(status.ready);
    let events = service.test_session_events().await.map_err(|_| "events")?;
    let first = events.first().ok_or("expected a startup row")?;
    assert!(
        first.starts_with("startup_build: v"),
        "versioned prefix, got {first:?}"
    );
    assert!(first.contains(" · hash:"), "hash suffix, got {first:?}");
    let hash = first.rsplit("hash:").next().ok_or("hash part")?;
    assert!(!hash.trim().is_empty(), "non-empty hash, got {first:?}");
    assert_eq!(status.startup_build, *first);
    Ok(())
}

#[tokio::test]
async fn poc_metrics_reads_empty_and_seeded_databases() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    // A fresh database reports zeros with no replay share, never an error.
    let empty = service.poc_metrics().await.map_err(|_| "metrics")?;
    assert_eq!(empty.total_runs, 0);
    assert_eq!(empty.completed_tasks, 0);
    assert_eq!(empty.macro_replay_pct, None);
    assert_eq!(empty.sync_imported, 0);
    assert_eq!(empty.sync_fallback, 0);
    // Seed one completed replay, one completed record, and mixed sync rows.
    let pool = service.database().await.map_err(|_| "database")?;
    sqlx::query("CREATE TABLE tasks(id INTEGER PRIMARY KEY, revision INTEGER NOT NULL, snapshot TEXT NOT NULL)")
        .execute(pool)
        .await?;
    for (state, mode) in [("completed", "replay"), ("completed", "record")] {
        sqlx::query("INSERT INTO tasks(revision, snapshot) VALUES(0, ?)")
            .bind(format!("{{\"state\":\"{state}\",\"mode\":\"{mode}\"}}"))
            .execute(pool)
            .await?;
    }
    for outcome in [
        "cookies_imported_unverified",
        "manual_login_requested",
        "playbook_decision:1:0:approved",
    ] {
        sqlx::query("INSERT INTO session_events(outcome) VALUES(?)")
            .bind(outcome)
            .execute(pool)
            .await?;
    }
    let store = playbook_store::PlaybookStore::new(pool.clone());
    store
        .record_run_start("run-1", None, playbook_store::RunKind::Ephemeral, 2)
        .await?;
    store.record_run_finish("run-1", "completed", 2).await?;
    let metrics = service.poc_metrics().await.map_err(|_| "metrics")?;
    assert_eq!(metrics.total_runs, 1);
    assert_eq!(metrics.completed_tasks, 2);
    assert_eq!(metrics.macro_replay_pct, Some(50.0));
    assert_eq!(metrics.sync_imported, 1);
    assert_eq!(metrics.sync_fallback, 1);
    assert_eq!(
        metrics
            .sync_by_outcome
            .get("playbook_decision:1:0:approved"),
        Some(&1)
    );
    assert_eq!(metrics.runs_by_status.get("completed"), Some(&1));
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn failed_card_journal_is_scoped_to_the_failed_run() -> Result<(), Box<dyn std::error::Error>>
{
    // A FAILED card's journal must contain only the failed run's lines:
    // the boot `startup_build` line (journaled with no run id) and an
    // earlier synthetic run's lines must not leak in.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    service.begin_journal_run().map_err(|_| "begin run 1")?;
    service
        .record("run_one_line")
        .await
        .map_err(|_| "record run 1")?;
    service.begin_journal_run().map_err(|_| "begin run 2")?;
    service
        .record("run_two_first")
        .await
        .map_err(|_| "record run 2a")?;
    service
        .record("run_two_second")
        .await
        .map_err(|_| "record run 2b")?;
    let journal = service.recent_journal(16).await;
    assert_eq!(
        journal,
        vec!["run_two_first".to_owned(), "run_two_second".to_owned()],
        "only the current run's lines, oldest first, got {journal:?}"
    );
    // The store still holds everything (Session Activity is unscoped);
    // only the failed-run payload is run-scoped.
    let events = service.test_session_events().await.map_err(|_| "events")?;
    assert_eq!(
        events.len(),
        4,
        "boot + run 1 + run 2 lines, got {events:?}"
    );
    assert!(
        events
            .first()
            .is_some_and(|line| line.starts_with("startup_build")),
        "boot line is still journaled first, got {events:?}"
    );
    Ok(())
}

#[tokio::test]
async fn shortcut_offer_journaled_only_after_landing() -> Result<(), Box<dyn std::error::Error>> {
    // The "Save as Shortcut" card is derived from the `shortcut_offer:`
    // journal line, and that line has exactly one producer:
    // `offer_shortcut_after_landing`. A grounder hit journals it; no hit
    // journals nothing.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let hit = ProposedEntry {
        grounder_hit: Some(GrounderHit {
            site: "amazon".to_owned(),
            url: "https://www.amazon.in".to_owned(),
        }),
        ..ProposedEntry::default()
    };
    let line = service
        .offer_shortcut_after_landing(&hit)
        .await
        .ok_or("offer line")?;
    assert!(line.starts_with("shortcut_offer:"), "got {line:?}");
    assert!(
        line.contains("amazon") && line.contains("https://www.amazon.in"),
        "line names the site and URL, got {line:?}"
    );
    let events = service.test_session_events().await.map_err(|_| "events")?;
    assert!(
        events.iter().any(|event| event.contains("shortcut_offer:")),
        "offer journaled: {events:?}"
    );
    // No hit, no offer, no journal line.
    let before = events.len();
    let none = service
        .offer_shortcut_after_landing(&ProposedEntry::default())
        .await;
    assert!(none.is_none(), "no hit means no offer");
    let events = service.test_session_events().await.map_err(|_| "events")?;
    assert_eq!(events.len(), before, "nothing journaled without a hit");
    Ok(())
}

#[tokio::test]
async fn proposal_alone_never_journals_shortcut_offer() -> Result<(), Box<dyn std::error::Error>> {
    // Regression guard for the old behavior (the suggestion was
    // journaled at proposal time): running the ladder alone — an
    // honest miss and a direct-open miss — must never produce the offer
    // line. Only a post-landing call to `offer_shortcut_after_landing`
    // may.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let mut intent = macro_engine::SemanticIntent {
        role: "link".into(),
        label_query: "amazon".into(),
        container_query: None,
        raw_prompt: "find amazon".into(),
        ordinal_index: None,
        is_last: false,
        is_plural: false,
        entry_url: None,
        primary_target_noun: Some("amazon".into()),
    };
    let proposed = service
        .propose_entry_url("find amazon", &mut intent, None)
        .await;
    assert!(proposed.source.is_none(), "honest miss, no tier answered");
    let origin = url::Url::parse("https://www.google.com/").ok();
    let Some(orchestration_engine::CommandMatch::Ephemeral { mut intent }) =
        orchestration_engine::resolve_command("open amazon for me", origin.as_ref(), &[])
    else {
        panic!("ad-hoc prompt resolves ephemeral");
    };
    let proposed = service
        .propose_entry_url("open amazon for me", &mut intent, None)
        .await;
    assert!(proposed.direct_open_miss, "ladder miss flagged");
    let events = service.test_session_events().await.map_err(|_| "events")?;
    assert!(
        !events.iter().any(|event| event.contains("shortcut_offer:")),
        "proposal alone never offers: {events:?}"
    );
    Ok(())
}

#[tokio::test]
async fn accepting_shortcut_offer_hits_shortcut_rung_next_run()
-> Result<(), Box<dyn std::error::Error>> {
    // What the card's Save button invokes: `save_site_shortcut` persists
    // `amazon → https://www.amazon.in` to SQLite; the next dispatch
    // loads it into the ladder and the engine answers from the shortcut
    // rung — zero tokens, no grounding.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "initialize")?;
    let saved = service
        .save_site_shortcut("amazon".to_owned(), "https://www.amazon.in/".to_owned())
        .await
        .map_err(|_| "save_site_shortcut")?;
    assert_eq!(saved.name, "amazon");
    assert_eq!(saved.url, "https://www.amazon.in/");
    // The next dispatch's shortcut map, built exactly the way the
    // production lane builds it.
    let map = service.load_shortcut_map().await.ok_or("shortcut map")?;
    let shortcuts = orchestration_engine::InMemoryShortcuts::new(map);
    let ctx = orchestration_engine::ResolutionContext {
        llm: None,
        parser: None,
        shortcuts: Some(&shortcuts),
        site_search: None,
        domain_grounder: None,
        region_hint: "",
    };
    let resolved =
        orchestration_engine::resolve_entry_url("open amazon for me", None, &ctx).ok_or("route")?;
    assert_eq!(
        resolved.source,
        orchestration_engine::RouteSource::Shortcut,
        "saved shortcut answers the next run"
    );
    assert_eq!(resolved.url.as_str(), "https://www.amazon.in/");
    Ok(())
}
