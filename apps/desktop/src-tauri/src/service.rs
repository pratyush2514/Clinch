#![deny(unsafe_code)]
use crate::auth::{AuthPanel, ReauthReason, reason_for_signal};
use browser_driver::{Action, LaunchOptions, ManagedBrowser};
use futures::StreamExt;
use orchestration_engine::{Engine, EngineError, Task, TaskEvent, TaskId, TaskRequest};
use serde::Serialize;
use session_sync::{FallbackReason, PreparedSync, SyncRequest};
use std::{
    collections::{BTreeMap, VecDeque},
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
    /// Exact startup build line journaled as the first `session_events`
    /// row, surfaced so Session Activity names the running binary.
    startup_build: String,
}

/// Exact binary identity for Session Activity startup telemetry, e.g.
/// `startup_build: v0.1.0 · hash:abc1234`. Version comes from the workspace
/// manifest; the hash is stamped by build.rs from git (or a build nonce).
/// Single source for the journaled row and the `initialize` response so the
/// two can never disagree.
fn startup_build_line() -> String {
    format!(
        "startup_build: v{} · hash:{}",
        env!("CARGO_PKG_VERSION"),
        env!("CLINCH_BUILD_HASH")
    )
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

/// App-owned background browser context state for the UI preview card.
/// Read-only snapshot: never launches as a side effect.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextStatus {
    /// A managed Chromium session is currently attached.
    attached: bool,
    /// The attached session runs without an OS window.
    headless: bool,
}

/// Tauri event carrying one base64 JPEG viewport frame to the preview card.
pub const SCREENCAST_EVENT: &str = "browser-screencast-frame";

/// Why a managed-browser handle is being acquired.
///
/// The window-visibility contract lives in this type rather than in a bare
/// `headless: bool` at each call site, so "background work never opens an OS
/// window" is checkable by reading the argument instead of tracing a boolean.
///
/// * [`Self::Background`] — dispatch lanes, playbook runs, task replay, and
///   screencast acquisition. Always `--headless=new`.
/// * [`Self::Interactive`] — only actions the user asked for by name:
///   manual login, in-app re-authentication, source-profile and bridge sync
///   (each may need a visible login/2FA page), and Take Control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrowserIntent {
    Background,
    Interactive,
}

impl BrowserIntent {
    /// Launch options for a fresh process. Background is headless by
    /// construction: no code path can launch it any other way.
    fn launch_options(self) -> LaunchOptions {
        match self {
            Self::Background => LaunchOptions::replay(),
            Self::Interactive => LaunchOptions::interactive(),
        }
    }
}

/// What acquiring a browser should do with the session that is already
/// attached. Separated from the CDP work so the window-visibility invariant
/// is provable without launching Chromium.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AcquireAction {
    /// Take the live session exactly as it is.
    Reuse,
    /// Restart the live session under the requested window mode.
    Restart,
    /// Nothing attached: launch a fresh process.
    Launch,
}

/// Decide how to satisfy `intent` given the attached session's window mode
/// (`attached_headless`; `None` when dormant).
///
/// Background never restarts: it reuses whatever is attached, so a run can
/// neither open a window nor close one the user opened. Interactive restarts
/// only when the live session is headless.
fn acquire_action(intent: BrowserIntent, attached_headless: Option<bool>) -> AcquireAction {
    match (intent, attached_headless) {
        (_, None) => AcquireAction::Launch,
        (BrowserIntent::Background, Some(_)) | (BrowserIntent::Interactive, Some(false)) => {
            AcquireAction::Reuse
        }
        (BrowserIntent::Interactive, Some(true)) => AcquireAction::Restart,
    }
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
/// How long one playbook approval waits for a human decision before the
/// gate denies by default and the run fails closed.
const APPROVAL_TIMEOUT: Duration = Duration::from_mins(5);
/// Bounds for one visual-picker wait: long enough to find and click an
/// element, short enough that a forgotten armed picker cannot stall the
/// single-operation permit indefinitely.
const PICKER_TIMEOUT_MIN_MS: u64 = 1_000;
const PICKER_TIMEOUT_MAX_MS: u64 = 120_000;
/// Download staging directory under the app-data root. Both the run
/// machinery and the file-serving guard resolve through this name so a
/// download can never escape its per-run folder.
const DOWNLOADS_DIR: &str = "downloads";

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

/// Outcome of one entry-route proposal: the exact journaled line (so the UI
/// can render it without polling) plus which tier answered.
///
/// The tier matters because the search tier only reaches a results page.
/// A `SearchFallback` entry still owes a Stage-2 follow before the intent
/// can run against its real destination.
#[derive(Clone, Debug, Default)]
struct ProposedEntry {
    log: Option<String>,
    source: Option<orchestration_engine::RouteSource>,
    /// The prompt was a direct open the ladder could not ground: no saved
    /// shortcut, no typed domain, no directory hit. Dispatch turns this
    /// into ask-and-learn guidance instead of the generic "not runnable".
    direct_open_miss: bool,
    /// Whether the fenced domain grounder was env-configured when this
    /// proposal was built. Carried so a direct-open miss can say *why* the
    /// ladder could not ground the name: an unconfigured grounder is a
    /// setup problem, a configured one that declined is a genuine miss.
    /// Defaults to false; only the miss path reads it.
    grounder_configured: bool,
    /// A domain-grounder hit worth remembering: the site slot the prompt
    /// named and the URL the grounder resolved it to. Carried on the
    /// proposal — never journaled there — so dispatch can offer it as a
    /// shortcut only *after* navigation to the grounded URL completes. A
    /// proposal that never lands must not offer anything.
    grounder_hit: Option<GrounderHit>,
}

/// A `RouteSource::DomainGrounded` resolution the Action Thread may offer to
/// keep: the bare site slot plus the exact URL the grounder returned.
#[derive(Clone, Debug, Default)]
struct GrounderHit {
    site: String,
    url: String,
}

/// Everything the batch dispatch lane needs after its preamble: the
/// attached browser, the run's journal handles, the proposed route's log
/// line, and the post-landing shortcut offer (when the ladder grounded one).
struct BatchPreamble {
    browser: std::sync::Arc<browser_driver::ManagedBrowser>,
    run_id: u64,
    journal_id: String,
    journal: Option<playbook_store::PlaybookStore>,
    name: String,
    steps: Vec<playbook_store::Step>,
    route_log: Option<String>,
    shortcut_offer: Option<String>,
}

impl ProposedEntry {
    /// Whether this proposal landed on a search page rather than the
    /// destination, and therefore needs a follow-through click.
    fn needs_search_follow(&self) -> bool {
        self.source == Some(orchestration_engine::RouteSource::SearchFallback)
    }
}

/// Where a Stage-2 follow landed: the origin-normalized run portal plus the
/// journaled line naming the destination.
#[derive(Clone, Debug)]
struct FollowedDestination {
    portal: url::Url,
    log: String,
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
    /// Completed-run registry key for `save_run_as_workflow`: `Some` only
    /// when this ephemeral run completed and was remembered (the exact
    /// executed graph plus origin, no client round-trip). `None` for saved
    /// replays and non-completed ephemerals. Additive to the IPC shape:
    /// older clients ignore unknown keys.
    run_id: Option<String>,
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
    /// Live screencast pump forwarding viewport frames to the UI. Aborted
    /// on release, takeover, re-acquire, and app exit — never detached.
    screencast: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Completed ephemeral runs awaiting persistence: the executed graph
    /// (origin plus steps) keyed by journal id, newest last. Session memory
    /// only — `save_run_as_workflow` drains entries into durable playbooks.
    completed_runs: Mutex<VecDeque<CompletedRun>>,
    next_playbook_run: AtomicU64,
    /// Fenced structured-intent parser consulted only when the deterministic
    /// grammar parse is not confident, and only for slots — never for URLs,
    /// selectors, or code.
    ///
    /// Ships as [`orchestration_engine::StubIntentParser`], which declines
    /// every prompt so low-confidence commands degrade to raw search. That
    /// keeps the app offline-first with no model dependency, no API key, and
    /// no prompt leaving the machine; choosing a local or hosted provider is
    /// a product decision that swaps this one field.
    intent_parser: Arc<dyn orchestration_engine::IntentParser>,
}

/// One finished ephemeral run held for persistence: the exact executed
/// graph plus the portal it ran under. Recorded only on terminal
/// completion; anything else never becomes saveable. The save command
/// supplies a fresh name, so none is stored here.
///
/// `prompt` is the phrasing that produced the run. It becomes the saved
/// workflow's prompt key if — and only if — the user chooses to save, which
/// is what closes the learning loop: an irregular prompt that needed the
/// parser seam once resolves from storage every time after.
#[derive(Clone, Debug)]
struct CompletedRun {
    id: String,
    origin: url::Url,
    steps: Vec<playbook_store::Step>,
    prompt: String,
}

/// Session cap on persistable completed runs: old entries evict
/// first-in-first-out, so the registry can never grow with the session.
const MAX_COMPLETED_RUNS: usize = 32;

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
            screencast: Mutex::new(None),
            completed_runs: Mutex::new(VecDeque::new()),
            next_playbook_run: AtomicU64::new(1),
            intent_parser: Arc::new(orchestration_engine::StubIntentParser),
        }
    }

    /// Swap the intent-parser seam. Builder-style so the field stays
    /// immutable at runtime — a parser is chosen at construction, never
    /// mid-session.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_intent_parser(
        mut self,
        parser: Arc<dyn orchestration_engine::IntentParser>,
    ) -> Self {
        self.intent_parser = parser;
        self
    }

    /// Resolve the slots search-and-follow grounds on for `prompt`.
    ///
    /// Routes through the confidence gate: a crisp command is answered by
    /// grammar alone (zero tokens), an irregular one gets one bounded parser
    /// shot, and anything unanswered degrades to ungrounded slots plus raw
    /// search. Shared by Stage 2 and its tests so both exercise the same
    /// cascade.
    fn follow_slots(&self, prompt: &str) -> orchestration_engine::ResolvedSlots {
        let ctx = orchestration_engine::ResolutionContext {
            account_dir: None,
            llm: None,
            parser: Some(&self.intent_parser),
            shortcuts: None,
            site_search: None,
            domain_grounder: None,
            region_hint: "",
        };
        orchestration_engine::resolve_slots(prompt, None, &ctx)
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
        // Schema first, then the startup build line as the very first
        // `session_events` row of this boot — before engine recovery or any
        // other journal write — so Session Activity always opens with the
        // exact binary identity and stale builds are unambiguous.
        self.database().await?;
        let startup_build = startup_build_line();
        self.record(&startup_build).await?;
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
            startup_build,
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
        // Task runs are background work in both modes: replay additionally
        // *requires* headless (`EngineError::HeadlessRequired`), and a
        // first-run recording has no reason to put a window on screen either.
        let browser = self.browser(BrowserIntent::Background).await?;
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
        let root = tokio::fs::canonicalize(self.data.join(DOWNLOADS_DIR).join(id.0.to_string()))
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

    /// Managed Chromium executable: the explicit override, else the
    /// platform default install path.
    fn chromium_executable() -> PathBuf {
        std::env::var_os("CLINCH_CHROMIUM_PATH").map_or_else(default_chromium, PathBuf::from)
    }

    /// App-owned persistent profile directory. Never the user's daily profile.
    fn browser_profile(&self) -> PathBuf {
        self.data.join("browser-profile")
    }

    /// Acquire the managed browser for `intent`, launching lazily when no
    /// session is attached.
    ///
    /// [`BrowserIntent::Background`] can never create an OS window: it
    /// launches headless and, when a session already exists, reuses it
    /// exactly as-is instead of restarting. Reuse-as-is matters in both
    /// directions — a background run can neither promote a headless context
    /// into a visible window nor demote a window the user opened with Take
    /// Control. Only [`BrowserIntent::Interactive`] may restart a headless
    /// session into a visible one.
    async fn browser(&self, intent: BrowserIntent) -> Result<Arc<ManagedBrowser>, AppError> {
        let existing = self.browser.lock().map_err(|_| AppError::Internal)?.clone();
        match acquire_action(intent, existing.as_ref().map(|live| live.is_headless())) {
            AcquireAction::Reuse => return existing.ok_or(AppError::BrowserUnavailable),
            AcquireAction::Restart => {
                let live = existing.ok_or(AppError::BrowserUnavailable)?;
                return self.restart_browser(&live, intent).await;
            }
            AcquireAction::Launch => {}
        }
        let browser = Arc::new(
            ManagedBrowser::launch_with_options(
                &Self::chromium_executable(),
                &self.browser_profile(),
                intent.launch_options(),
            )
            .await
            .map_err(|_| AppError::BrowserUnavailable)?,
        );
        *self.browser.lock().map_err(|_| AppError::Internal)? = Some(browser.clone());
        Ok(browser)
    }

    /// Restart the attached session under `intent`'s window mode, carrying
    /// cookies in memory. A failed restart clears both the handle and the
    /// connected origin so the next acquisition starts from a clean state
    /// rather than reusing a dead target.
    async fn restart_browser(
        &self,
        browser: &ManagedBrowser,
        intent: BrowserIntent,
    ) -> Result<Arc<ManagedBrowser>, AppError> {
        let restarted = browser
            .restart(
                &Self::chromium_executable(),
                &self.browser_profile(),
                intent.launch_options(),
            )
            .await;
        let Ok(restarted) = restarted else {
            *self.browser.lock().map_err(|_| AppError::Internal)? = None;
            *self.session_origin.lock().map_err(|_| AppError::Internal)? = None;
            return Err(AppError::BrowserUnavailable);
        };
        let restarted = Arc::new(restarted);
        *self.browser.lock().map_err(|_| AppError::Internal)? = Some(restarted.clone());
        Ok(restarted)
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
        // Interactive: an imported session may still land on a login/2FA
        // page the user has to complete in a visible window.
        let browser = self.browser(BrowserIntent::Interactive).await?;
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
            "The companion bridge could not start. Is its port already in use?",
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
        // Interactive: same reason as local-profile sync — the landing may
        // be a challenge page the user must finish.
        let browser = self.browser(BrowserIntent::Interactive).await?;
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
        // Interactive by definition: the user signs in in this window.
        let browser = self.browser(BrowserIntent::Interactive).await?;
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
        // Interactive: the panel exists so the user can re-authenticate here.
        let browser = self.browser(BrowserIntent::Interactive).await?;
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
        let timeout_ms = timeout_ms.clamp(PICKER_TIMEOUT_MIN_MS, PICKER_TIMEOUT_MAX_MS);
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
        self.persist_playbook(playbook).await
    }

    /// Shared persistence core: validate-then-upsert one playbook, mapping
    /// store failures to UI errors. Returns the row id as text.
    async fn persist_playbook(
        &self,
        playbook: playbook_store::Playbook,
    ) -> Result<String, AppError> {
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

    /// Record one terminally completed ephemeral run for later persistence,
    /// evicting the oldest entries past [`MAX_COMPLETED_RUNS`]. Only
    /// completed runs are remembered: anything else was never proven.
    ///
    /// Remembering is not saving. The entry lives in session memory until the
    /// user explicitly accepts the "Save as Playbook" card; declining (or
    /// simply moving on) persists nothing, which keeps the store free of runs
    /// nobody wanted to keep.
    fn remember_completed_run(
        &self,
        id: &str,
        origin: &url::Url,
        steps: &[playbook_store::Step],
        prompt: &str,
    ) {
        if let Ok(mut runs) = self.completed_runs.lock() {
            runs.push_back(CompletedRun {
                id: id.to_owned(),
                origin: origin.clone(),
                steps: steps.to_vec(),
                prompt: prompt.to_owned(),
            });
            while runs.len() > MAX_COMPLETED_RUNS {
                runs.pop_front();
            }
        }
    }

    /// Persist a completed ephemeral run as a durable, re-runnable
    /// playbook: the exact executed graph (entry route, batch intent, noun,
    /// selectors) plus an optional memo, stored under `name`. The run must
    /// still sit in the session registry — unknown ids fail closed, and
    /// evicted graphs fail closed asking for a re-run. Returns the row id
    /// as text for `execute_playbook`.
    pub async fn save_run_as_workflow(
        &self,
        run_id: String,
        name: String,
        description: Option<String>,
    ) -> Result<String, AppError> {
        let run = self
            .completed_runs
            .lock()
            .map_err(|_| AppError::Internal)?
            .iter()
            .find(|run| run.id == run_id)
            .cloned();
        let Some(run) = run else {
            return Err(AppError::InvalidInput(
                "Unknown or expired run. Re-run the prompt, then save the completed workflow.",
            ));
        };
        let memo = description
            .map(|memo| memo.trim().to_owned())
            .filter(|memo| !memo.is_empty());
        // This click is the learning loop closing. The prompt that produced
        // the run becomes the workflow's key, so the next time the user says
        // the same thing it resolves from storage instead of re-parsing —
        // the whole point of remembering an irregular phrasing once.
        let playbook = playbook_store::Playbook::new(name, run.origin, run.steps)
            .map_err(|_| AppError::InvalidInput("Check the workflow name, portal, and steps."))?
            .with_description(memo)
            .with_prompt_key(orchestration_engine::prompt_key(&run.prompt));
        self.persist_playbook(playbook).await
    }

    /// Newest-first stored-playbook summaries for the workflow list.
    pub async fn list_playbooks(&self) -> Result<Vec<playbook_store::PlaybookSummary>, AppError> {
        self.playbooks()
            .await?
            .list_playbooks()
            .await
            .map_err(|_| AppError::StorageUnavailable)
    }

    /// Save a site shortcut: a user-chosen name → destination URL. The URL
    /// is validated as user-directed (absolute https, no credentials)
    /// before it reaches the store, so the direct-open ladder never learns
    /// a broken rung. Names normalize at the store boundary.
    pub async fn save_site_shortcut(
        &self,
        name: String,
        url: String,
    ) -> Result<playbook_store::SiteShortcut, AppError> {
        orchestration_engine::validate_user_directed_url(&url).map_err(|_| {
            AppError::InvalidInput("The shortcut URL must be an absolute https URL.")
        })?;
        self.playbooks()
            .await?
            .set_site_shortcut(&name, &url)
            .await
            .map_err(|err| match err {
                playbook_store::StoreError::Shortcut(_) => {
                    AppError::InvalidInput("That shortcut name or URL is not valid.")
                }
                _ => AppError::StorageUnavailable,
            })
    }

    /// Every saved site shortcut, for the palette editor.
    pub async fn list_site_shortcuts(&self) -> Result<Vec<playbook_store::SiteShortcut>, AppError> {
        self.playbooks()
            .await?
            .list_site_shortcuts()
            .await
            .map_err(|_| AppError::StorageUnavailable)
    }

    /// Delete a site shortcut. `false` when the name was never saved.
    pub async fn delete_site_shortcut(&self, name: String) -> Result<bool, AppError> {
        self.playbooks()
            .await?
            .delete_site_shortcut(&name)
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
        let approved = tokio::time::timeout(APPROVAL_TIMEOUT, receive)
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
        let (result, _) = self
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
        Ok(result)
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
        // Tier 0: closed-world browser lifecycle. Runs before `resolve_command`
        // and before the synthetic search-origin fallback below — "spin up
        // browser for me" must never be parsed as an intent or sent to
        // search. Pure and prompt-local, so it costs no model or network.
        if let Some(command) = orchestration_engine::resolve_app_command(&prompt) {
            return self.dispatch_app_command(command, emit).await;
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
            None if connected.is_none() => {
                // Ad-hoc without a connected portal: resolve against a
                // synthetic search origin so free-form prompts still yield an
                // ephemeral intent, then auto-acquire the browser and run
                // via the grounded search-fallback tier. The Portal URL
                // field stays an optional override, never a prerequisite.
                let fallback_origin = url::Url::parse("https://www.google.com/").ok();
                let fallback_ref = fallback_origin.as_ref();
                match orchestration_engine::resolve_command(&prompt, fallback_ref, &saved) {
                    Some(matched) => match dispatch_lane(&matched) {
                        DispatchLane::Saved => Err(AppError::SessionRequired),
                        DispatchLane::Single | DispatchLane::Batch => {
                            let orchestration_engine::CommandMatch::Ephemeral { intent } = matched
                            else {
                                return Err(AppError::Internal);
                            };
                            self.dispatch_adhoc_auto_acquire(prompt, intent, emit).await
                        }
                    },
                    None => Err(AppError::InvalidInput(
                        "No saved workflow matches for this prompt. Try a different description.",
                    )),
                }
            }
            None => Err(AppError::InvalidInput(
                "No saved workflow matches, and no portal is connected for an ad-hoc intent. Connect a portal or save a workflow first.",
            )),
        }
    }

    /// Tier 0 execution: a closed-world app command, no intent parsing, no
    /// route proposal, no search. `OpenBlankBrowser` lazily attaches the
    /// browser (launching a visible window when none is attached) and
    /// guarantees `about:blank` — the blank-canvas spin-up, journaled as
    /// `browser_opened` with no route line at all.
    async fn dispatch_app_command(
        &self,
        command: orchestration_engine::AppCommand,
        _emit: impl FnMut(PlaybookEvent) + Send,
    ) -> Result<DispatchOutcome, AppError> {
        match command {
            orchestration_engine::AppCommand::OpenBlankBrowser => {
                let browser = self.browser(BrowserIntent::Interactive).await?;
                let blank = url::Url::parse("about:blank").map_err(|_| AppError::Internal)?;
                macro_engine::ensure_at_entry_url(&browser, &blank)
                    .await
                    .map_err(|_| AppError::BrowserUnavailable)?;
                let line = "browser_opened: about:blank · lifecycle";
                let _ = self.record(line).await;
                Ok(DispatchOutcome {
                    kind: "lifecycle",
                    name: "open blank browser".to_owned(),
                    result: orchestration_engine::SequenceOutcome {
                        completed_steps: 0,
                        total_steps: 0,
                        status: orchestration_engine::SequenceStatus::Completed,
                        stopped_at: None,
                    },
                    steps: Vec::new(),
                    run_id: None,
                    route_log: None,
                    telemetry_log: Some(line.to_owned()),
                })
            }
        }
    }

    /// Ad-hoc dispatch without a prior portal connection: auto-acquire the
    /// browser (lazy launch), resolve the entry via the tiered resolver
    /// (entity → LLM → direct-open ladder → grounded search fallback),
    /// journal the target, `ensure_at_entry_url`, then `reanchor_portal`
    /// before running. The derived entry origin becomes the run portal, so
    /// the Portal URL input stays an optional override.
    ///
    /// Two-stage when the search tier answered: Stage 1 lands the results
    /// page, Stage 2 follows the top matching result through to the real
    /// destination, and only then does the intent execute. The run reports
    /// success only if Stage 2 landed somewhere.
    ///
    /// Trust boundary: the Stage-1 entry is a *machine-proposed* URL and is
    /// allowlist-validated below. The Stage-2 destination is not — it is
    /// observed from a real click on a real link rendered by the search
    /// engine, so there is no proposed host to validate. Confinement moves to
    /// that observed origin rather than trusting a predicted one.
    async fn dispatch_adhoc_auto_acquire(
        &self,
        prompt: String,
        mut intent: macro_engine::SemanticIntent,
        emit: impl FnMut(PlaybookEvent) + Send,
    ) -> Result<DispatchOutcome, AppError> {
        // Tiered resolution first so the destination is journaled even when
        // the browser cannot start (mirrors the connected lane ordering).
        // `propose_entry_url` validates every tier (https, no credentials,
        // allowlisted host) and journals `route_fallback: search` for the
        // grounded template. Its reported tier decides whether Stage 2 below
        // still owes a follow-through click.
        let proposed = self.propose_entry_url(&prompt, &mut intent, None).await;
        let route_log = proposed.log.clone();
        // Ask-and-learn: the ladder could not ground this direct open.
        // Fail here with guidance instead of the generic "not runnable"
        // or, worse, a scraped search page.
        if let Some(err) = Self::direct_open_miss_error(&proposed) {
            return Err(err);
        }
        let entry = intent.entry_url.clone().ok_or(AppError::InvalidInput(
            "The derived intent is not runnable.",
        ))?;
        let entry_url = url::Url::parse(&entry)
            .map_err(|_| AppError::InvalidInput("The derived intent is not runnable."))?;
        // Strict validation before any navigation or session write.
        // Provenance-aware: user-directed destinations (a typed domain, a
        // saved shortcut, a confirmed directory hit, a domain-grounded hit)
        // were named by the user, so structural validation suffices.
        // Machine-proposed URLs keep the host allowlist.
        let valid = orchestration_engine::entry_url_valid(proposed.source, entry_url.as_str());
        if !valid {
            return Err(AppError::InvalidInput(
                "The derived intent is not runnable.",
            ));
        }
        // The entry origin becomes the run portal; record it as the session
        // so the shared run machinery's origin check passes without a prior
        // manual Portal URL. Query/fragment never enter the session origin.
        let mut portal = entry_url.clone();
        portal.set_path("/");
        portal.set_query(None);
        portal.set_fragment(None);
        *self.session_origin.lock().map_err(|_| AppError::Internal)? = Some(portal.clone());
        // Stage 2. A search-tier entry only reaches a results page, which is
        // not the destination the prompt asked for. Land Stage 1, follow the
        // top matching result through, and rebind the run to wherever the
        // click actually went — before any intent executes. Navigation is
        // only "complete" once this returns.
        let mut follow_log = None;
        if proposed.needs_search_follow() {
            let browser = self.browser(BrowserIntent::Background).await?;
            // Stage 1: land the search page (headless, no window).
            Self::pre_navigate_to_entry(&browser, &intent).await?;
            let followed = self
                .follow_search_to_destination(&browser, &portal, &mut intent)
                .await?;
            portal = followed.portal;
            follow_log = Some(followed.log);
        }
        // Delegate to the connected lanes: they reuse the attached browser,
        // journal the target, `ensure_at_entry_url` via pre-navigation, and
        // `reanchor_portal` before snapshotting. Batch intents keep the batch
        // lane so plural prompts never truncate to one click. After a Stage-2
        // follow, `entry_url` already names the landed page, so
        // pre-navigation is a no-op instead of a trip back to the results.
        // The first proposal line is preserved because the delegate sees
        // `entry_url` already set and returns `route_log: None`.
        let mut outcome = if intent.is_plural {
            self.dispatch_plural_batch(portal, prompt, intent, emit)
                .await?
        } else {
            self.dispatch_single_ephemeral(portal, prompt, intent, emit)
                .await?
        };
        if outcome.route_log.is_none() {
            outcome.route_log = route_log;
        }
        // Landing detection for the auto-acquire lane: the delegate ran its
        // own pre-navigation (its `?` fails the run otherwise), so reaching
        // here means the outer proposal's grounded URL landed. The
        // delegate's own proposal was skipped (`entry_url` was preset), so
        // it carries no hit — the offer fires exactly once, from here.
        let offer = self.offer_shortcut_after_landing(&proposed).await;
        outcome.telemetry_log = Self::with_shortcut_offer(offer, outcome.telemetry_log.take());
        // The follow line rides the same channel as snapshot telemetry so
        // Session Activity shows the destination it landed on.
        if let Some(line) = follow_log {
            outcome.telemetry_log = Some(match outcome.telemetry_log.take() {
                Some(existing) => format!("{line}\n{existing}"),
                None => line,
            });
        }
        Ok(outcome)
    }

    /// Stage 2 of search-and-follow: from a settled search landing, click the
    /// top result matching the prompt's destination, then rebind the run to
    /// where the click actually landed.
    ///
    /// Rebinding covers all three pieces of run state that name a location:
    /// the driver's portal anchor (so confinement evaluates the destination),
    /// the session origin (so the shared run machinery's origin check passes),
    /// and step 1's `entry_url` (set to the landed page, which makes the
    /// delegate lane's pre-navigation a no-op rather than a trip back to the
    /// results page). The destination is read from the live target, never
    /// predicted from the prompt, so no TLD is ever guessed.
    async fn follow_search_to_destination(
        &self,
        browser: &Arc<ManagedBrowser>,
        search_origin: &url::Url,
        intent: &mut macro_engine::SemanticIntent,
    ) -> Result<FollowedDestination, AppError> {
        // Which word Stage 2 follows comes from the prompt's own grammar,
        // not a portal list. A prepositional complement names the destination
        // (`… invoices from github` → `github`), so the results page is
        // matched on the site while the artifact noun stays on the intent for
        // the batch gate once the destination loads. Without a complement the
        // direct object *is* the destination (`open amazon for me`), and the
        // noun falls back to the settle probe text.
        //
        // Irregular phrasing that grammar cannot read confidently gets one
        // bounded shot at the fenced parser seam first; an absent or stalled
        // parser simply leaves the slots ungrounded and the follow falls back
        // to probe text, which is the offline path.
        let slots = self.follow_slots(&intent.raw_prompt);
        let noun = macro_engine::search_follow_noun(intent, slots.grammar.site_context.as_deref())
            .to_owned();
        // Journaled so a wrong follow is attributable to the tier that chose
        // the noun. Prompt-derived words only — no URLs, no page text.
        let slot_log = self
            .journal_line(format!(
                "follow_slots: tier={} noun='{noun}'",
                slots.source.as_str()
            ))
            .await;
        let followed = match macro_engine::follow_search_result(browser, search_origin, &noun).await
        {
            Ok(followed) => followed,
            Err(error) => {
                // Journal the evidence (which links the page did offer)
                // before failing, so a miss is diagnosable.
                let _ = self.record(&format!("search_follow_failed: {error}")).await;
                return Err(AppError::WorkflowFailed);
            }
        };
        let mut portal = followed.landed.clone();
        portal.set_path("/");
        portal.set_query(None);
        portal.set_fragment(None);
        // Confinement first: every later snapshot is evaluated against the
        // destination origin, not the search host it came from.
        let previous = browser.reanchor_portal(&followed.landed);
        if let Some(current) = browser.portal_anchor()
            && previous.as_ref() != Some(&current)
        {
            self.journal_line(browser_driver::portal_reanchored_line(
                previous.as_ref(),
                &current,
            ))
            .await;
        }
        *self.session_origin.lock().map_err(|_| AppError::Internal)? = Some(portal.clone());
        intent.entry_url = Some(followed.landed.as_str().to_owned());
        // Host plus path only: a destination URL can carry tokens in its
        // query string, and this line is persisted.
        let line = self
            .journal_line(format!(
                "search_followed: '{}' → {}{}",
                followed.label,
                followed.landed.host_str().unwrap_or("?"),
                followed.landed.path()
            ))
            .await;
        // Slot tier first, then what it landed on: Session Activity reads the
        // follow as a decision plus its result.
        Ok(FollowedDestination {
            portal,
            log: format!("{slot_log}\n{line}"),
        })
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
        let (result, _) = self
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
            // Saved replays are already durable: nothing to remember.
            run_id: None,
            route_log: None,
            telemetry_log: None,
        })
    }

    /// Cold-path entry resolution: tab-independent by design. Never inspects
    /// the active tab's URL and never vetoes or filters against it — the raw
    /// prompt is the only input, so starting on `google.com` or `about:blank`
    /// cannot block cross-domain pre-navigation. Shared by the single and
    /// batch dispatch lanes. Records host plus path — never query strings —
    /// to `session_events` and returns the exact log line so dispatch
    /// outcomes can surface it in the Session Activity UI immediately, plus
    /// which tier answered. An entry that is already present proposes
    /// nothing and surfaces nothing.
    ///
    /// Proven destinations are not resolved here at all: they live in saved
    /// playbooks, which `resolve_command` matches upstream (tier 1) before
    /// dispatch ever produces an ephemeral intent. This function only runs
    /// for prompts no stored workflow claimed.
    /// Load the user's saved site shortcuts for the direct-open ladder.
    /// `None` when the store is unavailable — the caller journals the skip
    /// and yields a neutral proposal instead of a half-built route.
    async fn load_shortcut_map(&self) -> Option<std::collections::HashMap<String, String>> {
        let playbooks = self.playbooks().await.ok()?;
        let rows = playbooks.list_site_shortcuts().await.ok()?;
        Some(rows.into_iter().map(|s| (s.name, s.url)).collect())
    }

    /// Journal `line` and yield a neutral proposal: the ladder could not
    /// run, so dispatch falls back to its honest "not runnable" path.
    async fn proposal_skipped(&self, line: &str) -> ProposedEntry {
        let _ = self.record(line).await;
        ProposedEntry {
            log: Some(line.to_owned()),
            ..ProposedEntry::default()
        }
    }

    /// Landing detection for the learning loop: journal the "Save as
    /// Shortcut" offer for a domain-grounded proposal and return the line so
    /// dispatch can surface it on the outcome's telemetry log (journal lines
    /// alone don't reach the Action Thread). Call only after navigation to
    /// the grounded URL completed — a proposal that failed to land must not
    /// offer anything, so every call site sits behind a successful
    /// pre-navigation `?`. `None` when the proposal carried no grounder hit.
    async fn offer_shortcut_after_landing(&self, proposed: &ProposedEntry) -> Option<String> {
        let hit = proposed.grounder_hit.as_ref()?;
        let line = format!(
            "shortcut_offer: '{}' → {} · save to skip grounding next time",
            hit.site, hit.url
        );
        let _ = self.record(&line).await;
        Some(line)
    }

    /// Prepend a post-landing shortcut offer (when any) to a telemetry log.
    /// Journal order is proposal → landing → everything after; the
    /// outcome's telemetry mirrors it so the thread reads chronologically.
    fn with_shortcut_offer(offer: Option<String>, telemetry: Option<String>) -> Option<String> {
        match (offer, telemetry) {
            (Some(offer), Some(rest)) => Some(format!("{offer}\n{rest}")),
            (Some(offer), None) => Some(offer),
            (None, rest) => rest,
        }
    }

    /// Fail a direct-open miss with ask-and-learn guidance. `Some(err)`
    /// when the ladder ran and found nothing; `None` otherwise. When the
    /// grounder was never configured, the guidance says so — the fix is
    /// setup, not rephrasing the prompt.
    fn direct_open_miss_error(proposed: &ProposedEntry) -> Option<AppError> {
        if proposed.direct_open_miss {
            let message = if proposed.grounder_configured {
                "I couldn't find a destination for that. Try the full domain (for example 'open amazon.in'), or save a site shortcut and try again."
            } else {
                "I couldn't find a destination for that. The site-name grounder isn't configured — set CLINCH_GROUNDER_PROVIDER to groq or ollama and try again, or use the full domain (for example 'open amazon.in') or a saved site shortcut."
            };
            Some(AppError::InvalidInput(message))
        } else {
            None
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn propose_entry_url(
        &self,
        prompt: &str,
        intent: &mut macro_engine::SemanticIntent,
        connected_origin: Option<&url::Url>,
    ) -> ProposedEntry {
        if intent.entry_url.is_some() {
            return ProposedEntry::default();
        }
        // Production wires no account directory (no stored credential backs
        // one) and no URL adapter. The direct-open ladder reads the user's
        // saved shortcuts (loaded once per dispatch — the engine stays
        // runtime-agnostic) and the optional structured site directory
        // (`CLINCH_BRAVE_API_KEY`); anything the ladder cannot ground stays
        // a miss instead of a scraped search page. The intent parser is
        // deliberately absent here: it resolves *slots*, never URLs, so it
        // belongs to `follow_slots` rather than this tier.
        //
        // This function never fails outward: a store or thread failure is
        // journaled and yields a neutral proposal, so dispatch falls back
        // to its honest "not runnable" path instead of a half-built route.
        let Some(shortcuts) = self.load_shortcut_map().await else {
            return self
                .proposal_skipped("route_proposal_skipped: site shortcut store unavailable")
                .await;
        };
        let shortcut_store = orchestration_engine::InMemoryShortcuts::new(shortcuts);
        let site_search = orchestration_engine::BraveSiteSearch::from_env();
        // Fenced domain grounder: an LLM-backed adapter when
        // `CLINCH_GROUNDER_PROVIDER` selects one — `groq` reads its key from
        // `GROQ_API_KEY`, `ollama` talks to the local daemon — and the
        // declining stub otherwise. Unconfigured or offline stays a normal
        // outcome: the ladder degrades to the honest miss.
        let live_grounder = orchestration_engine::LlmDomainGrounder::from_env();
        let stub_grounder = orchestration_engine::StubDomainGrounder;
        let region_hint = orchestration_engine::system_region_hint();
        // Captured before the blocking-thread move below: the miss line
        // names which ladder rungs were even live, so an unconfigured
        // grounder reads as a setup hint rather than a dead end.
        let grounder_configured = live_grounder.is_some();
        let site_search_configured = site_search.is_some();
        // The directory rung is synchronous network I/O (bounded at ten
        // seconds by the agent config). It runs on a blocking thread so it
        // can never stall the async runtime's workers; everything the
        // closure touches is owned, so the future stays `'static`.
        let prompt_owned = prompt.to_owned();
        let origin_owned = connected_origin.cloned();
        let resolved = tokio::task::spawn_blocking(move || {
            // The live adapter when configured, the declining stub
            // otherwise: the reference is built inside the closure from the
            // moved-in owners, so it cannot outlive them.
            let domain_grounder: &dyn orchestration_engine::DomainGrounder = match &live_grounder {
                Some(grounder) => grounder,
                None => &stub_grounder,
            };
            let ctx = orchestration_engine::ResolutionContext {
                account_dir: None,
                llm: None,
                parser: None,
                shortcuts: Some(&shortcut_store),
                site_search: site_search
                    .as_ref()
                    .map(|client| client as &dyn orchestration_engine::SiteSearchClient),
                domain_grounder: Some(domain_grounder),
                region_hint: region_hint.as_str(),
            };
            let route =
                orchestration_engine::resolve_entry_url(&prompt_owned, origin_owned.as_ref(), &ctx);
            // Sanitized provider failure from the grounder's one call, if it
            // made one and it failed — the ladder itself only reports the
            // miss. Read here, inside the closure, while the concrete
            // adapter is still owned.
            let grounder_error = live_grounder
                .as_ref()
                .and_then(orchestration_engine::LlmDomainGrounder::last_error);
            (route, grounder_error)
        });
        // A panicked blocking thread means the directory rung never ran;
        // journal it and yield a neutral proposal rather than a guess.
        let Ok((resolved, grounder_error)) = resolved.await else {
            return self
                .proposal_skipped("route_proposal_skipped: site directory thread failed")
                .await;
        };
        // Whether the miss (if any) is a direct open the ladder could not
        // ground. The dispatcher turns that into ask-and-learn guidance —
        // "try the full domain or save a site shortcut" — instead of the
        // generic "not runnable".
        let direct_open = orchestration_engine::is_direct_open(
            prompt,
            &orchestration_engine::parse_grammar(prompt, connected_origin),
        );
        if let Some(route) = resolved {
            // Grounded search fallback journals its own line (query string
            // included) so Session Activity shows the template, never a
            // guessed TLD. Dispatcher navigates to the search page and
            // grounds the top result link from the live AX tree.
            if route.source == orchestration_engine::RouteSource::SearchFallback {
                // The decoded `q` value, not the raw query string: the line
                // reads `q='open amazon'`, never `q='q=open+amazon'`.
                let query = route
                    .url
                    .query_pairs()
                    .find(|(key, _)| key == "q")
                    .map(|(_, value)| value.into_owned())
                    .unwrap_or_default();
                let line = format!(
                    "route_fallback: search q='{query}' · url={}",
                    route.url.as_str()
                );
                let _ = self.record(&line).await;
                intent.entry_url = Some(route.url.as_str().to_owned());
                return ProposedEntry {
                    log: Some(line),
                    source: Some(route.source),
                    direct_open_miss: false,
                    ..ProposedEntry::default()
                };
            }
            let line = format!(
                "route_proposed:{}{} · source: {:?}",
                route.url.host_str().unwrap_or("?"),
                route.url.path(),
                route.source
            );
            let _ = self.record(&line).await;
            intent.entry_url = Some(route.url.as_str().to_owned());
            // Learning loop: a domain-grounded hit is a shortcut candidate,
            // but the offer is journaled only after navigation completes
            // (`offer_shortcut_after_landing`) — a proposal that never lands
            // must not offer anything. The hit rides the proposal so every
            // dispatch lane can reach it without re-parsing the prompt.
            let grounder_hit = if route.source == orchestration_engine::RouteSource::DomainGrounded
            {
                let site = orchestration_engine::parse_grammar(prompt, connected_origin)
                    .target_noun
                    .unwrap_or_default();
                (!site.is_empty()).then(|| GrounderHit {
                    site,
                    url: route.url.as_str().to_owned(),
                })
            } else {
                None
            };
            ProposedEntry {
                log: Some(line),
                source: Some(route.source),
                direct_open_miss: false,
                grounder_configured,
                grounder_hit,
            }
        } else {
            let line = if direct_open {
                // Ask-and-learn: the miss names the way out — and which
                // rungs were even live. An unconfigured grounder is a setup
                // problem ("set CLINCH_GROUNDER_PROVIDER"); a configured one
                // that errored names the provider failure (e.g. a retired
                // model); a clean decline is a genuine miss. Without this,
                // all three look identical and the failure is undebuggable.
                let grounder_state = if !grounder_configured {
                    "grounder unconfigured (set CLINCH_GROUNDER_PROVIDER=groq or =ollama)"
                        .to_owned()
                } else if let Some(err) = grounder_error {
                    format!("grounder error: {err}")
                } else {
                    "grounder attempted, no domain returned".to_owned()
                };
                let directory_state = if site_search_configured {
                    "directory attempted, no match"
                } else {
                    "directory unconfigured"
                };
                format!(
                    "route_resolution_miss: prompt='{prompt}' · {grounder_state} · {directory_state} — try the full domain (open amazon.in) or save a site shortcut"
                )
            } else {
                format!("route_resolution_miss: prompt='{prompt}'")
            };
            let _ = self.record(&line).await;
            ProposedEntry {
                log: Some(line),
                source: None,
                direct_open_miss: direct_open,
                grounder_configured,
                ..ProposedEntry::default()
            }
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

    /// Cross-domain pre-navigation plus the post-landing shortcut offer in
    /// one step: pre-navigation's `?` is the landing gate, so the returned
    /// offer line (if any) is proof the grounded URL actually landed — a
    /// failed navigation never reaches the journal call.
    async fn pre_navigate_with_offer(
        &self,
        browser: &Arc<ManagedBrowser>,
        steps: &[playbook_store::Step],
        proposed: &ProposedEntry,
    ) -> Result<Option<String>, AppError> {
        Self::pre_navigate_to_step(browser, steps).await?;
        Ok(self.offer_shortcut_after_landing(proposed).await)
    }

    /// Everything the batch lane needs after its preamble: the attached
    /// browser, the run's journal handles, the proposed route's log line,
    /// and the post-landing shortcut offer (when the ladder grounded one).
    async fn begin_batch_dispatch(
        &self,
        portal: &url::Url,
        prompt: &str,
        intent: &mut macro_engine::SemanticIntent,
    ) -> Result<BatchPreamble, AppError> {
        // Ephemeral entry point (command-bar Run): propose the route at the
        // top, before any macro task step exists. A direct-open miss fails
        // before the batch run begins rather than snapshotting the connected
        // portal for a destination the prompt never named.
        let proposed = self.propose_entry_url(prompt, intent, Some(portal)).await;
        if let Some(err) = Self::direct_open_miss_error(&proposed) {
            return Err(err);
        }
        let route_log = proposed.log.clone();
        let (browser, run_id, journal_id, journal) = self.begin_batch_run(portal).await?;
        let name = orchestration_engine::ephemeral_name(prompt);
        // The proposed route lives on both `intent.entry_url` and step 1, so
        // pre-navigation provably runs from `step.entry_url`.
        let steps = vec![playbook_store::Step::Semantic {
            intent: intent.clone(),
        }];
        // Pre-navigation's `?` is the landing gate for the offer below.
        let shortcut_offer = self
            .pre_navigate_with_offer(&browser, &steps, &proposed)
            .await?;
        Ok(BatchPreamble {
            browser,
            run_id,
            journal_id,
            journal,
            name,
            steps,
            route_log,
            shortcut_offer,
        })
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
        // A direct-open miss fails here with guidance rather than running
        // the intent against the connected portal, which is not what the
        // prompt asked for.
        let proposed = self
            .propose_entry_url(&prompt, &mut intent, Some(&portal))
            .await;
        if let Some(err) = Self::direct_open_miss_error(&proposed) {
            return Err(err);
        }
        let route_log = proposed.log.clone();
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
        // never snapshotted. Background: an ad-hoc run must never open a
        // window, only reuse whatever session is already attached.
        let browser = self.browser(BrowserIntent::Background).await?;
        let shortcut_offer = self
            .pre_navigate_with_offer(&browser, &playbook.steps, &proposed)
            .await?;
        let telemetry_log = self.reanchor_to_entry(&browser, &playbook.steps).await;
        let telemetry_log = Self::with_shortcut_offer(shortcut_offer, telemetry_log);
        // A pure direct open ends at the landing: the navigation IS the
        // task. Running the semantic step afterward would fail on zero
        // candidates — there is nothing to click on a bare "open X" — so
        // complete here with no steps. (Reaching this line means the
        // pre-navigation `?` above succeeded, so the landing is confirmed.)
        // Completed pure opens are deliberately not remembered: the
        // consent-gated shortcut card, not a playbook, is the persistence
        // mechanism for direct opens.
        if orchestration_engine::is_direct_open(
            &prompt,
            &orchestration_engine::parse_grammar(&prompt, Some(&portal)),
        ) {
            return Ok(DispatchOutcome {
                kind: "ephemeral",
                name,
                result: orchestration_engine::SequenceOutcome {
                    completed_steps: 0,
                    total_steps: 0,
                    status: orchestration_engine::SequenceStatus::Completed,
                    stopped_at: None,
                },
                steps: Vec::new(),
                run_id: None,
                route_log,
                telemetry_log,
            });
        }
        self.verify_bridge_auth(&portal).await?;
        let (result, journal_id) = self
            .run_steps(
                portal.clone(),
                &playbook.steps,
                RunScope {
                    kind: playbook_store::RunKind::Ephemeral,
                    playbook_id: None,
                },
                emit,
            )
            .await?;
        if result.status == orchestration_engine::SequenceStatus::Completed {
            self.remember_completed_run(&journal_id, &portal, &playbook.steps, &prompt);
        }
        // The registry key travels only when the run was remembered, so the
        // Save button can offer one-click persistence without re-asking the
        // portal or round-tripping steps through the client.
        let run_id = (result.status == orchestration_engine::SequenceStatus::Completed)
            .then(|| journal_id.clone());
        Ok(DispatchOutcome {
            kind: "ephemeral",
            name,
            result,
            steps: playbook.steps.clone(),
            run_id,
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
        let BatchPreamble {
            browser,
            run_id,
            journal_id,
            journal,
            name,
            steps,
            route_log,
            shortcut_offer,
        } = self
            .begin_batch_dispatch(&portal, &prompt, &mut intent)
            .await?;
        self.verify_bridge_auth(&portal).await?;
        let events = std::sync::Mutex::new(&mut emit);
        // Read-only snapshot first: the approval names the real candidate
        // count, and empty fields fail closed before any gate is raised.
        // A single entry-URL retry covers wrong-page starts: navigate once,
        // re-snapshot once, then stop. No approval gate ever opens on zero
        // candidates.
        let (candidates, snapshot_telemetry) = self
            .snapshot_anchored_batch_candidates(&browser, &portal, &intent, &steps)
            .await;
        let telemetry_log = Self::with_shortcut_offer(shortcut_offer, snapshot_telemetry);
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
                &journal_id,
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
                self.remember_completed_run(&journal_id, &portal, &steps, &prompt);
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
                Self::emit_batch_blocked(&events, run_id);
                Ok(outcome(
                    orchestration_engine::SequenceStatus::Failed,
                    clicks_completed,
                    Some(failed_candidate_index),
                ))
            }
            Err(_) => {
                Self::finish_batch_run(journal.as_ref(), &journal_id, "failed", 0).await;
                Self::emit_batch_blocked(&events, run_id);
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
        let browser = self.browser(BrowserIntent::Background).await?;
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

    /// Gate copy for one batch approval: how many controls, of what role,
    /// for which request, and — when an entry was resolved — where. Shared
    /// by the ad-hoc batch lane and saved plural replays so an approval reads
    /// identically no matter which lane raised it.
    ///
    /// Host plus path only: query strings can carry tokens and never reach
    /// the summary.
    fn batch_summary(count: usize, intent: &macro_engine::SemanticIntent, subject: &str) -> String {
        let mut summary = format!(
            "Batch click: {count} {} controls for {subject}",
            intent.role
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
        summary
    }

    /// Gate content for one intent-consent request, batch or single.
    ///
    /// A plural request itemizes every resolved control so the card can list
    /// what is about to be clicked; a single-target request carries none,
    /// because the intent already names its one target. Saved replays have no
    /// live prompt, so the subject falls back to the phrasing the intent was
    /// learned from, then to its label query.
    fn intent_approval_content(approval: &orchestration_engine::IntentApproval) -> ApprovalContent {
        let intent = &approval.intent;
        if !approval.is_batch() {
            return ApprovalContent {
                kind: "intent",
                summary: format!("{} · {}", intent.role, intent.label_query),
                candidates: Vec::new(),
            };
        }
        let subject = if intent.raw_prompt.trim().is_empty() {
            intent.label_query.as_str()
        } else {
            intent.raw_prompt.as_str()
        };
        ApprovalContent {
            kind: "intent",
            summary: Self::batch_summary(approval.candidates.len(), intent, subject),
            candidates: candidate_previews(&approval.candidates),
        }
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
        self.approve_playbook_step(
            run_id,
            0,
            1,
            ApprovalContent {
                kind: "intent",
                summary: Self::batch_summary(candidates.len(), intent, prompt),
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

    /// Terminal Blocked phase for batch runs that never completed: denied,
    /// drift-halted, and failed executions all park the progress UI the
    /// same way.
    fn emit_batch_blocked(
        events: &std::sync::Mutex<&mut (impl FnMut(PlaybookEvent) + Send)>,
        run_id: u64,
    ) {
        Self::emit_batch_phase(events, run_id, orchestration_engine::SequencePhase::Blocked);
    }

    /// Terminal batch outcome: one ephemeral step carrying N actions, plus
    /// the route-proposal and snapshot-telemetry lines for immediate
    /// Session Activity render.
    // Eight plain data params on a pure value constructor: bundling would
    // obscure the IPC-mapped fields for no coupling gain.
    #[allow(clippy::too_many_arguments)]
    fn batch_outcome(
        name: String,
        steps: Vec<playbook_store::Step>,
        status: orchestration_engine::SequenceStatus,
        completed: usize,
        stopped: Option<usize>,
        journal_id: &str,
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
            // Only completed batches sit in the registry; every other
            // terminal state carries no key, so the UI offers no save.
            run_id: (status == orchestration_engine::SequenceStatus::Completed)
                .then(|| journal_id.to_owned()),
            route_log,
            telemetry_log,
        }
    }

    /// Shared run machinery for stored and ephemeral playbooks: session
    /// contract, semaphore, headed browser, per-run output dir, approval
    /// gates, and progress streaming. Returns the terminal outcome plus the
    /// journal id, so ephemeral lanes can key completed runs for later
    /// persistence.
    async fn run_steps(
        &self,
        portal: url::Url,
        steps: &[playbook_store::Step],
        scope: RunScope,
        mut emit: impl FnMut(PlaybookEvent) + Send,
    ) -> Result<(orchestration_engine::SequenceOutcome, String), AppError> {
        let connected = self
            .session_origin
            .lock()
            .map_err(|_| AppError::Internal)?
            .clone();
        if connected.is_none_or(|url| url.origin() != portal.origin()) {
            return Err(AppError::SessionRequired);
        }
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        let browser = self.browser(BrowserIntent::Background).await?;
        // Saved cross-domain playbooks replay from their entry route, not
        // the connected portal: navigate first (no-op when already there),
        // then bind confinement to the intentional destination so snapshots
        // evaluate the entry origin. Legacy steps without entries skip both.
        Self::pre_navigate_to_step(&browser, steps).await?;
        let _ = self.reanchor_to_entry(&browser, steps).await;
        let run_id = self.next_playbook_run.fetch_add(1, Ordering::Relaxed);
        let output = self
            .data
            .join(DOWNLOADS_DIR)
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
            |index, approval| {
                // Plural saved steps arrive with their resolved controls
                // attached, so the gate itemizes exactly what the batch will
                // click. Single-target steps carry none and read as before.
                self.approve_playbook_step(
                    run_id,
                    index,
                    total_steps,
                    Self::intent_approval_content(&approval),
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
        Ok((outcome, journal_id))
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
        if let Ok(mut pump) = self.screencast.lock()
            && let Some(handle) = pump.take()
        {
            handle.abort();
        }
        if let Ok(guard) = self.browser.lock()
            && let Some(browser) = guard.as_ref()
        {
            browser.terminate();
        }
    }

    /// Read-only background-context state for the UI preview card. Never
    /// launches a browser as a side effect — the setup hook and polling
    /// views own no process handle.
    pub fn context_status(&self) -> Result<ContextStatus, AppError> {
        let guard = self.browser.lock().map_err(|_| AppError::Internal)?;
        Ok(match guard.as_ref() {
            Some(browser) => ContextStatus {
                attached: true,
                headless: browser.is_headless(),
            },
            None => ContextStatus {
                attached: false,
                headless: true,
            },
        })
    }

    /// Lazily attach the app-owned background Chromium (headless: no OS
    /// window, dedicated Clinch profile) and stream its viewport into
    /// `emit` until released, retaken, or re-acquired. Reuses the live
    /// session when one is already attached. Nothing launches on startup
    /// or on status reads — only dispatch, sync flows, and this call
    /// attach. Fails closed when Chromium cannot start.
    pub async fn acquire_context(
        &self,
        emit: impl Fn(browser_driver::ScreencastFrame) + Send + 'static,
    ) -> Result<ContextStatus, AppError> {
        let browser = self.browser(BrowserIntent::Background).await?;
        browser
            .start_screencast()
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        let mut frames = browser
            .screencast_frames()
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        if let Ok(mut pump) = self.screencast.lock() {
            if let Some(handle) = pump.take() {
                handle.abort();
            }
            let forwarding = browser.clone();
            *pump = Some(tokio::spawn(async move {
                while let Some(event) = frames.next().await {
                    let frame = browser_driver::ScreencastFrame {
                        data: String::from(event.data.clone()),
                        session_id: event.session_id,
                    };
                    emit(frame);
                    let _ = forwarding.ack_screencast_frame(event.session_id).await;
                }
                let _ = forwarding.stop_screencast().await;
            }));
        }
        Ok(ContextStatus {
            attached: true,
            headless: browser.is_headless(),
        })
    }

    /// Gracefully idle the background context: stop the frame pump, end the
    /// screencast, and shut the browser process down. Session intent
    /// (`session_origin`) is preserved, so the next acquire re-attaches
    /// lazily. Never fails an already-idle context.
    pub async fn release_context(&self) -> Result<(), AppError> {
        if let Ok(mut pump) = self.screencast.lock()
            && let Some(handle) = pump.take()
        {
            handle.abort();
        }
        let browser = self.browser.lock().map_err(|_| AppError::Internal)?.take();
        if let Some(browser) = browser {
            let _ = browser.stop_screencast().await;
            browser
                .shutdown()
                .await
                .map_err(|_| AppError::BrowserUnavailable)?;
        }
        Ok(())
    }

    /// Hand the managed browser to the user: switch to a headed window on
    /// the same profile (cookies preserved by the restart path) so it is
    /// directly interactive. Streaming, if active, keeps running for the
    /// preview card.
    pub async fn take_control(&self) -> Result<ContextStatus, AppError> {
        // The one and only path that may put a window on screen.
        let browser = self.browser(BrowserIntent::Interactive).await?;
        Ok(ContextStatus {
            attached: true,
            headless: browser.is_headless(),
        })
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
pub(crate) mod tests {
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

    /// Serializes every holder of [`ChromiumEnvGuard`].
    ///
    /// `CLINCH_CHROMIUM_PATH` is process-wide, and several tests in this
    /// binary point it at a nonexistent executable to prove a launch fails
    /// closed. Without a lock they interleave: the first guard to drop
    /// restores the variable (usually by removing it) while another test is
    /// still mid-launch, which then finds the real Chromium on the machine
    /// and stops failing. That reads as a flaky assertion far from its cause,
    /// so the mutation is serialized rather than merely documented as safe.
    static CHROMIUM_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Points `CLINCH_CHROMIUM_PATH` at a nonexistent binary for the guard's
    /// lifetime, restoring the prior value on drop.
    ///
    /// Holds [`CHROMIUM_ENV_LOCK`] for its whole lifetime, so exactly one
    /// test at a time observes the bogus path. Shared with the IPC suite in
    /// `crate::tests` so both serialize against the same lock — two guards
    /// with two locks would not be a guard at all.
    pub(crate) struct ChromiumEnvGuard {
        prior: Option<std::ffi::OsString>,
        /// Poisoning is irrelevant here: the lock protects an env var, not an
        /// invariant a panicking test could corrupt.
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    #[allow(unsafe_code)]
    impl ChromiumEnvGuard {
        pub(crate) fn hold_bogus() -> Self {
            let lock = CHROMIUM_ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prior = std::env::var_os("CLINCH_CHROMIUM_PATH");
            // Edition 2024 marks env mutation unsafe (process-wide). Sound
            // because the lock above makes this the only live mutator, and
            // every browser-launching fixture either holds this guard or is
            // `#[ignore]`d.
            unsafe {
                std::env::set_var("CLINCH_CHROMIUM_PATH", "nonexistent-chromium-hermetic-test");
            }
            Self { prior, _lock: lock }
        }
    }

    #[allow(unsafe_code)]
    impl Drop for ChromiumEnvGuard {
        fn drop(&mut self) {
            if let Some(prior) = self.prior.take() {
                unsafe {
                    std::env::set_var("CLINCH_CHROMIUM_PATH", prior);
                }
            } else {
                unsafe {
                    std::env::remove_var("CLINCH_CHROMIUM_PATH");
                }
            }
        }
    }

    #[test]
    fn background_acquisition_never_requests_a_window() {
        // The window-visibility contract, provable without Chromium.
        // 1. Background launches headless; only interactive launches headed.
        assert!(BrowserIntent::Background.launch_options().headless);
        assert!(!BrowserIntent::Interactive.launch_options().headless);
        // 2. Dormant: both intents launch, each under its own mode.
        assert_eq!(
            acquire_action(BrowserIntent::Background, None),
            AcquireAction::Launch
        );
        assert_eq!(
            acquire_action(BrowserIntent::Interactive, None),
            AcquireAction::Launch
        );
        // 3. Background NEVER restarts, in either direction: it cannot
        //    promote a headless context into a window, and it cannot demote
        //    a window the user opened with Take Control.
        for attached in [true, false] {
            assert_eq!(
                acquire_action(BrowserIntent::Background, Some(attached)),
                AcquireAction::Reuse,
                "background must reuse an attached session (headless={attached})"
            );
        }
        // 4. Interactive restarts only when the live session has no window,
        //    and reuses an already-visible one.
        assert_eq!(
            acquire_action(BrowserIntent::Interactive, Some(true)),
            AcquireAction::Restart
        );
        assert_eq!(
            acquire_action(BrowserIntent::Interactive, Some(false)),
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
            service.acquire_context(|_| {}).await,
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
    async fn test_saved_playbook_plural_batch_execution() -> Result<(), Box<dyn std::error::Error>>
    {
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
        let single = AppService::intent_approval_content(
            &orchestration_engine::IntentApproval::single(single),
        );
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
    async fn propose_entry_grounds_search_with_no_static_route_table()
    -> Result<(), Box<dyn std::error::Error>> {
        // No browser: the tiered proposer reads the shortcut store (empty
        // here) and otherwise resolves purely, journaling through `record`,
        // which fails open without observers.
        //
        // With the curated `(portal, class)` table deleted, a portal-shaped
        // prompt no longer resolves to an invented deep link. It advances to
        // the fixed search template and owes a Stage-2 follow, which grounds
        // the real destination from a live click. Proven destinations come
        // from saved playbooks (tier 1) instead, resolved before dispatch.
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
        assert_eq!(
            intent.entry_url.as_deref(),
            Some("https://www.google.com/search?q=download+all+invoices+from+github")
        );
        assert!(proposed.needs_search_follow());
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
        // Filler (`the`) is stripped from the query by sanitization.
        assert_eq!(
            other.entry_url.as_deref(),
            Some("https://www.google.com/search?q=find+dashboard")
        );
        // A search entry still owes a Stage-2 follow before it can run.
        assert!(proposed.needs_search_follow());
        assert!(
            proposed
                .log
                .as_deref()
                .is_some_and(|line| line.starts_with("route_fallback: search"))
        );
        // An entry already present (a saved playbook's own route) proposes
        // nothing and is never overwritten by a search template.
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
        assert!(!proposed.needs_search_follow());
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
        assert!(!proposed.needs_search_follow(), "not a search page");
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
    async fn direct_open_miss_names_why_the_ladder_missed() -> Result<(), Box<dyn std::error::Error>>
    {
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
        // Explicit propagation: the proposed route lands on both the intent
        // and step 1 of the ephemeral task. With no static route table the
        // proposal is the grounded search template; Stage 2 rewrites
        // `entry_url` to the landed destination before the batch runs.
        let expected = "https://www.google.com/search?q=download+all+invoices+from+github";
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
        // Session Activity carries the proposal.
        let pool = service.database().await.map_err(|_| "database")?;
        let rows: Vec<(String,)> = sqlx::query_as("SELECT outcome FROM session_events")
            .fetch_all(pool)
            .await
            .map_err(|_| "events")?;
        assert!(
            rows.iter()
                .any(|(outcome,)| outcome.starts_with("route_fallback: search")),
            "route_fallback logged, got {rows:?}"
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
        // Ad-hoc prompts resolve through grounded search now that no static
        // route table exists; Stage 2 replaces this with the landed
        // destination at run time. Grammar slots survive persistence: the
        // artifact noun anchors the batch, the `github` complement was the
        // destination cue and is not the batch anchor.
        assert_eq!(
            intent.entry_url.as_deref(),
            Some("https://www.google.com/search?q=download+all+invoices+from+github")
        );
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
    async fn test_search_fallback_auto_clicks_first_result()
    -> Result<(), Box<dyn std::error::Error>> {
        use browser_driver::test_utils::fake_cdp::{
            FakeCdpClient, FakeCdpServer, ScriptStep, search_results_tree,
        };
        use std::time::Duration;
        // End-to-end Stage 1 → Stage 2 over scripted CDP traffic, with no
        // Chromium binary: search landing → candidate link selection →
        // re-anchored destination navigation.
        let dir = tempfile::tempdir()?;
        let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
        service.initialize().await.map_err(|_| "initialize")?;

        // Stage 1: the prompt resolves to the search template, not a guessed
        // TLD, and reports that a follow is still owed. ("find amazon", not
        // "open amazon for me": a bare direct open is a ladder miss now —
        // see `direct_open_miss_is_guidance_not_search` — so the
        // search-and-follow path is exercised with a non-direct prompt.)
        let prompt = "find amazon";
        let Some(orchestration_engine::CommandMatch::Ephemeral { mut intent }) =
            orchestration_engine::resolve_command(
                prompt,
                url::Url::parse("https://www.google.com/").ok().as_ref(),
                &[],
            )
        else {
            panic!("ad-hoc prompt resolves ephemeral");
        };
        let proposed = service.propose_entry_url(prompt, &mut intent, None).await;
        assert!(proposed.needs_search_follow(), "search tier answered");
        let search_entry = intent.entry_url.clone().ok_or("search entry")?;
        assert_eq!(
            search_entry, "https://www.google.com/search?q=find+amazon",
            "sanitized query, fixed host"
        );
        for guess in ["amazon.com", "amazon.in"] {
            assert!(!search_entry.contains(guess), "no TLD guessing: {guess}");
        }

        // Stage 2 selection, over the real AX tree the results page returns.
        let fake = FakeCdpServer::start(vec![ScriptStep::reply(
            "Accessibility.getFullAXTree",
            search_results_tree(),
        )])
        .await
        .map_err(|error| format!("fake server failed to start: {error}"))?;
        // Stage 2's noun comes from the same cascade the live lane uses. This
        // prompt is a crisp direct action, so grammar answers it on the fast
        // path and the parser seam is never consulted.
        let slots = service.follow_slots(&intent.raw_prompt);
        assert_eq!(
            slots.source,
            orchestration_engine::SlotSource::GrammarFastPath
        );
        let noun = macro_engine::search_follow_noun(&intent, slots.grammar.site_context.as_deref())
            .to_owned();
        assert_eq!(noun, "amazon", "target noun drives the follow");
        let run = async {
            let mut client = FakeCdpClient::connect(fake.url()).await?;
            let tree = client
                .call("Accessibility.getFullAXTree", serde_json::json!({}))
                .await?;
            let nodes: Vec<browser_driver::AxNode> =
                serde_json::from_value(tree.get("nodes").cloned().unwrap_or_default())
                    .map_err(|error| format!("bad tree: {error}"))?;
            let elements = browser_driver::interactive_elements(&nodes);
            let picked = macro_engine::select_search_result(&elements, &noun)
                .ok_or("a result must be selected")?;
            // The engine's own nav links come first in document order and
            // also say "Amazon": the landmark gate must skip them.
            assert_eq!(picked.name, "Amazon.in - Online Shopping");
            assert_eq!(picked.backend_node_id, 2);
            assert!(picked.landmark.is_none(), "never page chrome");
            // The non-matching competitor ahead of it is skipped by the noun
            // gate, so this is not merely "first organic link".
            assert!(
                elements
                    .iter()
                    .any(|element| element.name == "Flipkart Online Shopping"),
                "competitor present but not chosen"
            );
            // An absent noun fails closed instead of clicking something.
            assert!(macro_engine::select_search_result(&elements, "nonexistentbrand").is_none());
            assert!(macro_engine::select_search_result(&elements, "").is_none());
            assert_eq!(fake.received_methods(), vec!["Accessibility.getFullAXTree"]);
            assert!(fake.violations().is_empty());
            client.close().await;
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let outcome = tokio::time::timeout(Duration::from_secs(10), run).await;
        fake.shutdown();
        outcome.map_err(|_| "fake CDP roundtrip timed out")??;

        // Stage 2 landing: confinement re-anchors to the observed
        // destination, so the destination page is no longer "drift" even
        // though the run was requested against the search origin.
        let search_origin = url::Url::parse("https://www.google.com/")?;
        let landed = url::Url::parse("https://www.amazon.in/ref=nav_logo")?;
        assert!(
            browser_driver::portal_reanchored_line(Some(&search_origin), &landed)
                .starts_with("portal_reanchored: https://www.google.com/ → "),
            "transition journaled"
        );
        // The run portal becomes the landed origin, query/fragment stripped.
        let mut portal = landed.clone();
        portal.set_path("/");
        portal.set_query(None);
        portal.set_fragment(None);
        assert_eq!(portal.as_str(), "https://www.amazon.in/");
        Ok(())
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
        assert_eq!(
            saved.entry_url.as_deref(),
            Some("https://www.google.com/search?q=download+all+invoices+from+github")
        );
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

    #[tokio::test]
    async fn intent_parser_seam_is_wired_but_declines_by_default()
    -> Result<(), Box<dyn std::error::Error>> {
        // Shipped posture: the seam exists and is consulted, but the default
        // adapter declines, so no model is required, no key is needed, and no
        // prompt leaves the machine. Low-confidence prompts degrade to raw
        // search rather than failing.
        let dir = tempfile::tempdir()?;
        let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
        let irregular = "pull up what I owe on aws";
        let slots = service.follow_slots(irregular);
        assert_eq!(
            slots.source,
            orchestration_engine::SlotSource::Ungrounded,
            "the stub declines, so slots stay ungrounded"
        );
        // Crisp prompts never reach the seam at all.
        assert_eq!(
            service.follow_slots("download invoices from github").source,
            orchestration_engine::SlotSource::GrammarFastPath
        );
        // Swapping in an adapter is the only change needed to light up tier
        // 2B — the plumbing above it is already live.
        let double = std::sync::Arc::new(orchestration_engine::TestDoubleIntentParser::answering(
            orchestration_engine::ParsedSlots {
                action: "link".into(),
                artifact_noun: Some("bill".into()),
                site_context: Some("aws".into()),
            },
        ));
        let wired = AppService::new(dir.path().to_owned(), dir.path().to_owned())
            .with_intent_parser(double.clone());
        let slots = wired.follow_slots(irregular);
        assert_eq!(slots.source, orchestration_engine::SlotSource::IntentParser);
        assert_eq!(slots.grammar.site_context.as_deref(), Some("aws"));
        assert_eq!(double.calls(), 1);
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
        // No connected portal: ad-hoc prompts auto-acquire the browser via
        // the grounded search fallback instead of dying as a miss. With no
        // Chromium present the launch fails closed, but the fallback is
        // already journaled and the session points at the search origin.
        let _chromium = ChromiumEnvGuard::hold_bogus();
        assert!(matches!(
            service
                .dispatch_natural_command("download my report".into(), |_| {})
                .await,
            Err(AppError::BrowserUnavailable)
        ));
        let events = service.test_session_events().await.map_err(|_| "events")?;
        assert!(
            events
                .iter()
                .any(|outcome| outcome.starts_with("route_fallback: search")),
            "route_fallback logged, got {events:?}"
        );
        let connected = service
            .session_origin
            .lock()
            .map_err(|_| "session")?
            .clone();
        assert_eq!(
            connected.as_ref().and_then(|url| url.host_str()),
            Some("www.google.com")
        );
        Ok(())
    }

    #[tokio::test]
    async fn adhoc_search_fallback_never_guesses_tlds() -> Result<(), Box<dyn std::error::Error>> {
        // Hermetic proof: unknown prompts advance to the fixed search
        // template without inventing amazon.com / amazon.in. No browser
        // needed — pure tiered resolution plus journaling. ("find amazon",
        // not "open amazon for me": a bare direct open is a ladder miss —
        // see `direct_open_miss_is_guidance_not_search`.)
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
        // Sanitized query with no guessed amazon TLD.
        assert_eq!(
            intent.entry_url.as_deref(),
            Some("https://www.google.com/search?q=find+amazon")
        );
        assert!(proposed.needs_search_follow());
        let line = proposed.log.ok_or("route line")?;
        assert!(line.starts_with("route_fallback: search"), "got {line:?}");
        // The decoded query reads cleanly — never a doubled `q=q=`.
        assert!(
            line.contains("q='find amazon'"),
            "decoded query journaled, got {line:?}"
        );
        assert!(!line.contains("q='q="), "no doubled prefix, got {line:?}");
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
    async fn adhoc_dispatch_auto_acquires_navigates_and_reanchors()
    -> Result<(), Box<dyn std::error::Error>> {
        // Hermetic ad-hoc proof without a real Chromium binary:
        // - `dispatch_natural_command` with no session auto-acquires (lazy
        //   launch attempted → `BrowserUnavailable` with bogus executable),
        // - the grounded search entry is journaled (`route_fallback`),
        // - the session re-anchors to the new origin (`portal_reanchored`
        //   journaled via the same `journal_line` + `reanchor_portal` path
        //   the live lane uses after `ensure_at_entry_url`).
        // CDP navigation itself needs a real browser (covered by the
        // Chromium-gated fixtures); here we prove the dispatch wiring that
        // precedes and follows it.
        let dir = tempfile::tempdir()?;
        let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
        service.initialize().await.map_err(|_| "initialize")?;
        let _chromium = ChromiumEnvGuard::hold_bogus();
        // A non-direct prompt exercises the search-fallback dispatch wiring;
        // "open amazon for me" would stop earlier as a direct-open miss.
        assert!(matches!(
            service
                .dispatch_natural_command("find amazon".into(), |_| {})
                .await,
            Err(AppError::BrowserUnavailable)
        ));
        let events = service.test_session_events().await.map_err(|_| "events")?;
        assert!(
            events.iter().any(|outcome| outcome
                == "route_fallback: search q='find amazon' · url=https://www.google.com/search?q=find+amazon"),
            "sanitized search fallback journaled, got {events:?}"
        );
        // Stage 2 never ran here (the browser could not launch), so no
        // destination was claimed: the run failed instead of reporting the
        // search page as the completed navigation.
        assert!(
            !events
                .iter()
                .any(|outcome| outcome.starts_with("search_followed:")),
            "no destination claimed without a browser, got {events:?}"
        );
        // Session auto-anchored to the search origin (Portal URL was never
        // required).
        let connected = service
            .session_origin
            .lock()
            .map_err(|_| "session")?
            .clone();
        let connected = connected.ok_or("session auto-set")?;
        assert_eq!(connected.host_str(), Some("www.google.com"));
        // Re-anchor emission uses the driver helper every dynamic
        // navigation runs: journal one transition and prove it lands.
        let previous: Option<url::Url> = None;
        let line = service
            .journal_line(browser_driver::portal_reanchored_line(
                previous.as_ref(),
                &connected,
            ))
            .await;
        assert!(
            line.starts_with("portal_reanchored: none → "),
            "got {line:?}"
        );
        let events = service.test_session_events().await.map_err(|_| "events")?;
        assert!(
            events
                .iter()
                .any(|outcome| outcome.starts_with("portal_reanchored:")),
            "portal_reanchored emitted, got {events:?}"
        );
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

    #[tokio::test]
    async fn shortcut_offer_journaled_only_after_landing() -> Result<(), Box<dyn std::error::Error>>
    {
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
    async fn proposal_alone_never_journals_shortcut_offer() -> Result<(), Box<dyn std::error::Error>>
    {
        // Regression guard for the old behavior (the suggestion was
        // journaled at proposal time): running the ladder alone — search
        // fallback and direct-open miss — must never produce the offer line.
        // Only a post-landing call to `offer_shortcut_after_landing` may.
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
        assert!(proposed.needs_search_follow(), "search fallback proposed");
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
            account_dir: None,
            llm: None,
            parser: None,
            shortcuts: Some(&shortcuts),
            site_search: None,
            domain_grounder: None,
            region_hint: "",
        };
        let resolved = orchestration_engine::resolve_entry_url("open amazon for me", None, &ctx)
            .ok_or("route")?;
        assert_eq!(
            resolved.source,
            orchestration_engine::RouteSource::Shortcut,
            "saved shortcut answers the next run"
        );
        assert_eq!(resolved.url.as_str(), "https://www.amazon.in/");
        Ok(())
    }
}
