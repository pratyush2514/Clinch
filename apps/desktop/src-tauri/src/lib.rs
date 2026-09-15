#![deny(unsafe_code)]
mod service;
use service::{AppError, AppService, ApprovalPreview, SessionStatus, StorageStatus};
use tauri::Manager;

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
        sync_session,
        manual_login,
        close_browser,
        preview_approval,
        resolve_approval
    ])
}

pub fn run() {
    let result = with_commands(tauri::Builder::default())
        .setup(|app| {
            let data = app.path().app_data_dir()?;
            let home = app.path().home_dir()?;
            app.manage(AppService::new(data, home));
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
