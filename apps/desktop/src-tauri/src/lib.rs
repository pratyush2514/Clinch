#![deny(unsafe_code)]
mod auth;
mod service;
mod ws_server;
use auth::AuthPanel;
use service::{
    AppError, AppService, ApprovalPreview, BridgeStatus, IntentPreview, PickerStatus,
    SessionStatus, StorageStatus,
};
use tauri::Manager;
use tauri_plugin_opener::OpenerExt;

#[tauri::command]
async fn downloaded_file_action<R: tauri::Runtime>(
    id: orchestration_engine::TaskId,
    index: usize,
    reveal: bool,
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppService>,
) -> Result<(), AppError> {
    let path = state.downloaded_file(id, index).await?;
    if reveal {
        app.opener()
            .reveal_item_in_dir(path)
            .map_err(|_| AppError::StorageUnavailable)
    } else {
        app.opener()
            .open_path(path.to_string_lossy(), None::<&str>)
            .map_err(|_| AppError::StorageUnavailable)
    }
}

#[tauri::command]
async fn task_decision(
    id: orchestration_engine::TaskId,
    index: usize,
    approved: bool,
    state: tauri::State<'_, AppService>,
) -> Result<(), AppError> {
    state.decide_task(id, index, approved).await
}
#[tauri::command]
async fn browser_viewport(
    state: tauri::State<'_, AppService>,
) -> Result<browser_driver::Viewport, AppError> {
    state.viewport().await
}
#[tauri::command]
async fn initialize(state: tauri::State<'_, AppService>) -> Result<StorageStatus, AppError> {
    state.initialize().await
}
#[tauri::command]
async fn sync_session(
    request: session_sync::SyncRequest,
    state: tauri::State<'_, AppService>,
) -> Result<SessionStatus, AppError> {
    state.sync(request).await
}
#[tauri::command]
async fn manual_login(
    portal_url: String,
    state: tauri::State<'_, AppService>,
) -> Result<SessionStatus, AppError> {
    state.manual_login(&portal_url).await
}
#[tauri::command]
async fn close_browser(state: tauri::State<'_, AppService>) -> Result<(), AppError> {
    state.close_browser().await
}
#[tauri::command]
async fn bridge_status(state: tauri::State<'_, AppService>) -> Result<BridgeStatus, AppError> {
    state.bridge_status()
}
#[tauri::command]
async fn bridge_sync_session(
    portal_url: String,
    state: tauri::State<'_, AppService>,
) -> Result<SessionStatus, AppError> {
    state.bridge_sync(&portal_url).await
}
#[tauri::command]
async fn auth_status(state: tauri::State<'_, AppService>) -> Result<Option<AuthPanel>, AppError> {
    state.auth_status()
}
#[tauri::command]
async fn begin_embedded_auth(
    portal_url: String,
    state: tauri::State<'_, AppService>,
) -> Result<AuthPanel, AppError> {
    state.begin_embedded_auth(&portal_url).await
}
#[tauri::command]
async fn complete_embedded_auth(
    state: tauri::State<'_, AppService>,
) -> Result<SessionStatus, AppError> {
    state.complete_embedded_auth().await
}
#[tauri::command]
async fn cancel_embedded_auth(state: tauri::State<'_, AppService>) -> Result<(), AppError> {
    state.cancel_embedded_auth().await
}
#[tauri::command]
async fn picker_enable(state: tauri::State<'_, AppService>) -> Result<(), AppError> {
    state.picker_enable().await
}
#[tauri::command]
async fn picker_status(state: tauri::State<'_, AppService>) -> Result<PickerStatus, AppError> {
    state.picker_status()
}
#[tauri::command]
async fn picker_pick(
    timeout_ms: Option<u64>,
    state: tauri::State<'_, AppService>,
) -> Result<browser_driver::PickedElement, AppError> {
    state.picker_pick(timeout_ms.unwrap_or(60_000)).await
}
#[tauri::command]
async fn picker_disable(state: tauri::State<'_, AppService>) -> Result<(), AppError> {
    state.picker_disable().await
}
#[tauri::command]
async fn preview_intent(
    role: String,
    label: String,
    state: tauri::State<'_, AppService>,
) -> Result<IntentPreview, AppError> {
    state.preview_intent(role, label).await
}
#[tauri::command]
async fn save_playbook(
    name: String,
    portal_url: String,
    steps: Vec<playbook_store::Step>,
    state: tauri::State<'_, AppService>,
) -> Result<String, AppError> {
    state.save_playbook(name, portal_url, steps).await
}
#[tauri::command]
async fn list_playbooks(
    state: tauri::State<'_, AppService>,
) -> Result<Vec<playbook_store::PlaybookSummary>, AppError> {
    state.list_playbooks().await
}
#[tauri::command]
async fn execute_playbook(
    id: String,
    progress: tauri::ipc::Channel<service::PlaybookEvent>,
    state: tauri::State<'_, AppService>,
) -> Result<orchestration_engine::SequenceOutcome, AppError> {
    state
        .execute_playbook(id, |event| {
            // If the view closes, the run still completes; the terminal
            // outcome return value carries its final state.
            let _ = progress.send(event);
        })
        .await
}
#[tauri::command]
// Tauri's CommandArg contract requires the State wrapper by value.
#[allow(clippy::needless_pass_by_value)]
fn decide_playbook(
    run_id: u64,
    index: usize,
    approved: bool,
    state: tauri::State<'_, AppService>,
) -> Result<(), AppError> {
    state.decide_playbook(run_id, index, approved)
}
#[tauri::command]
async fn harvest_invoices(
    request: orchestration_engine::InvoiceRequest,
    progress: tauri::ipc::Channel<orchestration_engine::TaskEvent>,
    state: tauri::State<'_, AppService>,
) -> Result<orchestration_engine::Task, AppError> {
    state
        .harvest(&request, |event| {
            // If the view closes, execution still checkpoints; get_task restores its durable result.
            let _ = progress.send(event);
        })
        .await
}
#[tauri::command]
async fn get_task(
    id: orchestration_engine::TaskId,
    state: tauri::State<'_, AppService>,
) -> Result<orchestration_engine::Task, AppError> {
    state.task(id).await
}
#[tauri::command]
// Tauri's CommandArg contract requires the State wrapper by value.
#[allow(clippy::needless_pass_by_value)]
fn preview_approval(state: tauri::State<'_, AppService>) -> Result<ApprovalPreview, AppError> {
    state.preview_approval()
}
#[tauri::command]
// Tauri's CommandArg contract requires the State wrapper by value.
#[allow(clippy::needless_pass_by_value)]
fn resolve_approval(
    id: u64,
    approved: bool,
    state: tauri::State<'_, AppService>,
) -> Result<bool, AppError> {
    state.resolve_approval(id, approved)
}

fn with_commands<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder.invoke_handler(tauri::generate_handler![
        initialize,
        downloaded_file_action,
        task_decision,
        browser_viewport,
        sync_session,
        manual_login,
        close_browser,
        bridge_status,
        bridge_sync_session,
        auth_status,
        begin_embedded_auth,
        complete_embedded_auth,
        cancel_embedded_auth,
        picker_enable,
        picker_status,
        picker_pick,
        picker_disable,
        preview_intent,
        save_playbook,
        list_playbooks,
        execute_playbook,
        decide_playbook,
        harvest_invoices,
        get_task,
        preview_approval,
        resolve_approval
    ])
}

pub fn run() {
    let result = with_commands(tauri::Builder::default().plugin(tauri_plugin_opener::init()))
        .setup(|app| {
            let data = app.path().app_data_dir()?;
            let home = app.path().home_dir()?;
            app.manage(AppService::new(data, home));
            // The companion extension dials on browser launch, long before any
            // sync click — bind the loopback listener here so early dials are
            // accepted instead of refused. `bridge_sync` reuses this listener.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<AppService>();
                match state.ensure_bridge().await {
                    Ok(port) => {
                        eprintln!("[clinch] companion bridge listening on 127.0.0.1:{port}");
                    }
                    Err(error) => {
                        eprintln!("[clinch] companion bridge failed to start: {error:?}");
                    }
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!());
    let Ok(app) = result else {
        eprintln!("Clinch could not start. Check the desktop runtime and app configuration.");
        std::process::exit(1);
    };
    app.run(|handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            handle.state::<AppService>().terminate_browser();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::{
        ipc::{CallbackFn, InvokeBody},
        test::{INVOKE_KEY, get_ipc_response, mock_builder, mock_context, noop_assets},
        webview::InvokeRequest,
    };

    #[derive(serde::Deserialize)]
    struct Preview {
        id: u64,
    }

    #[test]
    fn invoice_channel_command_requires_connected_session() -> Result<(), Box<dyn std::error::Error>>
    {
        let app = with_commands(mock_builder())
            .manage(AppService::new(
                std::path::PathBuf::new(),
                std::path::PathBuf::new(),
            ))
            .build(mock_context(noop_assets()))?;
        let webview =
            tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default()).build()?;
        let request = InvokeRequest {
            cmd: "harvest_invoices".into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: url::Url::parse("http://tauri.localhost")?,
            body: InvokeBody::Json(serde_json::json!({
                "request": {"workflow":"bills", "portalUrl":"https://example.com/billing", "billingSelector":null, "invoiceSelector":"a.invoice"},
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
    fn approval_and_consent_cross_real_command_dispatch() -> Result<(), Box<dyn std::error::Error>>
    {
        let app = with_commands(mock_builder())
            .manage(AppService::new(
                std::path::PathBuf::new(),
                std::path::PathBuf::new(),
            ))
            .build(mock_context(noop_assets()))?;
        let webview =
            tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default()).build()?;
        let origin = url::Url::parse("http://tauri.localhost")?;
        let request = |cmd: &str, body| InvokeRequest {
            cmd: cmd.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: origin.clone(),
            body: InvokeBody::Json(body),
            headers: tauri::http::HeaderMap::new(),
            invoke_key: INVOKE_KEY.into(),
        };
        let preview =
            get_ipc_response(&webview, request("preview_approval", serde_json::json!({})))
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
}
