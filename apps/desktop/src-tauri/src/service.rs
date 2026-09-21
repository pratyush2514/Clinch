#![deny(unsafe_code)]
use crate::auth::{AuthPanel, ReauthReason, reason_for_signal};
use browser_driver::{Action, LaunchOptions, ManagedBrowser};
use orchestration_engine::{Engine, EngineError, Task, TaskEvent, TaskId, TaskRequest};
use serde::Serialize;
use session_sync::{FallbackReason, PreparedSync, SyncRequest};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{OnceCell, Semaphore, oneshot};

#[derive(Debug, Serialize)]
#[serde(tag = "code", content = "message", rename_all = "snake_case")]
pub enum AppError {
    InvalidInput(&'static str),
    Busy,
    StorageUnavailable,
    BrowserUnavailable,
    Internal,
    StaleApproval,
    SessionRequired,
    WorkflowFailed,
    /// Extension bridge reports a logged-out session for the target portal.
    /// Carries the full human-readable message so the UI can surface it.
    AuthenticationRequired(String),
    /// Picking needs the visible managed window: replays run headless, so an
    /// overlay armed there can never receive a click.
    PickerUnavailable,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStatus {
    ready: bool,
    cookie_import_supported: bool,
    /// Source browser preselected in the UI. Windows defaults to Brave:
    /// Chrome 127+ seals its cookie key with App-Bound encryption (only
    /// Chrome's own elevation service can unwrap it), while Brave keeps a
    /// plain user-DPAPI key that imports instantly.
    default_browser: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionStatus {
    CookiesImported { count: usize },
    ManualLogin { reason: Option<FallbackReason> },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalPreview {
    id: u64,
    title: &'static str,
    description: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStatus {
    running: bool,
    port: u16,
    extensions: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PickerStatus {
    /// True only with a headed (visible-window) browser connected. Headless
    /// replay targets and absent browsers both report false.
    ready: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntentPreview {
    role: String,
    name: String,
    description: String,
    backend_node_id: i64,
    score: u8,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybookApproval {
    run_id: u64,
    step_index: usize,
    kind: &'static str,
    summary: String,
    /// Itemized batch candidates for the gate card. Empty on single-step
    /// approvals. In-memory and IPC-bound to the local desktop UI only —
    /// never written to stdout, log files, or telemetry streams.
    candidates: Vec<CandidatePreview>,
}

/// One queued control for the Sentinel Gate card: position, visible label,
/// role, whether it lives inside page chrome, and a text fingerprint of
/// its surroundings (`None` when it carries no container text). Roles and
/// landmark flags come straight from the AX snapshot; there are no DOM
/// tags anywhere in this pipeline, so `container` is words, not markup.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CandidatePreview {
    pub index: usize,
    pub label: String,
    pub role: String,
    pub is_landmark: bool,
    pub container: Option<String>,
}

/// Gate-card content: what the approval asks plus itemized candidates.
/// Bundled so the approval helper stays within the argument budget.
struct ApprovalContent {
    kind: &'static str,
    summary: String,
    candidates: Vec<CandidatePreview>,
}

/// Per-snippet budget for container fingerprints: enough to recognize a
/// row, short enough to keep gate payloads small.
const MAX_PREVIEW_CONTAINER_LEN: usize = 120;

/// Shape resolved batch candidates into gate-card previews in document
/// order. Pure mapping — the live approval path below only forwards it.
fn candidate_previews(candidates: &[browser_driver::AxElement]) -> Vec<CandidatePreview> {
    candidates
        .iter()
        .enumerate()
        .map(|(index, element)| {
            let fingerprint: String = element
                .container_text
                .join(" ")
                .chars()
                .take(MAX_PREVIEW_CONTAINER_LEN)
                .collect();
            CandidatePreview {
                index,
                label: element.name.clone(),
                role: element.role.clone(),
                is_landmark: element.landmark.is_some(),
                container: (!fingerprint.trim().is_empty()).then_some(fingerprint),
            }
        })
        .collect()
}

/// Progress plus approval requests for one playbook run, streamed over a
/// single Tauri channel. The UI renders an inline approval card whenever
/// `approval` is present; `phase` keeps tracking the run underneath.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybookEvent {
    run_id: u64,
    step_index: usize,
    total_steps: usize,
    phase: orchestration_engine::SequencePhase,
    highlight: Option<browser_driver::Highlight>,
    approval: Option<PlaybookApproval>,
}

struct PendingPlaybookGate {
    run_id: u64,
    step_index: usize,
    reply: oneshot::Sender<bool>,
}

/// What a run journal entry describes: stored-playbook runs carry their row
/// id; ephemeral NL runs carry none.
#[derive(Clone, Debug)]
struct RunScope {
    kind: playbook_store::RunKind,
    playbook_id: Option<String>,
}

/// Which execution lane a resolved command takes. Plural ephemeral intents
/// batch across every matching control; everything else keeps its existing
/// single-step path. Pure routing so the browserless suite can prove the
/// plural prompt never truncates to one click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DispatchLane {
    Saved,
    Single,
    Batch,
}

/// Route one resolved command: the plural flag alone selects the batch
/// lane — saved single-step replays never reach it because the resolver
/// already forces plural prompts ephemeral.
fn dispatch_lane(matched: &orchestration_engine::CommandMatch) -> DispatchLane {
    match matched {
        orchestration_engine::CommandMatch::Saved { .. } => DispatchLane::Saved,
        orchestration_engine::CommandMatch::Ephemeral { intent } if intent.is_plural => {
            DispatchLane::Batch
        }
        orchestration_engine::CommandMatch::Ephemeral { .. } => DispatchLane::Single,
    }
}

/// What a natural-language command ran: which workflow (stored or
/// single-step ephemeral) plus its terminal sequence outcome.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchOutcome {
    kind: &'static str,
    name: String,
    result: orchestration_engine::SequenceOutcome,
    /// Steps that actually ran, so ad-hoc outcomes can be saved as
    /// playbooks without reconstructing them client-side. Additive to the
    /// IPC shape: older clients ignore unknown keys.
    steps: Vec<playbook_store::Step>,
    /// Route telemetry for the Session Activity UI: the exact
    /// `route_proposed:…` / `route_resolution_miss:…` line recorded to
    /// `session_events`, so the command bar can render it immediately
    /// without polling. `None` for stored replays (no proposal attempted).
    /// Additive: older clients ignore unknown keys.
    route_log: Option<String>,
    /// Snapshot telemetry for the Session Activity UI: the exact
    /// `ax_snapshot_telemetry:…` line from the last candidate snapshot, so
    /// node counts render without polling. `None` when no snapshot ran
    /// (stored replays, single-step ephemerals snapshotting inside the
    /// macro engine). Additive: older clients ignore unknown keys.
    telemetry_log: Option<String>,
}

/// POC health metrics for local testing: playbook runs, macro-replay share
/// derived from task checkpoints, and session-sync outcomes. Everything is
/// computed from local tables; missing tables (a flow that never ran) read
/// as zero rather than failing the command.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PocMetrics {
    total_runs: i64,
    runs_by_status: BTreeMap<String, i64>,
    completed_tasks: i64,
    macro_replay_pct: Option<f64>,
    sync_imported: i64,
    sync_fallback: i64,
    sync_by_outcome: BTreeMap<String, i64>,
}

pub struct AppService {
    data: PathBuf,
    home: PathBuf,
    db: OnceCell<sqlx::SqlitePool>,
    browser: Mutex<Option<Arc<ManagedBrowser>>>,
    operation: Semaphore,
    approval: Mutex<Option<u64>>,
    next_approval: AtomicU64,
    engine: OnceCell<Engine>,
    session_origin: Mutex<Option<url::Url>>,
    /// Pending embedded auth panel. `None` means no panel is raised.
    /// Holds the portal plus why it was raised; cleared on completion/cancel.
    auth_pending: Mutex<Option<(url::Url, ReauthReason)>>,
    /// Lazily started companion-bridge listener plus its accept-loop task.
    /// The task handle is retained (never detached) for the app lifetime.
    bridge: Mutex<Option<Arc<crate::ws_server::BridgeServer>>>,
    bridge_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Pending playbook-run approval. Single-flight like the Sentinel gate:
    /// stale and duplicate decisions fail closed.
    playbook_gate: Mutex<Option<PendingPlaybookGate>>,
    next_playbook_run: AtomicU64,
}

impl AppService {
    pub fn new(data: PathBuf, home: PathBuf) -> Self {
        Self {
            data,
            home,
            db: OnceCell::new(),
            browser: Mutex::new(None),
            operation: Semaphore::new(1),
            approval: Mutex::new(None),
            next_approval: AtomicU64::new(1),
            engine: OnceCell::new(),
            session_origin: Mutex::new(None),
            auth_pending: Mutex::new(None),
            bridge: Mutex::new(None),
            bridge_task: Mutex::new(None),
            playbook_gate: Mutex::new(None),
            next_playbook_run: AtomicU64::new(1),
        }
    }

    async fn database(&self) -> Result<&sqlx::SqlitePool, AppError> {
        self.db
            .get_or_try_init(|| async {
                playbook_store::initialize(&self.data.join("clinch.db"))
                    .await
                    .map_err(|_| AppError::StorageUnavailable)
            })
            .await
    }

    /// Test-only session plumbing: connect `portal` without launching a
    /// browser (mirrors the `session_origin` a manual login would leave).
    #[cfg(test)]
    pub(crate) fn test_connect(&self, portal: url::Url) -> Result<(), AppError> {
        *self.session_origin.lock().map_err(|_| AppError::Internal)? = Some(portal);
        Ok(())
    }

    /// Test-only read of the Session Activity backing store.
    #[cfg(test)]
    pub(crate) async fn test_session_events(&self) -> Result<Vec<String>, AppError> {
        let pool = self.database().await?;
        let rows: Vec<(String,)> = sqlx::query_as("SELECT outcome FROM session_events")
            .fetch_all(pool)
            .await
            .map_err(|_| AppError::StorageUnavailable)?;
        Ok(rows.into_iter().map(|(outcome,)| outcome).collect())
    }

    pub async fn initialize(&self) -> Result<StorageStatus, AppError> {
        self.engine().await?;
        Ok(StorageStatus {
            ready: true,
            // macOS Keychain and Windows DPAPI paths are both implemented;
            // per-browser failures (e.g. Chrome 127+ App-Bound keys) still
            // fall back to manual login with an explicit reason.
            cookie_import_supported: cfg!(any(target_os = "macos", target_os = "windows")),
            default_browser: if cfg!(target_os = "windows") {
                "brave"
            } else {
                "chrome"
            },
        })
    }

    async fn engine(&self) -> Result<&Engine, AppError> {
        self.engine
            .get_or_try_init(|| async {
                let engine = Engine::new(self.database().await?.clone())
                    .await
                    .map_err(|error| engine_error(&error))?;
                engine
                    .recover()
                    .await
                    .map_err(|error| engine_error(&error))?;
                Ok(engine)
            })
            .await
    }

    pub async fn run_task(
        &self,
        request: &TaskRequest,
        emit: impl FnMut(TaskEvent) + Send,
    ) -> Result<Task, AppError> {
        session_sync::validate_portal(request.portal_url.as_str())
            .map_err(|_| AppError::InvalidInput("Enter a valid HTTPS portal page URL."))?;
        if request.portal_url.query().is_some() || request.portal_url.fragment().is_some() {
            return Err(AppError::InvalidInput(
                "Use a stable portal page URL without a query or fragment.",
            ));
        }
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        let connected = self
            .session_origin
            .lock()
            .map_err(|_| AppError::Internal)?
            .clone();
        if connected.is_none_or(|url| url.origin() != request.portal_url.origin()) {
            return Err(AppError::SessionRequired);
        }
        let mode = Engine::run_mode(request, &self.data)
            .await
            .map_err(|error| engine_error(&error))?;
        let browser = self
            .browser(mode == orchestration_engine::RunMode::Replay)
            .await?;
        self.engine()
            .await?
            .run_task(request, &browser, &self.data, emit)
            .await
            .map_err(|error| engine_error(&error))
    }

    pub async fn decide_task(
        &self,
        id: TaskId,
        index: usize,
        approved: bool,
    ) -> Result<(), AppError> {
        self.engine()
            .await?
            .decide(id, index, approved)
            .map_err(|_| AppError::StaleApproval)
    }

    pub async fn viewport(&self) -> Result<browser_driver::Viewport, AppError> {
        let browser = self
            .browser
            .lock()
            .map_err(|_| AppError::Internal)?
            .clone()
            .ok_or(AppError::SessionRequired)?;
        browser
            .viewport()
            .await
            .map_err(|_| AppError::BrowserUnavailable)
    }

    pub async fn task(&self, id: TaskId) -> Result<Task, AppError> {
        self.engine()
            .await?
            .load(id)
            .await
            .map_err(|error| engine_error(&error))
    }

    pub async fn downloaded_file(&self, id: TaskId, index: usize) -> Result<PathBuf, AppError> {
        let task = self.task(id).await?;
        if task.state != orchestration_engine::TaskState::Completed {
            return Err(AppError::WorkflowFailed);
        }
        let file = task
            .plan
            .steps
            .iter()
            .flat_map(|step| &step.output.files)
            .nth(index)
            .ok_or(AppError::InvalidInput("Unknown downloaded file."))?;
        let root = tokio::fs::canonicalize(self.data.join("downloads").join(id.0.to_string()))
            .await
            .map_err(|_| AppError::StorageUnavailable)?;
        let path = tokio::fs::canonicalize(&file.path)
            .await
            .map_err(|_| AppError::StorageUnavailable)?;
        if !path.starts_with(root)
            || !tokio::fs::metadata(&path)
                .await
                .map_err(|_| AppError::StorageUnavailable)?
                .is_file()
        {
            return Err(AppError::InvalidInput("Invalid downloaded file."));
        }
        Ok(path)
    }

    async fn browser(&self, headless: bool) -> Result<Arc<ManagedBrowser>, AppError> {
        let existing = self.browser.lock().map_err(|_| AppError::Internal)?.clone();
        let executable =
            std::env::var_os("CLINCH_CHROMIUM_PATH").map_or_else(default_chromium, PathBuf::from);
        let profile = self.data.join("browser-profile");
        let options = LaunchOptions { headless };
        let browser = if let Some(browser) = existing {
            if browser.is_headless() == headless {
                return Ok(browser);
            }
            let restarted = browser.restart(&executable, &profile, options).await;
            if let Ok(browser) = restarted {
                browser
            } else {
                *self.browser.lock().map_err(|_| AppError::Internal)? = None;
                *self.session_origin.lock().map_err(|_| AppError::Internal)? = None;
                return Err(AppError::BrowserUnavailable);
            }
        } else {
            ManagedBrowser::launch_with_options(&executable, &profile, options)
                .await
                .map_err(|_| AppError::BrowserUnavailable)?
        };
        let browser = Arc::new(browser);
        *self.browser.lock().map_err(|_| AppError::Internal)? = Some(browser.clone());
        Ok(browser)
    }

    async fn record(&self, outcome: &str) -> Result<(), AppError> {
        sqlx::query("INSERT INTO session_events (outcome) VALUES (?)")
            .bind(outcome)
            .execute(self.database().await?)
            .await
            .map_err(|_| AppError::StorageUnavailable)?;
        Ok(())
    }

    pub async fn sync(&self, request: SyncRequest) -> Result<SessionStatus, AppError> {
        let request = request.validate().map_err(|_| {
            AppError::InvalidInput(
                "Consent, a valid HTTPS portal, and a valid profile are required.",
            )
        })?;
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        self.database().await?;
        let prepared = session_sync::prepare(&request, &self.home).await;
        let browser = self.browser(false).await?;
        // Identity mirroring: replay the source browser's User-Agent so the
        // synced session does not arrive under a mismatched UA (a standard
        // anti-bot signal). Best-effort — an unknown UA keeps the native one.
        if let Some(user_agent) = session_sync::source_user_agent(request.browser()) {
            let _ = browser.mirror_user_agent(&user_agent).await;
        }
        // Headless-first note: session establishment itself stays interactive
        // (the user may need to see login/2FA), but every macro replay runs
        // headless via `run_task()` → `browser(mode == Replay)`.
        let injected = match prepared {
            PreparedSync::Cookies(cookies) => {
                if browser.inject(&cookies).await.is_ok() {
                    self.record("cookies_imported_unverified").await?;
                    Some(cookies.len())
                } else {
                    self.record("manual_login_injection_failed").await?;
                    None
                }
            }
            PreparedSync::ManualLogin(reason) => {
                self.record("manual_login_extraction_unavailable").await?;
                browser
                    .navigate(request.portal())
                    .await
                    .map_err(|_| AppError::BrowserUnavailable)?;
                *self.session_origin.lock().map_err(|_| AppError::Internal)? =
                    Some(request.portal().clone());
                return Ok(SessionStatus::ManualLogin {
                    reason: Some(reason),
                });
            }
        };
        // Best-effort LocalStorage hydration: seed the portal's prior session
        // keys before the first document loads. Never fails the sync — an
        // empty result skips the CDP call and the script only fills keys the
        // portal has not already set. Uses the same resolved profile as cookie
        // extraction, including the `Default` → `Profile 1` fallback.
        if let Some(host) = request.portal().host_str() {
            let user_data = session_sync::user_data_dir(request.browser(), &self.home);
            if let Ok(profile_dir) =
                session_sync::resolve_profile_dir(&user_data, request.profile_name()).await
            {
                let stored = session_sync::read_local_storage(&profile_dir, host).await;
                if !stored.is_empty() {
                    let _ = browser.hydrate_local_storage(&stored).await;
                }
            }
        }
        browser
            .navigate(request.portal())
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        // Zero-touch reconciliation is shared with the extension bridge
        // (`land_on_portal`): challenge raises the embedded panel, success
        // records the connected origin.
        if !self.land_on_portal(&browser, request.portal()).await? {
            return Ok(SessionStatus::ManualLogin {
                reason: Some(FallbackReason::ReauthRequired),
            });
        }
        match injected {
            Some(count) => Ok(SessionStatus::CookiesImported { count }),
            None => Ok(SessionStatus::ManualLogin {
                reason: Some(FallbackReason::InjectionFailed),
            }),
        }
    }

    /// Navigate to `portal` and reconcile the landing, shared by local-profile
    /// sync and the extension bridge. Sets `session_origin` and returns `true`
    /// on an authenticated landing; raises the embedded in-app panel and
    /// returns `false` on a login/2FA challenge.
    async fn land_on_portal(
        &self,
        browser: &ManagedBrowser,
        portal: &url::Url,
    ) -> Result<bool, AppError> {
        browser
            .navigate(portal)
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        let signal = browser
            .auth_signal(portal)
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        if let Some(reason) = reason_for_signal(&signal) {
            *self.auth_pending.lock().map_err(|_| AppError::Internal)? =
                Some((portal.clone(), reason));
            self.record("session_reauth_required").await?;
            return Ok(false);
        }
        *self.session_origin.lock().map_err(|_| AppError::Internal)? = Some(portal.clone());
        Ok(true)
    }

    /// Companion-bridge listener (loopback only). Started eagerly by the Tauri
    /// setup hook and lazily by `bridge_sync` as a backstop; stays for the session.
    async fn bridge_server(&self) -> Result<Arc<crate::ws_server::BridgeServer>, AppError> {
        self.bridge_server_on(crate::ws_server::BRIDGE_PORT).await
    }

    /// [`Self::bridge_server`] on an explicit port (ephemeral in tests).
    /// Race-safe: concurrent callers share the first bound listener instead
    /// of failing on `AddrInUse`.
    async fn bridge_server_on(
        &self,
        port: u16,
    ) -> Result<Arc<crate::ws_server::BridgeServer>, AppError> {
        if let Some(server) = self.bridge.lock().map_err(|_| AppError::Internal)?.clone() {
            return Ok(server);
        }
        if let Ok((server, task)) = crate::ws_server::BridgeServer::start(port).await {
            *self.bridge.lock().map_err(|_| AppError::Internal)? = Some(server.clone());
            *self.bridge_task.lock().map_err(|_| AppError::Internal)? = Some(task);
            return Ok(server);
        }
        // Lost a startup race — reuse the winner's listener.
        if let Some(server) = self.bridge.lock().map_err(|_| AppError::Internal)?.clone() {
            return Ok(server);
        }
        Err(AppError::InvalidInput(
            "The companion bridge could not start. Is port 9223 already in use?",
        ))
    }

    /// Start the companion-bridge listener eagerly (Tauri setup hook) and
    /// report the bound port. Idempotent: concurrent calls share one listener.
    pub async fn ensure_bridge(&self) -> Result<u16, AppError> {
        Ok(self
            .bridge_server()
            .await?
            .local_port()
            .unwrap_or(crate::ws_server::BRIDGE_PORT))
    }

    /// Companion-bridge listener state. Never starts the listener as a side
    /// effect — the setup hook and `bridge_sync` own startup.
    pub fn bridge_status(&self) -> Result<BridgeStatus, AppError> {
        let guard = self.bridge.lock().map_err(|_| AppError::Internal)?;
        Ok(match guard.as_ref() {
            Some(server) => BridgeStatus {
                running: server.is_alive(),
                port: server.local_port().unwrap_or(crate::ws_server::BRIDGE_PORT),
                extensions: server.connection_count(),
            },
            None => BridgeStatus {
                running: false,
                port: crate::ws_server::BRIDGE_PORT,
                extensions: 0,
            },
        })
    }

    /// Zero-touch sync via the Clinch Companion extension: the live session
    /// (cookies + User-Agent) arrives over loopback, is validated against the
    /// portal scope, and is injected into the managed browser — raw values are
    /// never persisted. Long-polls up to the bridge response timeout.
    pub async fn bridge_sync(&self, portal_url: &str) -> Result<SessionStatus, AppError> {
        use crate::ws_server::BridgeError;
        let portal = session_sync::validate_portal(portal_url)
            .map_err(|_| AppError::InvalidInput("Enter a valid HTTPS portal URL."))?;
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        self.database().await?;
        // Request first: a missing companion fails fast without opening a window.
        let session = self
            .bridge_server()
            .await?
            .request_sync(&portal, crate::ws_server::RESPONSE_TIMEOUT)
            .await
            .map_err(|error| match error {
                BridgeError::NoExtension => AppError::InvalidInput(
                    "The Clinch Companion extension is not connected. Install it in your source browser and retry.",
                ),
                BridgeError::Timeout => AppError::InvalidInput(
                    "The companion extension did not answer in time. Retry the sync.",
                ),
                BridgeError::NoCookies => AppError::InvalidInput(
                    "The companion found no cookies for this portal. Sign in there first.",
                ),
                _ => AppError::WorkflowFailed,
            })?;
        let browser = self.browser(false).await?;
        // Identity first: the session must not arrive under a mismatched UA.
        // The UA was allowlist-validated by the bridge, so a CDP failure here
        // means the target is gone.
        browser
            .mirror_user_agent(&session.user_agent)
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        if browser.inject(&session.cookies).await.is_err() {
            self.record("bridge_injection_failed").await?;
            return Ok(SessionStatus::ManualLogin {
                reason: Some(FallbackReason::InjectionFailed),
            });
        }
        self.record("bridge_cookies_imported").await?;
        let count = session.cookies.len();
        if !self.land_on_portal(&browser, &portal).await? {
            return Ok(SessionStatus::ManualLogin {
                reason: Some(FallbackReason::ReauthRequired),
            });
        }
        Ok(SessionStatus::CookiesImported { count })
    }

    pub async fn manual_login(&self, portal_url: &str) -> Result<SessionStatus, AppError> {
        // Validation is shared with sync, but manual login never reads source cookies or Keychain.
        let portal = session_sync::validate_portal(portal_url)
            .map_err(|_| AppError::InvalidInput("Enter a valid HTTPS portal URL."))?;
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        let browser = self.browser(false).await?;
        browser
            .navigate(&portal)
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        self.record("manual_login_requested").await?;
        *self.session_origin.lock().map_err(|_| AppError::Internal)? = Some(portal);
        Ok(SessionStatus::ManualLogin { reason: None })
    }

    /// Current embedded auth-panel state, if raised. No browser I/O.
    pub fn auth_status(&self) -> Result<Option<AuthPanel>, AppError> {
        let pending = self.auth_pending.lock().map_err(|_| AppError::Internal)?;
        Ok(pending
            .as_ref()
            .map(|(portal, reason)| AuthPanel::new(portal, *reason)))
    }

    /// Raise the embedded in-app auth panel for `portal_url`.
    ///
    /// Opens the portal in the app-owned managed Chromium profile (never the
    /// user's external daily browser) and leaves the panel pending until
    /// [`Self::complete_embedded_auth`] verifies the origin.
    pub async fn begin_embedded_auth(&self, portal_url: &str) -> Result<AuthPanel, AppError> {
        let portal = session_sync::validate_portal(portal_url)
            .map_err(|_| AppError::InvalidInput("Enter a valid HTTPS portal URL."))?;
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        self.database().await?;
        let browser = self.browser(false).await?;
        browser
            .navigate(&portal)
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        let reason = browser
            .auth_signal(&portal)
            .await
            .map_err(|_| AppError::BrowserUnavailable)
            .ok()
            .and_then(|signal| reason_for_signal(&signal))
            .unwrap_or(ReauthReason::Manual);
        *self.auth_pending.lock().map_err(|_| AppError::Internal)? = Some((portal.clone(), reason));
        self.record("embedded_auth_opened").await?;
        Ok(AuthPanel::new(&portal, reason))
    }

    /// Verify the managed profile now lands on `portal` and close the panel.
    ///
    /// Only the origin is checked plus the login-marker classifier — page
    /// content is never exfiltrated. On success the WAL `session_events`
    /// row is appended (metadata only, never cookie values) and
    /// `session_origin` is set so `run_task()` can proceed.
    pub async fn complete_embedded_auth(&self) -> Result<SessionStatus, AppError> {
        let (portal, _reason) = self
            .auth_pending
            .lock()
            .map_err(|_| AppError::Internal)?
            .clone()
            .ok_or(AppError::SessionRequired)?;
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        let browser = {
            self.browser
                .lock()
                .map_err(|_| AppError::Internal)?
                .clone()
                .ok_or(AppError::SessionRequired)?
        };
        let signal = browser
            .auth_signal(&portal)
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        if reason_for_signal(&signal).is_some() {
            return Err(AppError::SessionRequired);
        }
        *self.auth_pending.lock().map_err(|_| AppError::Internal)? = None;
        *self.session_origin.lock().map_err(|_| AppError::Internal)? = Some(portal);
        self.record("embedded_auth_completed").await?;
        Ok(SessionStatus::ManualLogin { reason: None })
    }

    /// Dismiss the embedded panel without connecting the portal.
    pub async fn cancel_embedded_auth(&self) -> Result<(), AppError> {
        *self.auth_pending.lock().map_err(|_| AppError::Internal)? = None;
        self.record("embedded_auth_cancelled").await?;
        Ok(())
    }

    /// Whether picking can work right now: a headed browser must be
    /// connected. Never touches the browser; safe to poll before arming.
    pub fn picker_status(&self) -> Result<PickerStatus, AppError> {
        let ready = self
            .browser
            .lock()
            .map_err(|_| AppError::Internal)?
            .as_ref()
            .is_some_and(|browser| !browser.is_headless());
        Ok(PickerStatus { ready })
    }

    /// Arm the visual element picker overlay on the live target. Refuses
    /// headless targets outright: an invisible overlay can never be clicked,
    /// and silently arming one is exactly the stuck-`Picking…` trap.
    pub async fn picker_enable(&self) -> Result<(), AppError> {
        let browser = {
            self.browser
                .lock()
                .map_err(|_| AppError::Internal)?
                .clone()
                .ok_or(AppError::SessionRequired)?
        };
        if browser.is_headless() {
            return Err(AppError::PickerUnavailable);
        }
        browser
            .enable_picker()
            .await
            .map_err(|_| AppError::BrowserUnavailable)
    }

    /// Wait for the next picked element (bounded). Holds the single-operation
    /// permit so a macro run cannot race the picking session.
    pub async fn picker_pick(
        &self,
        timeout_ms: u64,
    ) -> Result<browser_driver::PickedElement, AppError> {
        let timeout_ms = timeout_ms.clamp(1_000, 120_000);
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        let browser = {
            self.browser
                .lock()
                .map_err(|_| AppError::Internal)?
                .clone()
                .ok_or(AppError::SessionRequired)?
        };
        let picked = browser
            .await_pick(std::time::Duration::from_millis(timeout_ms))
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        // Best-effort teardown; a failed teardown must not discard the pick.
        let _ = browser.disable_picker().await;
        Ok(picked)
    }

    /// Remove the picker overlay and restore outlines.
    pub async fn picker_disable(&self) -> Result<(), AppError> {
        let browser = {
            self.browser
                .lock()
                .map_err(|_| AppError::Internal)?
                .clone()
                .ok_or(AppError::SessionRequired)?
        };
        browser
            .disable_picker()
            .await
            .map_err(|_| AppError::BrowserUnavailable)
    }

    /// Resolve a semantic intent against the connected portal without acting.
    /// Read-only preview for the workflow builder: snapshots, matches, and
    /// reports — never clicks, so no approval gate is involved.
    pub async fn preview_intent(
        &self,
        role: String,
        label: String,
    ) -> Result<IntentPreview, AppError> {
        let intent = macro_engine::SemanticIntent {
            role,
            label_query: label,
            container_query: None,
            raw_prompt: String::new(),
            ordinal_index: None,
            is_last: false,
            is_plural: false,
            entry_url: None,
            primary_target_noun: None,
        };
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        let portal = self
            .session_origin
            .lock()
            .map_err(|_| AppError::Internal)?
            .clone()
            .ok_or(AppError::SessionRequired)?;
        let browser = self
            .browser
            .lock()
            .map_err(|_| AppError::Internal)?
            .clone()
            .ok_or(AppError::SessionRequired)?;
        let (elements, _, _) = browser.ax_snapshot(&portal).await;
        macro_engine::resolve_intent(&elements, &intent)
            .map(|resolved| IntentPreview {
                role: resolved.element.role,
                name: resolved.element.name,
                description: resolved.element.description,
                backend_node_id: resolved.element.backend_node_id,
                score: resolved.score,
            })
            .ok_or(AppError::InvalidInput(
                "No live control matches this intent on the connected portal.",
            ))
    }

    async fn playbooks(&self) -> Result<playbook_store::PlaybookStore, AppError> {
        Ok(playbook_store::PlaybookStore::new(
            self.database().await?.clone(),
        ))
    }

    /// Persist a validated playbook (insert or replace by name). Returns the
    /// row id as text for `execute_playbook`.
    pub async fn save_playbook(
        &self,
        name: String,
        portal_url: String,
        steps: Vec<playbook_store::Step>,
    ) -> Result<String, AppError> {
        let portal = session_sync::validate_portal(&portal_url)
            .map_err(|_| AppError::InvalidInput("Enter a valid HTTPS portal URL."))?;
        let playbook = playbook_store::Playbook::new(name, portal, steps)
            .map_err(|_| AppError::InvalidInput("Check the workflow name, portal, and steps."))?;
        self.playbooks()
            .await?
            .save_playbook(&playbook)
            .await
            .map_err(|error| match error {
                playbook_store::StoreError::Invalid(_) | playbook_store::StoreError::Json(_) => {
                    AppError::InvalidInput("Check the workflow name, portal, and steps.")
                }
                _ => AppError::StorageUnavailable,
            })
    }

    /// Newest-first stored-playbook summaries for the workflow list.
    pub async fn list_playbooks(&self) -> Result<Vec<playbook_store::PlaybookSummary>, AppError> {
        self.playbooks()
            .await?
            .list_playbooks()
            .await
            .map_err(|_| AppError::StorageUnavailable)
    }

    /// POC metrics over local tables. Tables for flows that never ran read
    /// as zero; only a broken pool fails the command.
    pub async fn poc_metrics(&self) -> Result<PocMetrics, AppError> {
        const IMPORTED: &[&str] = &["cookies_imported_unverified", "bridge_cookies_imported"];
        const FALLBACK: &[&str] = &[
            "manual_login_injection_failed",
            "manual_login_extraction_unavailable",
            "manual_login_requested",
            "embedded_auth_opened",
            "embedded_auth_completed",
            "embedded_auth_cancelled",
            "session_reauth_required",
            "bridge_injection_failed",
        ];
        let pool = self.database().await?;
        let run_rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT status, COUNT(*) FROM runs GROUP BY status")
                .fetch_all(pool)
                .await
                .map_err(|_| AppError::StorageUnavailable)?;
        let mut runs_by_status = BTreeMap::new();
        let mut total_runs = 0;
        for (status, count) in run_rows {
            total_runs += count;
            runs_by_status.insert(status, count);
        }
        // The tasks table only exists after the engine initializes; a flow
        // that never ran contributes zero completed tasks, not an error.
        let completed_tasks: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM tasks WHERE json_extract(snapshot,'$.state')='completed'",
        )
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        let replayed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM tasks WHERE json_extract(snapshot,'$.state')='completed' AND json_extract(snapshot,'$.mode')='replay'",
        )
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        let macro_replay_pct = if completed_tasks > 0 {
            // Counts are run volumes, far below the 2^53 exact-integer range
            // of `f64`; callers format for display.
            #[allow(clippy::cast_precision_loss)]
            Some(replayed as f64 / completed_tasks as f64 * 100.0)
        } else {
            None
        };
        let sync_rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT outcome, COUNT(*) FROM session_events GROUP BY outcome")
                .fetch_all(pool)
                .await
                .map_err(|_| AppError::StorageUnavailable)?;
        let mut sync_by_outcome = BTreeMap::new();
        for (outcome, count) in sync_rows {
            sync_by_outcome.insert(outcome, count);
        }
        let total = |keys: &[&str]| {
            keys.iter()
                .filter_map(|key| sync_by_outcome.get(*key))
                .sum()
        };
        Ok(PocMetrics {
            total_runs,
            runs_by_status,
            completed_tasks,
            macro_replay_pct,
            sync_imported: total(IMPORTED),
            sync_fallback: total(FALLBACK),
            sync_by_outcome,
        })
    }

    /// Resolve exactly one pending playbook-run decision. Stale and duplicate
    /// decisions fail closed, mirroring `Engine::decide`.
    pub fn decide_playbook(
        &self,
        run_id: u64,
        index: usize,
        approved: bool,
    ) -> Result<(), AppError> {
        let mut gate = self.playbook_gate.lock().map_err(|_| AppError::Internal)?;
        if gate
            .as_ref()
            .is_none_or(|pending| pending.run_id != run_id || pending.step_index != index)
        {
            return Err(AppError::StaleApproval);
        }
        gate.take()
            .ok_or(AppError::StaleApproval)?
            .reply
            .send(approved)
            .map_err(|_| AppError::StaleApproval)
    }

    /// Block on one local playbook-run approval, emitted over the run's
    /// progress channel with a five-minute deadline. Single-flight: an
    /// occupied gate, a poisoned lock, a timeout, or a dropped receiver all
    /// deny. Every decision is journaled to `session_events` (metadata only).
    async fn approve_playbook_step(
        &self,
        run_id: u64,
        step_index: usize,
        total_steps: usize,
        content: ApprovalContent,
        emit: &std::sync::Mutex<&mut (impl FnMut(PlaybookEvent) + Send)>,
    ) -> bool {
        let (reply, receive) = oneshot::channel();
        if let Ok(mut gate) = self.playbook_gate.lock() {
            if gate.is_some() {
                return false;
            }
            *gate = Some(PendingPlaybookGate {
                run_id,
                step_index,
                reply,
            });
        } else {
            return false;
        }
        if let Ok(mut emit) = emit.lock() {
            emit(PlaybookEvent {
                run_id,
                step_index,
                total_steps,
                phase: orchestration_engine::SequencePhase::Running,
                highlight: None,
                approval: Some(PlaybookApproval {
                    run_id,
                    step_index,
                    kind: content.kind,
                    summary: content.summary,
                    candidates: content.candidates,
                }),
            });
        }
        let approved = tokio::time::timeout(Duration::from_mins(5), receive)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(false);
        if let Ok(mut gate) = self.playbook_gate.lock() {
            *gate = None;
        }
        let outcome = if approved { "approved" } else { "rejected" };
        let _ = self
            .record(&format!(
                "playbook_decision:{run_id}:{step_index}:{outcome}"
            ))
            .await;
        approved
    }

    fn action_summary(action: &Action) -> String {
        match action {
            Action::Navigate { url } => format!("Open {}", url.as_str()),
            Action::Click { selector } => format!("Click {selector}"),
            Action::Fill { selector, .. } => format!("Fill {selector}"),
            Action::Submit { selector } => format!("Submit {selector}"),
            Action::DownloadLinks { selector } => format!("Download {selector}"),
        }
    }

    /// Execute a stored playbook step-by-step on the connected portal,
    /// streaming progress plus inline approvals over `emit`. Shares the
    /// connected-session contract and the single-operation semaphore with
    /// macro runs; semantic and legacy clicks gate exactly like first-run
    /// execution, so playbooks cannot bypass Sentinel approval.
    pub async fn execute_playbook(
        &self,
        id: String,
        emit: impl FnMut(PlaybookEvent) + Send,
    ) -> Result<orchestration_engine::SequenceOutcome, AppError> {
        let playbook =
            self.playbooks()
                .await?
                .load_playbook(&id)
                .await
                .map_err(|error| match error {
                    playbook_store::StoreError::NotFound => {
                        AppError::InvalidInput("Unknown playbook.")
                    }
                    _ => AppError::StorageUnavailable,
                })?;
        self.run_steps(
            playbook.origin.clone(),
            &playbook.steps,
            RunScope {
                kind: playbook_store::RunKind::Saved,
                playbook_id: Some(id),
            },
            emit,
        )
        .await
    }

    /// Route a free-form command to a saved playbook, a single-step
    /// ephemeral intent, or a plural batch across every matching control,
    /// and run it with the same approvals and streaming as stored playbooks.
    /// Plural prompts bypass saved single-step replays at resolution time,
    /// so they always land in the batch lane below instead of clicking once.
    pub async fn dispatch_natural_command(
        &self,
        prompt: String,
        emit: impl FnMut(PlaybookEvent) + Send,
    ) -> Result<DispatchOutcome, AppError> {
        if prompt.trim().is_empty() {
            return Err(AppError::InvalidInput(
                "Describe what to run, for example 'download the monthly site report'.",
            ));
        }
        let saved = self
            .playbooks()
            .await?
            .list_playbooks()
            .await
            .map_err(|_| AppError::StorageUnavailable)?;
        let connected = self
            .session_origin
            .lock()
            .map_err(|_| AppError::Internal)?
            .clone();
        match orchestration_engine::resolve_command(&prompt, connected.as_ref(), &saved) {
            None => Err(AppError::InvalidInput(
                "No saved workflow matches, and no portal is connected for an ad-hoc intent. Connect a portal or save a workflow first.",
            )),
            Some(matched) => match dispatch_lane(&matched) {
                DispatchLane::Saved => {
                    let orchestration_engine::CommandMatch::Saved { id } = matched else {
                        return Err(AppError::Internal);
                    };
                    self.dispatch_saved(id, emit).await
                }
                DispatchLane::Single => {
                    let portal = connected.ok_or(AppError::SessionRequired)?;
                    let orchestration_engine::CommandMatch::Ephemeral { intent } = matched else {
                        return Err(AppError::Internal);
                    };
                    self.dispatch_single_ephemeral(portal, prompt, intent, emit)
                        .await
                }
                DispatchLane::Batch => {
                    let portal = connected.ok_or(AppError::SessionRequired)?;
                    let orchestration_engine::CommandMatch::Ephemeral { intent } = matched else {
                        return Err(AppError::Internal);
                    };
                    self.dispatch_plural_batch(portal, prompt, intent, emit)
                        .await
                }
            },
        }
    }

    /// Run a stored playbook by row id with the shared run machinery.
    async fn dispatch_saved(
        &self,
        id: String,
        emit: impl FnMut(PlaybookEvent) + Send,
    ) -> Result<DispatchOutcome, AppError> {
        let playbook =
            self.playbooks()
                .await?
                .load_playbook(&id)
                .await
                .map_err(|error| match error {
                    playbook_store::StoreError::NotFound => {
                        AppError::InvalidInput("Unknown playbook.")
                    }
                    _ => AppError::StorageUnavailable,
                })?;
        let name = playbook.name.clone();
        let result = self
            .run_steps(
                playbook.origin.clone(),
                &playbook.steps,
                RunScope {
                    kind: playbook_store::RunKind::Saved,
                    playbook_id: Some(id),
                },
                emit,
            )
            .await?;
        Ok(DispatchOutcome {
            kind: "saved",
            name,
            result,
            steps: playbook.steps.clone(),
            route_log: None,
            telemetry_log: None,
        })
    }

    /// Cold-path entry resolution: tab-independent by design. Never inspects
    /// the active tab's URL, never vetoes or filters against it — `(portal,
    /// intent_class)` comes purely from prompt tokens plus the primary target
    /// noun, queried against the normalized `portal_route` table. Shared by
    /// the single and batch dispatch lanes. Records host plus path — never
    /// query strings — to `session_events` and returns the exact log line so
    /// dispatch outcomes can surface it in the Session Activity UI
    /// immediately. Returns `None` when an entry is already present (nothing
    /// proposed, nothing to surface).
    async fn propose_entry_url(
        &self,
        prompt: &str,
        intent: &mut macro_engine::SemanticIntent,
    ) -> Option<String> {
        if intent.entry_url.is_some() {
            return None;
        }
        // Production wires no account directory (no stored credential backs
        // one) and no LLM adapter: only the curated table tier can fire.
        let ctx = orchestration_engine::ResolutionContext {
            account_dir: None,
            llm: None,
        };
        // Try the label first, then the primary target noun when it differs:
        // both are prompt-derived topic words, so neither inspects tab state.
        let mut resolved =
            orchestration_engine::resolve_entry_url(prompt, &intent.label_query, &ctx);
        if resolved.is_none()
            && let Some(noun) = intent.primary_target_noun.as_deref()
            && !noun.eq_ignore_ascii_case(&intent.label_query)
        {
            resolved = orchestration_engine::resolve_entry_url(prompt, noun, &ctx);
        }
        if let Some(route) = resolved {
            let line = format!(
                "route_proposed:{}{} · source: {:?}",
                route.url.host_str().unwrap_or("?"),
                route.url.path(),
                route.source
            );
            let _ = self.record(&line).await;
            intent.entry_url = Some(route.url.as_str().to_owned());
            Some(line)
        } else {
            let line = format!("route_resolution_miss: prompt='{prompt}'");
            let _ = self.record(&line).await;
            Some(line)
        }
    }

    /// Cross-domain pre-navigation: when step 1 names an entry URL that
    /// differs from the live tab (origin/path), navigate there first. Reuses
    /// the macro engine's existing settle routine (`goto` awaits page load),
    /// so starting on `google.com` or `about:blank` lands on the route before
    /// any ARIA snapshot. No-op without a step-1 entry URL.
    async fn pre_navigate_to_entry(
        browser: &Arc<ManagedBrowser>,
        intent: &macro_engine::SemanticIntent,
    ) -> Result<bool, AppError> {
        let Some(entry) = intent.entry_url.as_deref() else {
            return Ok(false);
        };
        let entry_url = url::Url::parse(entry)
            .map_err(|_| AppError::InvalidInput("The derived intent is not runnable."))?;
        macro_engine::ensure_at_entry_url(browser, &entry_url)
            .await
            .map_err(|_| AppError::BrowserUnavailable)
    }

    /// Pre-navigation driven explicitly by step 1's entry URL: extracts the
    /// step-1 intent and reuses [`Self::pre_navigate_to_entry`], so dispatch
    /// lanes provably navigate from `step.entry_url` (identical to
    /// `intent.entry_url` by construction) before the first `ax_snapshot`.
    async fn pre_navigate_to_step(
        browser: &Arc<ManagedBrowser>,
        steps: &[playbook_store::Step],
    ) -> Result<bool, AppError> {
        let [playbook_store::Step::Semantic { intent }, ..] = steps else {
            return Ok(false);
        };
        Self::pre_navigate_to_entry(browser, intent).await
    }

    /// Bind portal confinement to step 1's entry origin after intentional
    /// navigation, so the drift guard evaluates the proposed route's host
    /// instead of the pre-navigation tab origin. Returns the journaled
    /// `portal_reanchored` line when the anchor actually changed; silent
    /// when already bound, entry-less, or unparsable.
    async fn reanchor_to_entry(
        &self,
        browser: &std::sync::Arc<browser_driver::ManagedBrowser>,
        steps: &[playbook_store::Step],
    ) -> Option<String> {
        let [playbook_store::Step::Semantic { intent }, ..] = steps else {
            return None;
        };
        let entry = intent.entry_url.as_deref()?;
        let entry_url = url::Url::parse(entry).ok()?;
        let previous = browser.reanchor_portal(&entry_url);
        let current = browser.portal_anchor()?;
        if previous.as_ref() == Some(&current) {
            return None;
        }
        Some(
            self.journal_line(browser_driver::portal_reanchored_line(
                previous.as_ref(),
                &current,
            ))
            .await,
        )
    }

    /// Post-navigation session check via the existing Extension Bridge.
    /// Fail-open when no companion is connected (nothing to report
    /// logged-out); fails closed only when the bridge answers `no_cookies`
    /// for the target portal — no candidate snapshot may run unauthenticated.
    async fn verify_bridge_auth(&self, portal: &url::Url) -> Result<(), AppError> {
        let server = self.bridge.lock().map_err(|_| AppError::Internal)?.clone();
        let Some(server) = server else {
            return Ok(());
        };
        if server.connection_count() == 0 {
            return Ok(());
        }
        match server
            .request_sync(portal, crate::ws_server::RESPONSE_TIMEOUT)
            .await
        {
            Err(crate::ws_server::BridgeError::NoCookies) => {
                let label = portal.host_str().unwrap_or("portal").to_owned();
                Err(AppError::AuthenticationRequired(format!(
                    "Authentication required: extension bridge reports logged-out status for {label}"
                )))
            }
            _ => Ok(()),
        }
    }

    /// Run one ad-hoc semantic intent as a single-step ephemeral playbook.
    async fn dispatch_single_ephemeral(
        &self,
        portal: url::Url,
        prompt: String,
        mut intent: macro_engine::SemanticIntent,
        emit: impl FnMut(PlaybookEvent) + Send,
    ) -> Result<DispatchOutcome, AppError> {
        // Ephemeral entry point (command-bar Run): propose the route at the
        // top, before any macro task step exists, so `intent.entry_url` and
        // step 1 carry the same proposed route into pre-navigation. The
        // returned log line travels on the outcome for immediate UI render.
        let route_log = self.propose_entry_url(&prompt, &mut intent).await;
        let name = orchestration_engine::ephemeral_name(&prompt);
        let playbook = playbook_store::Playbook::new(
            name.clone(),
            portal.clone(),
            vec![playbook_store::Step::Semantic { intent }],
        )
        .map_err(|_| AppError::InvalidInput("The derived intent is not runnable."))?;
        // Lazy browser attach when disconnected, then cross-domain
        // pre-navigation from step 1's entry (reusing the settle routine)
        // before any snapshot. Re-anchor confinement to the proposed entry
        // origin so snapshots evaluate the intentional destination, not the
        // starting tab. Unauthenticated bridge sessions halt here —
        // never snapshotted.
        let browser = self.browser(false).await?;
        Self::pre_navigate_to_step(&browser, &playbook.steps).await?;
        let telemetry_log = self.reanchor_to_entry(&browser, &playbook.steps).await;
        self.verify_bridge_auth(&portal).await?;
        let result = self
            .run_steps(
                portal,
                &playbook.steps,
                RunScope {
                    kind: playbook_store::RunKind::Ephemeral,
                    playbook_id: None,
                },
                emit,
            )
            .await?;
        Ok(DispatchOutcome {
            kind: "ephemeral",
            name,
            result,
            steps: playbook.steps.clone(),
            route_log,
            // Per-snapshot lines live inside the macro engine here, which
            // has no journal access — but the re-anchor line (if the anchor
            // changed) still surfaces above the outcome.
            telemetry_log,
        })
    }

    /// Run a plural intent across every matching control: snapshot once for
    /// a candidate count, take one batch approval naming that count, then
    /// execute the batch. Approval precedes every click (never after), the
    /// operation semaphore is shared with single runs, and journaling
    /// mirrors `run_steps` fail-open telemetry. No candidate match means no
    /// approval prompt — the run fails closed instead.
    async fn dispatch_plural_batch(
        &self,
        portal: url::Url,
        prompt: String,
        mut intent: macro_engine::SemanticIntent,
        mut emit: impl FnMut(PlaybookEvent) + Send,
    ) -> Result<DispatchOutcome, AppError> {
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        // Ephemeral entry point (command-bar Run): propose the route at the
        // top, before any macro task step exists. The log line travels on
        // every batch outcome so failures still surface the proposal.
        let route_log = self.propose_entry_url(&prompt, &mut intent).await;
        let (browser, run_id, journal_id, journal) = self.begin_batch_run(&portal).await?;
        let name = orchestration_engine::ephemeral_name(&prompt);
        // The proposed route lives on both `intent.entry_url` and step 1, so
        // pre-navigation provably runs from `step.entry_url`.
        let steps = vec![playbook_store::Step::Semantic {
            intent: intent.clone(),
        }];
        // Cross-domain pre-navigation when step 1's entry differs from the
        // live tab (origin/path): navigate first via the shared settle
        // routine, re-anchor confinement to the intentional destination,
        // then verify bridge auth before any ARIA snapshot.
        Self::pre_navigate_to_step(&browser, &steps).await?;
        self.verify_bridge_auth(&portal).await?;
        let events = std::sync::Mutex::new(&mut emit);
        // Read-only snapshot first: the approval names the real candidate
        // count, and empty fields fail closed before any gate is raised.
        // A single entry-URL retry covers wrong-page starts: navigate once,
        // re-snapshot once, then stop. No approval gate ever opens on zero
        // candidates.
        let (candidates, telemetry_log) = self
            .snapshot_anchored_batch_candidates(&browser, &portal, &intent, &steps)
            .await;
        // Every terminal outcome below carries the same steps plus the
        // route-proposal and snapshot-telemetry lines, so failures still
        // surface both in the UI.
        let outcome = |status, completed: usize, stopped: Option<usize>| {
            Self::batch_outcome(
                name.clone(),
                steps.clone(),
                status,
                completed,
                stopped,
                route_log.clone(),
                telemetry_log.clone(),
            )
        };
        let Some(candidates) = candidates else {
            Self::finish_batch_run(journal.as_ref(), &journal_id, "failed", 0).await;
            let _ = self.record("batch:denied:ZeroCandidatesFound").await;
            return Ok(outcome(
                orchestration_engine::SequenceStatus::Failed,
                0,
                Some(0),
            ));
        };
        let approved = self
            .approve_batch(run_id, &candidates, &intent, &prompt, &events)
            .await;
        if !approved {
            Self::finish_batch_run(journal.as_ref(), &journal_id, "denied", 0).await;
            let _ = self.record("batch:denied:UserRejected").await;
            Self::emit_batch_phase(
                &events,
                run_id,
                orchestration_engine::SequencePhase::Blocked,
            );
            return Ok(outcome(
                orchestration_engine::SequenceStatus::Denied,
                0,
                Some(0),
            ));
        }
        match macro_engine::execute_batch(&browser, &portal, &intent).await {
            Ok(macro_engine::ExecuteOutcome::Completed(_)) => {
                Self::finish_batch_run(journal.as_ref(), &journal_id, "completed", 1).await;
                Self::emit_batch_phase(
                    &events,
                    run_id,
                    orchestration_engine::SequencePhase::Completed,
                );
                Ok(outcome(
                    orchestration_engine::SequenceStatus::Completed,
                    1,
                    None,
                ))
            }
            Ok(macro_engine::ExecuteOutcome::HaltedEarly {
                clicks_completed,
                failed_candidate_index,
                ..
            }) => {
                // Partial progress is honest data: completed carries the
                // clicks that landed, stopped_at the drift point. The
                // diverged URL stays out of telemetry (URLs can carry
                // tokens); the recovery card reads it from a future
                // outcome field, not from here.
                Self::finish_batch_run(journal.as_ref(), &journal_id, "failed", clicks_completed)
                    .await;
                let _ = self.record("batch:halted:UrlDriftDetected").await;
                Self::emit_batch_phase(
                    &events,
                    run_id,
                    orchestration_engine::SequencePhase::Blocked,
                );
                Ok(outcome(
                    orchestration_engine::SequenceStatus::Failed,
                    clicks_completed,
                    Some(failed_candidate_index),
                ))
            }
            Err(_) => {
                Self::finish_batch_run(journal.as_ref(), &journal_id, "failed", 0).await;
                Self::emit_batch_phase(
                    &events,
                    run_id,
                    orchestration_engine::SequencePhase::Blocked,
                );
                Ok(outcome(
                    orchestration_engine::SequenceStatus::Failed,
                    0,
                    Some(0),
                ))
            }
        }
    }

    /// Batch lane setup: session contract, headed browser, run id, journal
    /// id, and fail-open journal start. Mirrors the `run_steps` preamble so
    /// both lanes hold the same contracts; the operation semaphore stays
    /// with the caller so it spans the whole run.
    async fn begin_batch_run(
        &self,
        portal: &url::Url,
    ) -> Result<
        (
            std::sync::Arc<browser_driver::ManagedBrowser>,
            u64,
            String,
            Option<playbook_store::PlaybookStore>,
        ),
        AppError,
    > {
        let connected = self
            .session_origin
            .lock()
            .map_err(|_| AppError::Internal)?
            .clone();
        if connected.is_none_or(|url| url.origin() != portal.origin()) {
            return Err(AppError::SessionRequired);
        }
        let browser = self.browser(false).await?;
        let run_id = self.next_playbook_run.fetch_add(1, Ordering::Relaxed);
        let journal_id = format!(
            "run-{run_id}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        );
        let journal = self.playbooks().await.ok();
        if let Some(store) = &journal {
            let _ = store
                .record_run_start(&journal_id, None, playbook_store::RunKind::Ephemeral, 1)
                .await;
        }
        Ok((browser, run_id, journal_id, journal))
    }

    /// Count snapshot nodes mentioning the target noun (case-insensitive
    /// substring over accessible name, description, and surroundings).
    /// Mirrors the batch noun gate so telemetry and selection agree; an
    /// empty noun matches nothing.
    fn count_noun_matches(elements: &[browser_driver::AxElement], noun: &str) -> usize {
        let stem = noun.trim().to_lowercase();
        if stem.is_empty() {
            return 0;
        }
        elements
            .iter()
            .filter(|element| {
                element.name.to_lowercase().contains(&stem)
                    || element.description.to_lowercase().contains(&stem)
                    || element
                        .container_text
                        .iter()
                        .any(|context| context.to_lowercase().contains(&stem))
            })
            .count()
    }

    /// Session-activity line for one candidate snapshot, e.g.
    /// `ax_snapshot_telemetry: total_nodes=142, target_noun_matches=3
    /// (noun='invoice')`. Counts carry no URLs, labels, or page text —
    /// only volumes plus the prompt-derived noun.
    fn snapshot_telemetry_line(total_nodes: usize, noun_matches: usize, noun: &str) -> String {
        format!(
            "ax_snapshot_telemetry: total_nodes={total_nodes}, target_noun_matches={noun_matches} (noun='{noun}')"
        )
    }

    /// Combine one run's journaled diagnostic lines into the multi-line
    /// `telemetryLog` surfaced in Session Activity: resync counter, resync
    /// check, resync action(s), then snapshot stats — in journal order.
    /// `None` when nothing was journaled, so paths that never snapshot
    /// stay silent instead of emitting an empty block.
    fn combine_telemetry(lines: &[String]) -> Option<String> {
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }

    /// Journal one diagnostic line fail-open and hand it back, so the exact
    /// text in `session_events` also travels on the outcome for immediate
    /// UI render.
    async fn journal_line(&self, line: String) -> String {
        let _ = self.record(&line).await;
        line
    }

    /// Single entry-URL retry for an empty batch field: navigate once when
    /// the intent names an entry, settle for rendered row candidates,
    /// re-snapshot once, then stop — `None` when still empty, so the run
    /// fails closed with no approval gate opened. Returns every line this
    /// retry journaled alongside the candidates.
    async fn retry_batch_on_entry(
        &self,
        browser: &std::sync::Arc<browser_driver::ManagedBrowser>,
        portal: &url::Url,
        intent: &macro_engine::SemanticIntent,
    ) -> (Option<Vec<browser_driver::AxElement>>, Vec<String>) {
        let Some(entry) = intent.entry_url.as_deref() else {
            return (None, Vec::new());
        };
        let Ok(entry_url) = url::Url::parse(entry) else {
            return (None, Vec::new());
        };
        if browser.navigate(&entry_url).await.is_err() {
            return (None, Vec::new());
        }
        macro_engine::wait_for_settled_candidates(browser, portal, intent).await;
        self.snapshot_batch_candidates(browser, portal, intent)
            .await
    }

    /// Batch-lane snapshot with confinement bound to step 1 first:
    /// re-anchor to the intentional entry origin, settle-snapshot (plus the
    /// entry-URL retry on empty fields), and combine every journaled line —
    /// re-anchor line first when the anchor changed — into `telemetryLog`.
    /// `None` candidates open no approval gate: the run fails closed.
    async fn snapshot_anchored_batch_candidates(
        &self,
        browser: &std::sync::Arc<browser_driver::ManagedBrowser>,
        portal: &url::Url,
        intent: &macro_engine::SemanticIntent,
        steps: &[playbook_store::Step],
    ) -> (Option<Vec<browser_driver::AxElement>>, Option<String>) {
        let reanchored = self.reanchor_to_entry(browser, steps).await;
        let (candidates, mut journaled) = self
            .snapshot_settled_batch_candidates(browser, portal, intent)
            .await;
        if let Some(line) = reanchored {
            journaled.insert(0, line);
        }
        (candidates, Self::combine_telemetry(&journaled))
    }

    /// Settle-aware candidate collection for the batch lane: poll for
    /// rendered row candidates, snapshot once for the approval count, then
    /// take the single entry-URL retry on empty fields. `None` opens no
    /// approval gate — the run fails closed. Returns every line journaled
    /// across both snapshots (first plus retry) so outcomes surface the
    /// full diagnostic stream in the UI.
    async fn snapshot_settled_batch_candidates(
        &self,
        browser: &std::sync::Arc<browser_driver::ManagedBrowser>,
        portal: &url::Url,
        intent: &macro_engine::SemanticIntent,
    ) -> (Option<Vec<browser_driver::AxElement>>, Vec<String>) {
        macro_engine::wait_for_settled_candidates(browser, portal, intent).await;
        let (candidates, mut journaled) = self
            .snapshot_batch_candidates(browser, portal, intent)
            .await;
        if candidates.is_some() {
            return (candidates, journaled);
        }
        let (retry_candidates, retry_lines) =
            self.retry_batch_on_entry(browser, portal, intent).await;
        journaled.extend(retry_lines);
        (retry_candidates, journaled)
    }

    /// Build one snapshot's full diagnostic stream in strict journal order
    /// — counter, check, CDP error, resync action(s), stats — from the
    /// inline values [`browser_driver::ManagedBrowser::ax_snapshot`]
    /// returns. Pure construction over owned values: no side-channel
    /// state, nothing to drain out of order. The check line is emitted
    /// whenever the snapshot observed zero nodes (or a CDP error); healthy
    /// snapshots journal only counter plus stats.
    fn snapshot_journal_lines(
        elements: &[browser_driver::AxElement],
        check: &browser_driver::AxResyncCheck,
        resyncs: u64,
        intent: &macro_engine::SemanticIntent,
    ) -> Vec<String> {
        // `before_discard` is structurally 0: inline returns replaced the
        // drainable side-channels, so nothing can be wiped before journaling.
        // `after_drain` carries this snapshot's own resync count.
        let mut lines = vec![format!(
            "ax_resync_counter: before_discard=0 after_drain={resyncs}"
        )];
        if check.node_count == 0 || check.cdp_error.is_some() {
            lines.push(check.line());
        }
        if let Some(error) = check.cdp_error.as_deref() {
            lines.push(format!("ax_snapshot_cdp_error: '{error}'"));
        }
        for _ in 0..resyncs {
            lines.push(browser_driver::AX_TARGET_RESYNC_LINE.to_owned());
        }
        let noun = macro_engine::settle_probe_text(intent);
        lines.push(Self::snapshot_telemetry_line(
            elements.len(),
            Self::count_noun_matches(elements, noun),
            noun,
        ));
        lines
    }

    /// Read-only pre-snapshot for the batch lane: live candidates for the
    /// approval count, or `None` when the field is empty or unreadable.
    /// Every snapshot — hit or miss — journals its full diagnostic stream
    /// in strict order, so a zero field still leaves evidence instead of
    /// silence, and returns every logged line for UI surfacing. CDP
    /// failures arrive as check data (never `Err`), and an empty field
    /// fails closed via `resolve_batch` like single-run browser errors do.
    async fn snapshot_batch_candidates(
        &self,
        browser: &std::sync::Arc<browser_driver::ManagedBrowser>,
        portal: &url::Url,
        intent: &macro_engine::SemanticIntent,
    ) -> (Option<Vec<browser_driver::AxElement>>, Vec<String>) {
        let (elements, check, resyncs) = browser.ax_snapshot(portal).await;
        let mut journaled = Vec::new();
        for line in Self::snapshot_journal_lines(&elements, &check, resyncs, intent) {
            journaled.push(self.journal_line(line).await);
        }
        let candidates = match macro_engine::resolve_batch(&elements, intent) {
            macro_engine::ResolveOutcome::BatchMatch(batch) => Some(batch),
            _ => None,
        };
        (candidates, journaled)
    }

    /// One batch approval naming the live candidate count and carrying the
    /// itemized previews for the gate card. Denials flow back as `false`
    /// for the caller to report Blocked.
    async fn approve_batch(
        &self,
        run_id: u64,
        candidates: &[browser_driver::AxElement],
        intent: &macro_engine::SemanticIntent,
        prompt: &str,
        events: &std::sync::Mutex<&mut (impl FnMut(PlaybookEvent) + Send)>,
    ) -> bool {
        // When an entry was resolved (stored or proposed), the gate card
        // names its destination so a wrong route is visible pre-consent.
        // Host plus path only — query strings never reach the summary.
        let mut summary = format!(
            "Batch click: {} {} controls for {prompt}",
            candidates.len(),
            intent.role,
        );
        if let Some(entry) = intent.entry_url.as_deref()
            && let Ok(url) = url::Url::parse(entry)
        {
            use std::fmt::Write as _;
            let _ = write!(
                summary,
                " @ {}{}",
                url.host_str().unwrap_or("?"),
                url.path()
            );
        }
        self.approve_playbook_step(
            run_id,
            0,
            1,
            ApprovalContent {
                kind: "intent",
                summary,
                candidates: candidate_previews(candidates),
            },
            events,
        )
        .await
    }

    /// Fail-open journal close for batch runs: telemetry must never fail a
    /// run, mirroring `run_steps`.
    async fn finish_batch_run(
        journal: Option<&playbook_store::PlaybookStore>,
        journal_id: &str,
        status: &str,
        completed: usize,
    ) {
        if let Some(store) = journal {
            let _ = store.record_run_finish(journal_id, status, completed).await;
        }
    }

    /// One-line progress event for the single-step batch lane.
    fn emit_batch_phase(
        events: &std::sync::Mutex<&mut (impl FnMut(PlaybookEvent) + Send)>,
        run_id: u64,
        phase: orchestration_engine::SequencePhase,
    ) {
        if let Ok(mut emit) = events.lock() {
            emit(PlaybookEvent {
                run_id,
                step_index: 0,
                total_steps: 1,
                phase,
                highlight: None,
                approval: None,
            });
        }
    }

    /// Terminal batch outcome: one ephemeral step carrying N actions, plus
    /// the route-proposal and snapshot-telemetry lines for immediate
    /// Session Activity render.
    fn batch_outcome(
        name: String,
        steps: Vec<playbook_store::Step>,
        status: orchestration_engine::SequenceStatus,
        completed: usize,
        stopped: Option<usize>,
        route_log: Option<String>,
        telemetry_log: Option<String>,
    ) -> DispatchOutcome {
        DispatchOutcome {
            kind: "ephemeral",
            name,
            result: orchestration_engine::SequenceOutcome {
                completed_steps: completed,
                total_steps: 1,
                status,
                stopped_at: stopped,
            },
            steps,
            route_log,
            telemetry_log,
        }
    }

    /// Shared run machinery for stored and ephemeral playbooks: session
    /// contract, semaphore, headed browser, per-run output dir, approval
    /// gates, and progress streaming.
    async fn run_steps(
        &self,
        portal: url::Url,
        steps: &[playbook_store::Step],
        scope: RunScope,
        mut emit: impl FnMut(PlaybookEvent) + Send,
    ) -> Result<orchestration_engine::SequenceOutcome, AppError> {
        let connected = self
            .session_origin
            .lock()
            .map_err(|_| AppError::Internal)?
            .clone();
        if connected.is_none_or(|url| url.origin() != portal.origin()) {
            return Err(AppError::SessionRequired);
        }
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        let browser = self.browser(false).await?;
        let run_id = self.next_playbook_run.fetch_add(1, Ordering::Relaxed);
        let output = self
            .data
            .join("downloads")
            .join(format!("playbook-{run_id}"));
        let total_steps = steps.len();
        // Telemetry is fail-open by design: a journal write must never fail a
        // run. The id mixes the per-launch counter with wall time so restarts
        // cannot collide.
        let journal_id = format!(
            "run-{run_id}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        );
        let journal = self.playbooks().await.ok();
        if let Some(store) = &journal {
            let _ = store
                .record_run_start(
                    &journal_id,
                    scope.playbook_id.as_deref(),
                    scope.kind,
                    total_steps,
                )
                .await;
        }
        let events = std::sync::Mutex::new(&mut emit);
        let outcome = orchestration_engine::run_playbook_sequence(
            &browser,
            &portal,
            &output,
            steps,
            |event| {
                if let Ok(mut emit) = events.lock() {
                    emit(PlaybookEvent {
                        run_id,
                        step_index: event.step_index,
                        total_steps: event.total_steps,
                        phase: event.phase,
                        highlight: event.highlight,
                        approval: None,
                    });
                }
            },
            |index, action| {
                self.approve_playbook_step(
                    run_id,
                    index,
                    total_steps,
                    ApprovalContent {
                        kind: "action",
                        summary: Self::action_summary(&action),
                        candidates: Vec::new(),
                    },
                    &events,
                )
            },
            |index, intent| {
                self.approve_playbook_step(
                    run_id,
                    index,
                    total_steps,
                    ApprovalContent {
                        kind: "intent",
                        summary: format!("{} · {}", intent.role, intent.label_query),
                        candidates: Vec::new(),
                    },
                    &events,
                )
            },
        )
        .await;
        if let Some(store) = &journal {
            let status = match outcome.status {
                orchestration_engine::SequenceStatus::Completed => "completed",
                orchestration_engine::SequenceStatus::NeedsRepair => "needs_repair",
                orchestration_engine::SequenceStatus::Denied => "denied",
                orchestration_engine::SequenceStatus::Failed => "failed",
            };
            let _ = store
                .record_run_finish(&journal_id, status, outcome.completed_steps)
                .await;
        }
        Ok(outcome)
    }

    pub async fn close_browser(&self) -> Result<(), AppError> {
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        *self.session_origin.lock().map_err(|_| AppError::Internal)? = None;
        let browser = self.browser.lock().map_err(|_| AppError::Internal)?.take();
        if let Some(browser) = browser {
            browser
                .shutdown()
                .await
                .map_err(|_| AppError::BrowserUnavailable)?;
        }
        Ok(())
    }

    pub fn terminate_browser(&self) {
        if let Ok(guard) = self.browser.lock()
            && let Some(browser) = guard.as_ref()
        {
            browser.terminate();
        }
    }

    pub fn preview_approval(&self) -> Result<ApprovalPreview, AppError> {
        let id = self.next_approval.fetch_add(1, Ordering::Relaxed);
        let mut pending = self.approval.lock().map_err(|_| AppError::Internal)?;
        if pending.is_some() {
            return Err(AppError::Busy);
        }
        *pending = Some(id);
        Ok(ApprovalPreview {
            id,
            title: "Sentinel Gate · IPC check",
            description: "Approve this dummy action to verify the Rust-to-React round trip. No form is submitted, no payment is made, and no file is downloaded.",
        })
    }

    pub fn resolve_approval(&self, id: u64, approved: bool) -> Result<bool, AppError> {
        let mut pending = self.approval.lock().map_err(|_| AppError::Internal)?;
        if *pending != Some(id) {
            return Err(AppError::StaleApproval);
        }
        *pending = None;
        Ok(approved)
    }
}

fn engine_error(error: &EngineError) -> AppError {
    match error {
        EngineError::Invalid => AppError::InvalidInput(
            "Check the workflow name, portal, and selectors. Saved workflows must use the same portal URL.",
        ),
        EngineError::Database(_) | EngineError::Io(_) => AppError::StorageUnavailable,
        _ => AppError::WorkflowFailed,
    }
}

fn default_chromium() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
    }
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        PathBuf::from("google-chrome")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let root = dir.path().join("downloads/1");
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
                    task.plan.steps[1].output.files[0].path =
                        outside.to_string_lossy().into_owned();
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
        let (first, second) =
            tokio::join!(service.bridge_server_on(0), service.bridge_server_on(0));
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
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "run");
        assert_eq!(listed[0].step_count, 1);
        // No portal connected: execution is rejected before any browser I/O.
        assert!(matches!(
            service.execute_playbook(listed[0].id.clone(), |_| {}).await,
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
    async fn service_routes_plural_prompt_to_batch_execution()
    -> Result<(), Box<dyn std::error::Error>> {
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
    async fn propose_entry_attaches_validated_table_route() -> Result<(), Box<dyn std::error::Error>>
    {
        // No browser, no database: the tiered proposer is pure until the
        // record call, which fails open without an initialized pool.
        let service = AppService::new(PathBuf::new(), PathBuf::new());
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
        service
            .propose_entry_url("download all my invoices from github", &mut intent)
            .await;
        assert_eq!(
            intent.entry_url.as_deref(),
            Some("https://github.com/account/billing/history")
        );
        // Non-matching prompts and pre-set entries stay untouched.
        let mut other = intent.clone();
        other.label_query = "dashboard".into();
        other.entry_url = None;
        service
            .propose_entry_url("open the dashboard", &mut other)
            .await;
        assert_eq!(other.entry_url, None);
        let mut preset = intent.clone();
        preset.entry_url = Some("https://github.com/account/billing/history".into());
        service
            .propose_entry_url("download all my invoices from github", &mut preset)
            .await;
        assert_eq!(
            preset.entry_url.as_deref(),
            Some("https://github.com/account/billing/history")
        );
        Ok(())
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
        service.propose_entry_url(prompt, &mut intent).await;
        // Explicit propagation: the proposed route lands on both the intent
        // and step 1 of the ephemeral task.
        let expected = "https://github.com/account/billing/history";
        assert_eq!(intent.entry_url.as_deref(), Some(expected));
        let steps = [playbook_store::Step::Semantic {
            intent: intent.clone(),
        }];
        let playbook_store::Step::Semantic {
            intent: step_intent,
        } = &steps[0]
        else {
            panic!("step 1 is semantic");
        };
        assert_eq!(step_intent.entry_url.as_deref(), Some(expected));
        // Pre-navigation triggers from a foreign tab: step 1 differs from
        // `google.com`, so `ensure_at_entry_url` would issue CDP navigation
        // before the first `ax_snapshot`.
        let current = url::Url::parse("https://google.com")?;
        let entry = url::Url::parse(step_intent.entry_url.as_deref().ok_or("entry")?)?;
        assert!(macro_engine::entry_url_mismatched(&current, &entry));
        // Session Activity carries the proposal (host + path, never query).
        let pool = service.database().await.map_err(|_| "database")?;
        let rows: Vec<(String,)> = sqlx::query_as("SELECT outcome FROM session_events")
            .fetch_all(pool)
            .await
            .map_err(|_| "events")?;
        assert!(
            rows.iter().any(|(outcome,)| outcome
                .starts_with("route_proposed:github.com/account/billing/history")),
            "route_proposed logged, got {rows:?}"
        );
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
            "ax_snapshot_telemetry: total_nodes=142, target_noun_matches=3 (noun='invoice')"
                .to_owned(),
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
        assert!(combined.lines().count() == lines.len());
        // Paths that never snapshot stay silent instead of emitting empties.
        assert_eq!(AppService::combine_telemetry(&[]), None);
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
    async fn batch_approval_names_resolved_entry_destination()
    -> Result<(), Box<dyn std::error::Error>> {
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
    async fn natural_commands_reject_empties_and_dead_ends()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
        // Empty prompts fail before touching storage.
        assert!(matches!(
            service.dispatch_natural_command("   ".into(), |_| {}).await,
            Err(AppError::InvalidInput(_))
        ));
        // No saved match and no connected portal: honest dead end, no browser I/O.
        assert!(matches!(
            service
                .dispatch_natural_command("download my report".into(), |_| {})
                .await,
            Err(AppError::InvalidInput(_))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn poc_metrics_reads_empty_and_seeded_databases() -> Result<(), Box<dyn std::error::Error>>
    {
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
}
