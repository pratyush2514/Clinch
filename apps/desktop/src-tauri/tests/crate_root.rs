//! Integration tests for the `clinch-desktop` crate root.
//!
//! Moved out of `src/lib.rs` so the main source stays test-free.

mod common;

use clinch_desktop::*;
use common::ChromiumEnvGuard;
use common::test_support::{response_text, spawn_mock_daemon};
use std::sync::atomic::Ordering;
use tauri::{
    Manager,
    ipc::{CallbackFn, InvokeBody},
    test::{INVOKE_KEY, get_ipc_response, mock_builder, mock_context, noop_assets},
    webview::InvokeRequest,
};

#[derive(serde::Deserialize)]
struct Preview {
    id: u64,
}

#[tokio::test]
async fn run_button_dispatch_proposes_route_and_logs() -> Result<(), Box<dyn std::error::Error>> {
    // The exact command string the frontend Run button invokes
    // (`CommandBar.tsx` → `invoke("dispatch_natural_command", …)`).
    // With a github session connected, the IPC handler must reach the
    // ephemeral dispatch path: route proposal runs before any browser
    // attach, so even though no Chromium exists here
    // (`browser_unavailable`), `session_events` still carries the
    // honest route miss the ladder journaled.
    let dir = tempfile::tempdir()?;
    let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
    service.initialize().await.map_err(|_| "init")?;
    service
        .test_connect(url::Url::parse("https://github.com/").map_err(|_| "url")?)
        .map_err(|_| "connect")?;
    let app = with_commands(mock_builder())
        .manage(service)
        // Embedded mode: no daemon URL, so the command must run the
        // local engine path exactly as before this change.
        .manage(DaemonConfig::new(None))
        .build(mock_context(noop_assets()))?;
    let webview =
        tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default()).build()?;
    // `tauri://localhost` is the local origin on desktop: Tauri skips the
    // capability ACL for app commands invoked locally (the app defines
    // no app ACL manifest). `http://tauri.localhost` is the Windows
    // form and counts as *remote* here, so every command is rejected
    // with "not allowed".
    let origin = url::Url::parse("tauri://localhost")?;
    let _chromium = ChromiumEnvGuard::hold_bogus();
    let request = InvokeRequest {
        cmd: "dispatch_natural_command".into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: origin,
        body: InvokeBody::Json(serde_json::json!({
            "prompt": "download all my invoices from github",
            "progress": "__CHANNEL__:0",
        })),
        headers: tauri::http::HeaderMap::new(),
        invoke_key: INVOKE_KEY.into(),
    };
    let error = get_ipc_response(&webview, request)
        .err()
        .ok_or("expected browser_unavailable past proposal")?;
    assert_eq!(error["code"], "browser_unavailable");
    // Proposal ran on the IPC path before the browser attach failed:
    // the ladder missed honestly — no destination invented, no search
    // page proposed.
    let events = app
        .state::<AppService>()
        .test_session_events()
        .await
        .map_err(|_| "events")?;
    assert!(
        events
            .iter()
            .any(|outcome| outcome.contains("route_resolution_miss")),
        "honest route miss logged, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|outcome| outcome.starts_with("route_fallback:")),
        "no search fallback proposed, got {events:?}"
    );
    // No fabricated deep link reaches the journal.
    assert!(
        !events
            .iter()
            .any(|outcome| outcome.contains("github.com/account")),
        "no invented portal route, got {events:?}"
    );
    Ok(())
}

#[test]
fn task_channel_command_requires_connected_session() -> Result<(), Box<dyn std::error::Error>> {
    let app = with_commands(mock_builder())
        .manage(AppService::new(
            std::path::PathBuf::new(),
            std::path::PathBuf::new(),
        ))
        .manage(DaemonConfig::new(None))
        .build(mock_context(noop_assets()))?;
    let webview =
        tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default()).build()?;
    let request = InvokeRequest {
        cmd: "run_task".into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: url::Url::parse("tauri://localhost")?,
        body: InvokeBody::Json(serde_json::json!({
            "request": {"workflow":"reports", "portalUrl":"https://example.com/files", "linkSelector":null, "downloadSelector":"a.report"},
            "progress":"__CHANNEL__:7"
        })),
        headers: tauri::http::HeaderMap::new(),
        invoke_key: INVOKE_KEY.into(),
    };
    let error = get_ipc_response(&webview, request)
        .err()
        .ok_or("Expected session rejection")?;
    assert_eq!(error["code"], "session_required");
    Ok(())
}

#[test]
fn approval_and_consent_cross_real_command_dispatch() -> Result<(), Box<dyn std::error::Error>> {
    let app = with_commands(mock_builder())
        .manage(AppService::new(
            std::path::PathBuf::new(),
            std::path::PathBuf::new(),
        ))
        .manage(DaemonConfig::new(None))
        .build(mock_context(noop_assets()))?;
    let webview =
        tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default()).build()?;
    // Local origin: without it every command is ACL-rejected as remote.
    let origin = url::Url::parse("tauri://localhost")?;
    let request = |cmd: &str, body| InvokeRequest {
        cmd: cmd.into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: origin.clone(),
        body: InvokeBody::Json(body),
        headers: tauri::http::HeaderMap::new(),
        invoke_key: INVOKE_KEY.into(),
    };
    let preview = get_ipc_response(&webview, request("preview_approval", serde_json::json!({})))
        .map_err(|_| std::io::Error::other("preview IPC failed"))?
        .deserialize::<Preview>()?;
    let decision = request(
        "resolve_approval",
        serde_json::json!({"id": preview.id, "approved": true}),
    );
    let accepted = get_ipc_response(&webview, decision)
        .map_err(|_| std::io::Error::other("decision IPC failed"))?
        .deserialize::<bool>()?;
    assert!(accepted);
    let duplicate = request(
        "resolve_approval",
        serde_json::json!({"id": preview.id, "approved": true}),
    );
    assert!(get_ipc_response(&webview, duplicate).is_err());
    let denied = request(
        "sync_session",
        serde_json::json!({"request": {"browser": "chrome", "profile": "Default", "portalUrl": "https://example.com", "consent": false}}),
    );
    assert!(get_ipc_response(&webview, denied).is_err());
    Ok(())
}

// NOTE: multi-thread flavor is required: `get_ipc_response` blocks the
// calling thread on a std channel while the mock daemon's accept task
// needs a free worker to run the WebSocket handshake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_mode_proxies_command_to_daemon() -> Result<(), Box<dyn std::error::Error>> {
    use clinch_protocol::ServerResponse;

    // The mock echoes the command name it received, proving the proxy
    // sent the right `cmd` with the right arg names.
    let url = spawn_mock_daemon(|request| {
        vec![response_text(&ServerResponse::ok(
            request.id,
            serde_json::json!({"got": request.cmd}),
        ))]
    })
    .await?;
    let app = with_commands(mock_builder())
        .manage(AppService::new(
            std::path::PathBuf::new(),
            std::path::PathBuf::new(),
        ))
        .manage(DaemonConfig::new(Some(url)))
        .build(mock_context(noop_assets()))?;
    let webview =
        tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default()).build()?;
    let request = InvokeRequest {
        cmd: "browser_context_status".into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: url::Url::parse("tauri://localhost")?,
        body: InvokeBody::Json(serde_json::json!({})),
        headers: tauri::http::HeaderMap::new(),
        invoke_key: INVOKE_KEY.into(),
    };
    let value: serde_json::Value = get_ipc_response(&webview, request)
        .map_err(|error| std::io::Error::other(format!("proxy IPC failed: {error}")))?
        .deserialize()?;
    assert_eq!(value, serde_json::json!({"got": "browser_context_status"}));
    Ok(())
}

// NOTE: multi-thread flavor is required: `get_ipc_response` blocks the
// calling thread on a std channel while the mock daemon's accept task
// needs a free worker to run the WebSocket handshake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_mode_surfaces_daemon_errors_untouched() -> Result<(), Box<dyn std::error::Error>> {
    use clinch_protocol::ServerResponse;

    let daemon_error = serde_json::json!({"code": "busy", "message": null});
    let url = spawn_mock_daemon({
        let daemon_error = daemon_error.clone();
        move |request| {
            vec![response_text(&ServerResponse::err(
                request.id,
                daemon_error.clone(),
            ))]
        }
    })
    .await?;
    let app = with_commands(mock_builder())
        .manage(AppService::new(
            std::path::PathBuf::new(),
            std::path::PathBuf::new(),
        ))
        .manage(DaemonConfig::new(Some(url)))
        .build(mock_context(noop_assets()))?;
    let webview =
        tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default()).build()?;
    let request = InvokeRequest {
        cmd: "close_browser".into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: url::Url::parse("tauri://localhost")?,
        body: InvokeBody::Json(serde_json::json!({})),
        headers: tauri::http::HeaderMap::new(),
        invoke_key: INVOKE_KEY.into(),
    };
    let error = get_ipc_response(&webview, request)
        .err()
        .ok_or("expected the daemon error to surface")?;
    assert_eq!(error, daemon_error);
    Ok(())
}

#[test]
fn wsl_path_translation_maps_linux_paths_to_unc() {
    assert_eq!(
        wsl_path_to_unc("/home/u/dl/invoice.pdf", "Ubuntu").as_deref(),
        Some("\\\\wsl$\\Ubuntu\\home\\u\\dl\\invoice.pdf")
    );
    assert_eq!(
        wsl_path_to_unc("/tmp/a", "Debian").as_deref(),
        Some("\\\\wsl$\\Debian\\tmp\\a")
    );
    // Non-absolute paths have nothing sane to map to.
    assert_eq!(wsl_path_to_unc("relative/path", "Ubuntu"), None);
    assert_eq!(wsl_path_to_unc("", "Ubuntu"), None);
}

#[test]
fn context_generation_bumps_monotonically() {
    let config = DaemonConfig::new(None);
    assert_eq!(config.context_generation().load(Ordering::SeqCst), 0);
    assert_eq!(config.bump_context_generation(), 1);
    assert_eq!(config.bump_context_generation(), 2);
}

#[test]
fn daemon_config_remote_detection() {
    assert!(!DaemonConfig::new(None).remote());
    assert!(DaemonConfig::new(Some("ws://127.0.0.1:18790".to_owned())).remote());
}
