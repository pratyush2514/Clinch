#![deny(unsafe_code)]
pub mod auth;
pub mod daemon_client;
pub mod service;
pub mod ws_server;
// Narrow public seam for external integration tests (`tests/`): the funnel
// dispatcher and its outcome/error types. Everything else in `service`
// stays crate-private.
// (`mod auth` is used via `crate::auth::…` in `service.rs`.)
use clinch_protocol::{cmd, event};
use daemon_client::DaemonClient;
use service::LendRequest;
pub use service::{AppError, AppService, DispatchOutcome, PlaybookEvent};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use tauri::{Emitter, Manager};
use tauri_plugin_opener::OpenerExt;

/// Remote-mode configuration, managed as Tauri state alongside `AppService`.
///
/// When `CLINCH_DAEMON_URL` is set (e.g. `ws://127.0.0.1:18790`) the app runs
/// in **remote mode**: every command below proxies to the clinch-daemon over
/// WebSocket instead of running the embedded engine. When unset this carries
/// `None` and every command runs its existing embedded body — zero behavior
/// change, embedded stays the default.
pub struct DaemonConfig {
    url: Option<String>,
    client: Mutex<Option<Arc<DaemonClient>>>,
    /// Bumped on every context acquire AND release. The screencast/cursor
    /// forwarder spawned per acquire stops when it observes a newer
    /// generation, so a stale forwarder can never emit into a new session.
    context_generation: Arc<AtomicU64>,
}

impl DaemonConfig {
    pub fn new(url: Option<String>) -> Self {
        Self {
            url,
            client: Mutex::new(None),
            context_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Reads `CLINCH_DAEMON_URL`; absent or blank means embedded mode.
    fn from_env() -> Self {
        let url = std::env::var(clinch_protocol::DAEMON_URL_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty());
        Self::new(url)
    }

    pub fn remote(&self) -> bool {
        self.url.is_some()
    }

    fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    /// Lazily connects to the daemon; the connection is then shared by all
    /// proxied commands. On failure returns an `AppError`-shaped JSON value
    /// (`{"code":"browser_unavailable","message":"daemon unreachable at …"}`)
    /// so the frontend sees the same error shape as embedded mode.
    async fn client(&self) -> Result<Arc<DaemonClient>, serde_json::Value> {
        if let Some(client) = self.client.lock().ok().and_then(|guard| guard.clone()) {
            return Ok(client);
        }
        let Some(url) = self.url.clone() else {
            return Err(remote_error(
                "browser_unavailable",
                "remote mode not configured",
            ));
        };
        match DaemonClient::connect(&url).await {
            Ok(client) => {
                let client = Arc::new(client);
                if let Ok(mut guard) = self.client.lock() {
                    *guard = Some(Arc::clone(&client));
                }
                Ok(client)
            }
            Err(reason) => Err(remote_error(
                "browser_unavailable",
                format!("daemon unreachable at {url}: {reason}"),
            )),
        }
    }

    /// Proxy one command: forward params, return the daemon's JSON untouched.
    async fn call(
        &self,
        command: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, serde_json::Value> {
        self.client().await?.call(command, params).await
    }

    pub fn bump_context_generation(&self) -> u64 {
        self.context_generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn context_generation(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.context_generation)
    }
}

/// An `AppError`-shaped JSON error value for failures synthesized on the
/// client side (unreachable daemon, bad daemon payload, …). Matches the
/// `#[serde(tag = "code", content = "message", rename_all = "snake_case")]`
/// shape `AppError` serializes to.
fn remote_error(code: &str, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({"code": code, "message": message.into()})
}

/// Embedded-mode adapter. The remote proxy speaks `Value → Value` and
/// forwards the daemon's JSON untouched; the embedded path converts its
/// typed result/error into the same `serde_json::Value` shape here, so both
/// modes hand the frontend identical JSON.
fn into_json<T: serde::Serialize>(
    result: Result<T, AppError>,
) -> Result<serde_json::Value, serde_json::Value> {
    match result {
        Ok(value) => Ok(serde_json::to_value(value).unwrap_or(serde_json::Value::Null)),
        Err(error) => Err(serde_json::to_value(error).unwrap_or(serde_json::Value::Null)),
    }
}

/// Remote-mode driver for the streaming commands (`dispatch_natural_command`,
/// `execute_playbook`, `run_task`): forwards each `clinch-progress` push to
/// the Tauri channel untouched, then returns the settled outcome.
///
/// The channel carries `serde_json::Value` rather than the typed event so the
/// daemon's progress JSON passes through byte-for-byte — no client-side
/// `Deserialize` of the event types needed. (The typed event is only produced
/// by the embedded path, which converts with `serde_json::to_value` at the
/// call site below.)
async fn remote_with_progress(
    daemon: &DaemonConfig,
    command: &str,
    params: serde_json::Value,
    progress: tauri::ipc::Channel<serde_json::Value>,
) -> Result<serde_json::Value, serde_json::Value> {
    let client = daemon.client().await?;
    let (mut pushes, outcome) = client.call_with_progress(command, params).await;
    while let Some(payload) = pushes.recv().await {
        let _ = progress.send(payload);
    }
    outcome
        .await
        .unwrap_or_else(|_| Err(remote_error("internal", "daemon call cancelled")))
}

/// Translate a daemon-side absolute Linux path to the Windows UNC path that
/// reaches the same file when the daemon runs under WSL2:
/// `/home/u/dl/f.pdf` → `\\wsl$\Ubuntu\home\u\dl\f.pdf`.
/// Returns `None` when the path is not absolute (nothing sane to map).
pub fn wsl_path_to_unc(linux_path: &str, distro: &str) -> Option<String> {
    if !linux_path.starts_with('/') {
        return None;
    }
    let windows_path = linux_path.replace('/', "\\");
    Some(format!("\\\\wsl$\\{distro}{windows_path}"))
}

#[tauri::command]
async fn downloaded_file_action<R: tauri::Runtime>(
    id: orchestration_engine::TaskId,
    index: usize,
    reveal: bool,
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        // `downloaded_file_action` is intentionally NOT proxied as a command:
        // the file lives on the daemon host, so resolve the daemon-side path
        // via the daemon-only `downloaded_file_path` helper and reveal/open
        // it with the local OS opener.
        let path_value = daemon
            .call(
                cmd::DOWNLOADED_FILE_PATH,
                serde_json::json!({"id": id, "index": index}),
            )
            .await?;
        let serde_json::Value::String(linux_path) = path_value else {
            return Err(remote_error(
                "storage_unavailable",
                "daemon returned a non-string file path",
            ));
        };
        let distro = std::env::var(clinch_protocol::WSL_DISTRO_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| clinch_protocol::DEFAULT_WSL_DISTRO.to_owned());
        let Some(local_path) = wsl_path_to_unc(&linux_path, &distro) else {
            return Err(remote_error(
                "storage_unavailable",
                "daemon file path is not translatable for local reveal",
            ));
        };
        if reveal {
            app.opener().reveal_item_in_dir(local_path).map_err(|_| {
                remote_error(
                    "storage_unavailable",
                    "could not reveal the downloaded file",
                )
            })?;
        } else {
            app.opener()
                .open_path(local_path, None::<&str>)
                .map_err(|_| {
                    remote_error("storage_unavailable", "could not open the downloaded file")
                })?;
        }
        return Ok(serde_json::Value::Null);
    }
    let outcome: Result<(), AppError> = async {
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
    .await;
    into_json(outcome)
}

#[tauri::command]
async fn task_decision(
    id: orchestration_engine::TaskId,
    index: usize,
    approved: bool,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(
                cmd::TASK_DECISION,
                serde_json::json!({"id": id, "index": index, "approved": approved}),
            )
            .await;
    }
    into_json(state.decide_task(id, index, approved).await)
}
#[tauri::command]
async fn dispatch_natural_command(
    prompt: String,
    progress: tauri::ipc::Channel<serde_json::Value>,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return remote_with_progress(
            &daemon,
            cmd::DISPATCH_NATURAL_COMMAND,
            serde_json::json!({"prompt": prompt}),
            progress,
        )
        .await;
    }
    into_json(
        state
            .dispatch_natural_command(prompt, |event| {
                if let Ok(payload) = serde_json::to_value(event) {
                    let _ = progress.send(payload);
                }
            })
            .await,
    )
}
#[tauri::command]
async fn browser_viewport(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(cmd::BROWSER_VIEWPORT, serde_json::json!({}))
            .await;
    }
    into_json(state.viewport().await)
}
#[tauri::command]
async fn acquire_browser_context<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        let client = daemon.client().await?;
        let status = client
            .call(cmd::ACQUIRE_BROWSER_CONTEXT, serde_json::json!({}))
            .await?;
        // The daemon pushes screencast frames and cursor positions for this
        // connection; forward them to the same Tauri events the embedded path
        // emits, with payloads as `serde_json::Value` so the bytes are
        // identical. The generation guard stops a stale forwarder from
        // emitting into a newer session after release + re-acquire.
        let generation = daemon.bump_context_generation();
        let counter = daemon.context_generation();
        let mut pushes = client.subscribe_pushes();
        tauri::async_runtime::spawn(async move {
            while counter.load(Ordering::SeqCst) == generation {
                match pushes.recv().await {
                    Ok(push) if counter.load(Ordering::SeqCst) == generation => {
                        let name = match push.event.as_str() {
                            event::SCREENCAST_FRAME => Some(event::SCREENCAST_FRAME),
                            event::CURSOR_MOVED => Some(event::CURSOR_MOVED),
                            _ => None,
                        };
                        if let Some(name) = name {
                            let _ = app.emit(name, push.payload);
                        }
                    }
                    // Released while a push was in flight (drop it), or the
                    // client went away: stop the forwarder either way.
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    // A lagging forwarder skips missed frames rather than
                    // stalling the read loop.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                }
            }
        });
        return Ok(status);
    }
    let outcome = async {
        // Frames stream over `browser-screencast-frame` until release, takeover
        // re-acquire, or app exit; cursor positions ride `browser-cursor-moved`
        // alongside so the UI can render the agent's pointer. A closed listener
        // simply ends the pump.
        let cursor_app = app.clone();
        state
            .acquire_context(
                move |frame| {
                    let _ = app.emit(service::SCREENCAST_EVENT, frame);
                },
                move |cursor| {
                    let _ = cursor_app.emit(service::CURSOR_EVENT, cursor);
                },
            )
            .await
    }
    .await;
    into_json(outcome)
}
#[tauri::command]
async fn release_browser_context(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        let outcome = daemon
            .call(cmd::RELEASE_BROWSER_CONTEXT, serde_json::json!({}))
            .await;
        // Stop the context forwarder even if the daemon call failed: the UI
        // has released, so no more frames may be emitted.
        daemon.bump_context_generation();
        return outcome;
    }
    into_json(state.release_context().await)
}
#[tauri::command]
async fn take_control(
    state: tauri::State<'_, AppService>,
    url: Option<String>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    // Remote note: proxied as-is. The interactive takeover needs a display on
    // the daemon host, so in remote mode this only works when the daemon runs
    // somewhere with a screen.
    if daemon.remote() {
        return daemon
            .call(cmd::TAKE_CONTROL, serde_json::json!({"url": url}))
            .await;
    }
    into_json(state.take_control(url).await)
}
// Tauri's CommandArg contract requires the State wrapper by value.
#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
async fn browser_context_status(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(cmd::BROWSER_CONTEXT_STATUS, serde_json::json!({}))
            .await;
    }
    into_json(state.context_status())
}
#[tauri::command]
async fn initialize(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon.call(cmd::INITIALIZE, serde_json::json!({})).await;
    }
    into_json(state.initialize().await)
}
#[tauri::command]
async fn sync_session(
    request: session_sync::SyncRequest,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(cmd::SYNC_SESSION, serde_json::json!({"request": request}))
            .await;
    }
    into_json(state.sync(request).await)
}
#[tauri::command]
async fn manual_login(
    portal_url: String,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(
                cmd::MANUAL_LOGIN,
                serde_json::json!({"portal_url": portal_url}),
            )
            .await;
    }
    into_json(state.manual_login(&portal_url).await)
}
#[tauri::command]
async fn close_browser(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon.call(cmd::CLOSE_BROWSER, serde_json::json!({})).await;
    }
    into_json(state.close_browser().await)
}
#[tauri::command]
async fn bridge_status(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon.call(cmd::BRIDGE_STATUS, serde_json::json!({})).await;
    }
    into_json(state.bridge_status())
}
#[tauri::command]
async fn bridge_sync_session(
    portal_url: String,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(
                cmd::BRIDGE_SYNC_SESSION,
                serde_json::json!({"portal_url": portal_url}),
            )
            .await;
    }
    into_json(state.bridge_sync(&portal_url).await)
}
/// Consent-gated session lending: the challenge-card or auth-sync-card
/// "Sync my session" tap. The tap is the consent event; the backend
/// resolves the page URL from the run registry, never from client input.
/// The synced session persists to Clinch's app profile ("sync once, stay
/// logged in"); the daily browser is never written to.
#[tauri::command]
async fn lend_session(
    request: LendRequest,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(cmd::LEND_SESSION, serde_json::json!({"request": request}))
            .await;
    }
    into_json(state.lend_session(request).await)
}
/// Revoke a persisted session: delete every cookie the app profile holds
/// for `host`. The "Forget this site" control behind a synced badge.
#[tauri::command]
async fn forget_site_session(
    host: String,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(cmd::FORGET_SITE_SESSION, serde_json::json!({"host": host}))
            .await;
    }
    into_json(state.forget_site_session(host).await)
}
#[tauri::command]
async fn auth_status(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon.call(cmd::AUTH_STATUS, serde_json::json!({})).await;
    }
    into_json(state.auth_status())
}
#[tauri::command]
async fn begin_embedded_auth(
    portal_url: String,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(
                cmd::BEGIN_EMBEDDED_AUTH,
                serde_json::json!({"portal_url": portal_url}),
            )
            .await;
    }
    into_json(state.begin_embedded_auth(&portal_url).await)
}
#[tauri::command]
async fn complete_embedded_auth(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(cmd::COMPLETE_EMBEDDED_AUTH, serde_json::json!({}))
            .await;
    }
    into_json(state.complete_embedded_auth().await)
}
#[tauri::command]
async fn cancel_embedded_auth(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(cmd::CANCEL_EMBEDDED_AUTH, serde_json::json!({}))
            .await;
    }
    into_json(state.cancel_embedded_auth().await)
}
#[tauri::command]
async fn picker_enable(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon.call(cmd::PICKER_ENABLE, serde_json::json!({})).await;
    }
    into_json(state.picker_enable().await)
}
#[tauri::command]
async fn picker_status(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon.call(cmd::PICKER_STATUS, serde_json::json!({})).await;
    }
    into_json(state.picker_status())
}
#[tauri::command]
async fn picker_pick(
    timeout_ms: Option<u64>,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(
                cmd::PICKER_PICK,
                serde_json::json!({"timeout_ms": timeout_ms}),
            )
            .await;
    }
    into_json(state.picker_pick(timeout_ms.unwrap_or(60_000)).await)
}
#[tauri::command]
async fn picker_disable(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(cmd::PICKER_DISABLE, serde_json::json!({}))
            .await;
    }
    into_json(state.picker_disable().await)
}
#[tauri::command]
async fn preview_intent(
    role: String,
    label: String,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(
                cmd::PREVIEW_INTENT,
                serde_json::json!({"role": role, "label": label}),
            )
            .await;
    }
    into_json(state.preview_intent(role, label).await)
}
#[tauri::command]
async fn save_playbook(
    name: String,
    portal_url: String,
    steps: Vec<playbook_store::Step>,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(
                cmd::SAVE_PLAYBOOK,
                serde_json::json!({"name": name, "portal_url": portal_url, "steps": steps}),
            )
            .await;
    }
    into_json(state.save_playbook(name, portal_url, steps).await)
}
#[tauri::command]
async fn save_run_as_workflow(
    run_id: String,
    name: String,
    description: Option<String>,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(
                cmd::SAVE_RUN_AS_WORKFLOW,
                serde_json::json!({"run_id": run_id, "name": name, "description": description}),
            )
            .await;
    }
    into_json(state.save_run_as_workflow(run_id, name, description).await)
}
#[tauri::command]
async fn list_playbooks(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(cmd::LIST_PLAYBOOKS, serde_json::json!({}))
            .await;
    }
    into_json(state.list_playbooks().await)
}
#[tauri::command]
async fn save_site_shortcut(
    name: String,
    url: String,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(
                cmd::SAVE_SITE_SHORTCUT,
                serde_json::json!({"name": name, "url": url}),
            )
            .await;
    }
    into_json(state.save_site_shortcut(name, url).await)
}
#[tauri::command]
async fn list_site_shortcuts(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(cmd::LIST_SITE_SHORTCUTS, serde_json::json!({}))
            .await;
    }
    into_json(state.list_site_shortcuts().await)
}
#[tauri::command]
async fn delete_site_shortcut(
    name: String,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(cmd::DELETE_SITE_SHORTCUT, serde_json::json!({"name": name}))
            .await;
    }
    into_json(state.delete_site_shortcut(name).await)
}
#[tauri::command]
async fn execute_playbook(
    id: String,
    progress: tauri::ipc::Channel<serde_json::Value>,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return remote_with_progress(
            &daemon,
            cmd::EXECUTE_PLAYBOOK,
            serde_json::json!({"id": id}),
            progress,
        )
        .await;
    }
    into_json(
        state
            .execute_playbook(id, |event| {
                // If the view closes, the run still completes; the terminal
                // outcome return value carries its final state.
                if let Ok(payload) = serde_json::to_value(event) {
                    let _ = progress.send(payload);
                }
            })
            .await,
    )
}
#[tauri::command]
// Tauri's CommandArg contract requires the State wrapper by value.
#[allow(clippy::needless_pass_by_value)]
async fn decide_playbook(
    run_id: u64,
    index: usize,
    approved: bool,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(
                cmd::DECIDE_PLAYBOOK,
                serde_json::json!({"run_id": run_id, "index": index, "approved": approved}),
            )
            .await;
    }
    into_json(state.decide_playbook(run_id, index, approved))
}
#[tauri::command]
async fn get_poc_metrics(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(cmd::GET_POC_METRICS, serde_json::json!({}))
            .await;
    }
    into_json(state.poc_metrics().await)
}
#[tauri::command]
async fn run_task(
    request: orchestration_engine::TaskRequest,
    progress: tauri::ipc::Channel<serde_json::Value>,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return remote_with_progress(
            &daemon,
            cmd::RUN_TASK,
            serde_json::json!({"request": request}),
            progress,
        )
        .await;
    }
    into_json(
        state
            .run_task(&request, |event| {
                // If the view closes, execution still checkpoints; get_task restores its durable result.
                if let Ok(payload) = serde_json::to_value(event) {
                    let _ = progress.send(payload);
                }
            })
            .await,
    )
}
#[tauri::command]
async fn get_task(
    id: orchestration_engine::TaskId,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(cmd::GET_TASK, serde_json::json!({"id": id}))
            .await;
    }
    into_json(state.task(id).await)
}
#[tauri::command]
// Tauri's CommandArg contract requires the State wrapper by value.
#[allow(clippy::needless_pass_by_value)]
async fn preview_approval(
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(cmd::PREVIEW_APPROVAL, serde_json::json!({}))
            .await;
    }
    into_json(state.preview_approval())
}
#[tauri::command]
// Tauri's CommandArg contract requires the State wrapper by value.
#[allow(clippy::needless_pass_by_value)]
async fn resolve_approval(
    id: u64,
    approved: bool,
    state: tauri::State<'_, AppService>,
    daemon: tauri::State<'_, DaemonConfig>,
) -> Result<serde_json::Value, serde_json::Value> {
    if daemon.remote() {
        return daemon
            .call(
                cmd::RESOLVE_APPROVAL,
                serde_json::json!({"id": id, "approved": approved}),
            )
            .await;
    }
    into_json(state.resolve_approval(id, approved))
}

pub fn with_commands<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder.invoke_handler(tauri::generate_handler![
        initialize,
        downloaded_file_action,
        task_decision,
        browser_viewport,
        acquire_browser_context,
        release_browser_context,
        take_control,
        browser_context_status,
        sync_session,
        manual_login,
        close_browser,
        bridge_status,
        bridge_sync_session,
        lend_session,
        forget_site_session,
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
        save_run_as_workflow,
        list_playbooks,
        save_site_shortcut,
        list_site_shortcuts,
        delete_site_shortcut,
        execute_playbook,
        decide_playbook,
        dispatch_natural_command,
        get_poc_metrics,
        run_task,
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
            // Managed in both modes: remote commands never touch it, and
            // constructing it is cheap (no browser launches until used).
            app.manage(AppService::new(data, home));
            let daemon = DaemonConfig::from_env();
            if daemon.remote() {
                // The daemon runs the engine AND the companion bridge
                // (127.0.0.1:9223) on its own host; starting a second local
                // bridge here would confuse the Companion extension, so the
                // local `ensure_bridge` spawn is skipped in remote mode.
                eprintln!(
                    "[clinch] remote mode: proxying engine commands to {}",
                    daemon.url().unwrap_or("?")
                );
            } else {
                eprintln!("[clinch] embedded mode: running the local engine");
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
            }
            app.manage(daemon);
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
