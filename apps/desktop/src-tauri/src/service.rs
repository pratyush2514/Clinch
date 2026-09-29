#![deny(unsafe_code)]
use crate::auth::{AuthPanel, ReauthReason, reason_for_signal};
use browser_driver::{Action, AuthState, ChallengeKind, LaunchOptions, ManagedBrowser, WindowMode};
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

/// Verdict word carried by an observed-state caption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedVerdict {
    Completed,
    Failed,
}

/// Values read from the live page after a run ended. Every field is an
/// observation (or `None` when it could not be read), never intent.
#[derive(Debug, Clone, Copy, Default)]
pub struct ObservedState<'a> {
    pub final_url: Option<&'a str>,
    pub page_title: Option<&'a str>,
    /// Label of the control the run acted on, when one was recorded.
    pub acted_on: Option<&'a str>,
    /// Verifier note (completions) or failure reason (failures).
    pub note: Option<&'a str>,
}

/// Deterministic caption of where a run actually ended: the live URL
/// (host + path only — queries and fragments can carry tokens), the page
/// title, the acted-on label and the verifier note or failure reason. A
/// pure function of observed inputs; a missing observation is stated as
/// unread, never filled from intent.
pub fn caption_observed_state(verdict: ObservedVerdict, observed: &ObservedState<'_>) -> String {
    let verdict = match verdict {
        ObservedVerdict::Completed => "completed",
        ObservedVerdict::Failed => "failed",
    };
    let page = match observed.final_url {
        Some(raw) => match url::Url::parse(raw) {
            Ok(parsed) => format!("{}{}", parsed.host_str().unwrap_or("?"), parsed.path()),
            Err(_) => "unparseable url".to_owned(),
        },
        None => "unread".to_owned(),
    };
    let title = observed
        .page_title
        .map(|title| title.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|title| !title.is_empty())
        .map_or_else(
            || "unread".to_owned(),
            |title| format!("'{}'", title.chars().take(120).collect::<String>()),
        );
    let mut caption = format!("observed_outcome: {verdict} · landed {page} · title {title}");
    if let Some(label) = observed.acted_on.filter(|label| !label.is_empty()) {
        caption.push_str(&format!(" · acted on '{label}'"));
    }
    if let Some(note) = observed.note.filter(|note| !note.is_empty()) {
        caption.push_str(&format!(" · {note}"));
    }
    caption
}

/// Honest label for an auth reading that classified nothing: says whether
/// the page could be read at all instead of blaming a probe failure when
/// the page was simply unclassified.
pub fn auth_unknown_label(observed_url: Option<&str>) -> String {
    match observed_url {
        Some(_) => {
            "auth_state_detected: unknown · page read, no logged-out markers matched".to_owned()
        }
        None => "auth_state_detected: unknown · no readable page to probe".to_owned(),
    }
}

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
    /// A run failed. Carries the run's recent journal lines (`session_events`)
    /// because failed runs return `Err` — the UI never sees the telemetry
    /// carried on success outcomes, so without this the miss is invisible
    /// without dumping SQLite by hand. Empty when there is no run context.
    WorkflowFailed(Vec<String>),
    /// Extension bridge reports a logged-out session for the target portal.
    /// Carries the full human-readable message so the UI can surface it.
    AuthenticationRequired(String),
    /// Picking needs the visible managed window: replays run off-screen, so an
    /// overlay armed there can never receive a click.
    PickerUnavailable,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStatus {
    pub ready: bool,
    cookie_import_supported: bool,
    /// Source browser preselected in the UI. Windows defaults to Brave:
    /// Chrome 127+ seals its cookie key with App-Bound encryption (only
    /// Chrome's own elevation service can unwrap it), while Brave keeps a
    /// plain user-DPAPI key that imports instantly.
    default_browser: &'static str,
    /// Exact startup build line journaled as the first `session_events`
    /// row, surfaced so Session Activity names the running binary.
    pub startup_build: String,
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
    pub id: u64,
    title: &'static str,
    description: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStatus {
    running: bool,
    port: u16,
    extensions: usize,
    /// One entry per connected companion (browser label + install id),
    /// oldest first. Drives the card's bridge-status line and the
    /// multi-browser source picker.
    connections: Vec<crate::ws_server::BridgeConnectionInfo>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PickerStatus {
    /// True only with a headed (visible-window) browser connected. Off-screen
    /// replay targets and absent browsers both report false.
    pub ready: bool,
}

/// App-owned background browser context state for the UI preview card.
/// Read-only snapshot: never launches as a side effect.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextStatus {
    /// A managed Chromium session is currently attached.
    pub attached: bool,
    /// The attached session runs without an OS window.
    headless: bool,
    /// How the attached session's window runs. Distinct from `headless`
    /// above: off-screen headed owns real windows (positioned off-monitor
    /// and OS-hidden), so the UI badge can say "headed" honestly while
    /// the no-visible-window gates keep using `headless`.
    window_mode: browser_driver::WindowMode,
}

/// Tauri event carrying one base64 JPEG viewport frame to the preview card.
pub const SCREENCAST_EVENT: &str = "browser-screencast-frame";

/// Tauri event carrying one agent cursor position to the preview card, so
/// the UI can render a visible pointer over the cursorless screencast
/// frames. Payload is [`browser_driver::CursorEvent`].
pub const CURSOR_EVENT: &str = "browser-cursor-moved";

/// Why a managed-browser handle is being acquired.
///
/// The window-visibility contract lives in this type rather than in a bare
/// `headless: bool` at each call site, so "background work never opens an OS
/// window" is checkable by reading the argument instead of tracing a boolean.
///
/// * [`Self::Background`] — dispatch lanes, playbook runs, task replay, and
///   screencast acquisition. Launches off-screen headed (a real Chromium
///   compositor positioned off-monitor and OS-hidden: trusted input events
///   for the action engine, no visible window), and reuses whatever is
///   attached as-is: after an L1 escalation the session may be
///   off-screen headed, which still shows no visible window.
/// * [`Self::Interactive`] — only actions the user asked for by name:
///   manual login, in-app re-authentication, source-profile and bridge sync
///   (each may need a visible login/2FA page), and Take Control.
/// * [`Self::ChallengeEscalation`] — automatic L1 bot-challenge escalation:
///   restarts the session as off-screen headed (a real compositor for
///   Cloudflare's probes, hidden via OS APIs). Never interactive: the
///   window is hidden, not handed to the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserIntent {
    Background,
    Interactive,
    ChallengeEscalation,
}

impl BrowserIntent {
    /// Launch options for a fresh process. Background launches off-screen
    /// headed by construction: no code path can launch it any other way.
    pub fn launch_options(self) -> LaunchOptions {
        match self {
            Self::Interactive => LaunchOptions::interactive(),
            // Off-screen headed, not `--headless=new`: a real headed
            // Chromium (compositor, plugins, screen metrics) positioned
            // off-monitor and OS-hidden, so bot-mitigation probes see a
            // headed browser while the user sees no window. Headless is
            // the most fingerprinted mode; nothing about a background run
            // — or an escalation of one — needs it.
            Self::Background | Self::ChallengeEscalation => LaunchOptions::offscreen_headed(),
        }
    }
}

/// What acquiring a browser should do with the session that is already
/// attached. Separated from the CDP work so the window-visibility invariant
/// is provable without launching Chromium.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcquireAction {
    /// Take the live session exactly as it is.
    Reuse,
    /// Restart the live session under the requested window mode.
    Restart,
    /// Nothing attached: launch a fresh process.
    Launch,
}

/// Decide how to satisfy `intent` given the attached session's window mode
/// (`None` when dormant).
///
/// Background never restarts: it reuses whatever is attached, so a run can
/// neither open a visible window nor close one the user opened. Interactive restarts
/// only when the live session shows no visible window. [`BrowserIntent::ChallengeEscalation`]
/// reuses any attached session (off-screen headed or visible-headed),
/// launching off-screen headed only when dormant (cookies carried in
/// memory by the restart path).
pub fn acquire_action(intent: BrowserIntent, attached: Option<WindowMode>) -> AcquireAction {
    match (intent, attached) {
        (_, None) => AcquireAction::Launch,
        (BrowserIntent::Background, Some(_))
        | (BrowserIntent::Interactive, Some(WindowMode::Headed))
        | (BrowserIntent::ChallengeEscalation, Some(WindowMode::Headed | WindowMode::Offscreen)) => {
            AcquireAction::Reuse
        }
        (BrowserIntent::Interactive, Some(_)) => AcquireAction::Restart,
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
    pub kind: &'static str,
    pub summary: String,
    /// Itemized batch candidates for the gate card. Empty on single-step
    /// approvals. In-memory and IPC-bound to the local desktop UI only —
    /// never written to stdout, log files, or telemetry streams.
    pub candidates: Vec<CandidatePreview>,
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
pub struct ApprovalContent {
    kind: &'static str,
    pub summary: String,
    pub candidates: Vec<CandidatePreview>,
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
pub const DOWNLOADS_DIR: &str = "downloads";

/// Shape resolved batch candidates into gate-card previews in document
/// order. Pure mapping — the live approval path below only forwards it.
pub fn candidate_previews(candidates: &[browser_driver::AxElement]) -> Vec<CandidatePreview> {
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
    pub approval: Option<PlaybookApproval>,
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
/// can render it without polling) plus which tier answered. Every tier names
/// a real destination now: the resolver never proposes a search page, so a
/// proposal either grounds or misses honestly — there is no follow-up click
/// owed to any tier.
#[derive(Clone, Debug, Default)]
pub struct ProposedEntry {
    pub log: Option<String>,
    pub source: Option<orchestration_engine::RouteSource>,
    /// The prompt was a direct open the ladder could not ground: no saved
    /// shortcut, no typed domain, no directory hit. Dispatch turns this
    /// into ask-and-learn guidance instead of the generic "not runnable".
    pub direct_open_miss: bool,
    /// Whether the fenced domain grounder was env-configured when this
    /// proposal was built. Carried so a direct-open miss can say *why* the
    /// ladder could not ground the name: an unconfigured grounder is a
    /// setup problem, a configured one that declined is a genuine miss.
    /// Defaults to false; only the miss path reads it.
    pub grounder_configured: bool,
    /// A domain-grounder hit worth remembering: the site slot the prompt
    /// named and the URL the grounder resolved it to. Carried on the
    /// proposal — never journaled there — so dispatch can offer it as a
    /// shortcut only *after* navigation to the grounded URL completes. A
    /// proposal that never lands must not offer anything.
    pub grounder_hit: Option<GrounderHit>,
}

/// A `RouteSource::DomainGrounded` resolution the Action Thread may offer to
/// keep: the bare site slot plus the exact URL the grounder returned.
#[derive(Clone, Debug, Default)]
pub struct GrounderHit {
    pub site: String,
    pub url: String,
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

/// Which execution lane a resolved command takes. Plural ephemeral intents
/// batch across every matching control; everything else keeps its existing
/// single-step path. Pure routing so the browserless suite can prove the
/// plural prompt never truncates to one click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchLane {
    Saved,
    Single,
    Batch,
}

/// Route one resolved command: the plural flag alone selects the batch
/// lane — saved single-step replays never reach it because the resolver
/// already forces plural prompts ephemeral.
pub fn dispatch_lane(matched: &orchestration_engine::CommandMatch) -> DispatchLane {
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
    pub run_id: Option<String>,
    /// Session-lending registry key for `lend_session`: `Some` whenever
    /// settle registered this run's page URL for a consent tap — a
    /// challenge card or an auth-sync card. Deliberately decoupled from
    /// `run_id`: pure direct opens are not remembered for save-as-workflow
    /// (that would persist an empty graph), but their card tap still needs
    /// a registry key, so settle synthesizes one when `run_id` is `None`.
    /// Additive: older clients ignore unknown keys.
    lend_id: Option<String>,
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
    /// One-shot JPEG viewport (base64) captured when the run settled, so
    /// the thread's final frame is evidence of what the run saw. The live
    /// screencast is armed on the pre-navigation session and does not
    /// survive cross-origin navigation, which left settled direct opens
    /// frozen on the launch placeholder; this capture happens after the
    /// landing, on the live target. `None` when no browser is attached or
    /// capture fails — the run outcome is unaffected. Additive: older
    /// clients ignore unknown keys.
    final_frame: Option<String>,
    /// The page URL when the settled run landed on a bot-mitigation
    /// interstitial (Cloudflare / Turnstile / reCAPTCHA human-verification
    /// gate) instead of the destination. `Some` only for completed runs;
    /// the thread routes the human check to the user via headed takeover
    /// rather than silently completing on a CAPTCHA page. `None` means no
    /// challenge markers were found (or no browser was attached). Additive:
    /// older clients ignore unknown keys.
    challenge: Option<String>,
    /// The page URL when the settled run landed on a clean guest page
    /// while signed out. `Some` only for completed, unchallenged runs that
    /// the auth probe read as logged out; the thread renders the
    /// auth-sync card (sync via the Companion bridge, or Take Control to
    /// log in by hand). `None` means the page read as authenticated or
    /// unclassifiable — today's silent behavior. Additive: older clients
    /// ignore unknown keys.
    auth_url: Option<String>,
    /// The live page's URL when the run settled, for the preview overlay's
    /// browser chrome. Best-effort and fail-open: `None` when no browser
    /// was attached or the target died mid-settle. Additive: older clients
    /// ignore unknown keys.
    final_url: Option<String>,
    /// The live page's document title when the run settled, for the
    /// preview overlay's tab label. Same fail-open contract as `final_url`.
    /// Additive: older clients ignore unknown keys.
    page_title: Option<String>,
}

/// POC health metrics for local testing: playbook runs, macro-replay share
/// derived from task checkpoints, and session-sync outcomes. Everything is
/// computed from local tables; missing tables (a flow that never ran) read
/// as zero rather than failing the command.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PocMetrics {
    pub total_runs: i64,
    pub runs_by_status: BTreeMap<String, i64>,
    pub completed_tasks: i64,
    pub macro_replay_pct: Option<f64>,
    pub sync_imported: i64,
    pub sync_fallback: i64,
    pub sync_by_outcome: BTreeMap<String, i64>,
}

pub struct AppService {
    data: PathBuf,
    home: PathBuf,
    db: OnceCell<sqlx::SqlitePool>,
    browser: Mutex<Option<Arc<ManagedBrowser>>>,
    pub operation: Semaphore,
    approval: Mutex<Option<u64>>,
    next_approval: AtomicU64,
    engine: OnceCell<Engine>,
    pub session_origin: Mutex<Option<url::Url>>,
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
    /// Session-lend registry: lendable runs awaiting a possible consent
    /// tap, oldest first — retained bot-mitigation challenges and clean
    /// guest landings alike. The tap is the consent event — the bridge is
    /// never asked without it — and each run gets exactly one attempt.
    /// Session memory only, capped like `completed_runs`.
    pub session_lends: Mutex<VecDeque<(String, SessionLendState)>>,
    next_playbook_run: AtomicU64,
    /// Monotonic counter backing synthesized session-lending registry keys
    /// (`lend-<n>`) for runs whose `run_id` is `None` — pure direct opens
    /// are deliberately not remembered for save-as-workflow.
    next_lend_id: AtomicU64,
    /// Monotonic counter backing journal run ids (`journal-<n>`), the same
    /// idiom as `next_lend_id`: each dispatch gets a fresh one at entry so
    /// a FAILED card's journal can be scoped to the run that failed.
    next_journal_run: AtomicU64,
    /// The current dispatch's journal run id. Stamped on every `record()`
    /// line so `recent_journal` filters to this run only. `None` outside a
    /// dispatch (boot, session sync) — those rows stay unscoped and never
    /// leak into a FAILED card.
    journal_run: Mutex<Option<String>>,
    /// Fenced structured-intent parser consulted only when the deterministic
    /// grammar parse is not confident, and only for slots — never for URLs,
    /// selectors, or code.
    ///
    /// [`orchestration_engine::LlmIntentParser::from_env`] when a provider is
    /// configured (`CLINCH_GROUNDER_PROVIDER` set to `groq` with
    /// `GROQ_API_KEY`, or to `ollama` for the local daemon); otherwise the
    /// declining [`orchestration_engine::StubIntentParser`]. Unconfigured
    /// keeps the app offline-first with no model dependency, no API key, and
    /// no prompt leaving the machine — behavior identical to the shipped
    /// stub. A configured provider spends one bounded call per
    /// low-confidence prompt, and every answer still passes the slot fence
    /// before it is believed.
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
/// Session-lend state for one run: the page URL (resolved server-side from
/// the settle, never trusted from the frontend) plus whether the single
/// consent-gated attempt already ran and what it returned. Repeat taps
/// return the recorded outcome — no retry loop, no second bridge request.
/// Covers both origins: a retained bot-mitigation challenge and a clean
/// guest landing.
#[derive(Clone)]
pub struct SessionLendState {
    pub page_url: String,
    pub origin: SessionLendOrigin,
    /// The one recorded outcome. Set after the first consent tap so a
    /// repeat tap replays it instead of re-running the bridge exchange.
    pub outcome: Option<LendOutcome>,
}

/// Why a run became lendable: the card it renders, the copy it shows,
/// and what the post-lend re-probe checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionLendOrigin {
    /// Bot-mitigation gate: the challenge card; re-probe checks the gate.
    Challenge,
    /// Clean guest landing: the auth-sync card; re-probe checks auth state.
    GuestLanding,
}

/// Outcome of one session-lend attempt, surfaced on the challenge or
/// auth-sync card. `cleared` means the re-probe found no gate (challenge
/// origin) or an authenticated page (guest-landing origin) after lending;
/// otherwise `reason` carries the static user-facing string and Take
/// Control stays the fallback.
/// One consent tap on a challenge or auth-sync card. `lend_id` is the
/// registry key for the pending lend; `source_connection_id` optionally
/// picks one connected companion (the card's source picker) — `None`
/// broadcasts to every connected companion.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LendRequest {
    pub lend_id: String,
    #[serde(default)]
    pub source_connection_id: Option<u64>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LendOutcome {
    pub cleared: bool,
    pub cookies_lent: usize,
    pub reason: Option<String>,
    pub final_frame: Option<String>,
}

#[derive(Clone, Debug)]
struct CompletedRun {
    id: String,
    origin: url::Url,
    steps: Vec<playbook_store::Step>,
    prompt: String,
}

/// Session cap on persistable completed runs: old entries evict
/// first-in-first-out, so the registry can never grow with the session.
pub const MAX_COMPLETED_RUNS: usize = 32;

/// The resolved landing for a grounded funnel site: what the ladder (or
/// the explicit domain) answered, kept together so the landing arm stays
/// under the argument-count lint.
struct FunnelLanding<'a> {
    site: &'a str,
    source: Option<orchestration_engine::RouteSource>,
    entry: String,
    route_log: Option<String>,
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
            screencast: Mutex::new(None),
            completed_runs: Mutex::new(VecDeque::new()),
            session_lends: Mutex::new(VecDeque::new()),
            next_playbook_run: AtomicU64::new(1),
            next_lend_id: AtomicU64::new(1),
            next_journal_run: AtomicU64::new(1),
            journal_run: Mutex::new(None),
            // The real Tier 2B when configured, the declining stub otherwise:
            // unconfigured keeps today's offline behavior exactly, so no
            // key and no provider means no behavior change.
            intent_parser: orchestration_engine::LlmIntentParser::from_env().map_or_else(
                || {
                    Arc::new(orchestration_engine::StubIntentParser)
                        as Arc<dyn orchestration_engine::IntentParser>
                },
                |parser| Arc::new(parser) as Arc<dyn orchestration_engine::IntentParser>,
            ),
        }
    }

    /// Swap the intent-parser seam. Builder-style so the field stays
    pub async fn database(&self) -> Result<&sqlx::SqlitePool, AppError> {
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
    pub fn test_connect(&self, portal: url::Url) -> Result<(), AppError> {
        *self.session_origin.lock().map_err(|_| AppError::Internal)? = Some(portal);
        Ok(())
    }

    /// Test-only read of the Session Activity backing store.
    pub async fn test_session_events(&self) -> Result<Vec<String>, AppError> {
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
        // requires a browser with no visible window
        // (`EngineError::NoVisibleWindowRequired`), and a
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
            return Err(AppError::WorkflowFailed(Vec::new()));
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
    /// [`BrowserIntent::Background`] never shows the user a window: it
    /// launches off-screen headed (a real headed Chromium positioned
    /// off-monitor and OS-hidden — no `--headless=new`, which is the most
    /// fingerprinted mode) and, when a session already exists, reuses it
    /// exactly as-is instead of restarting. Reuse-as-is matters in both
    /// directions — a background run can neither promote its hidden
    /// context into a visible window nor demote a window the user opened
    /// with Take Control. Only [`BrowserIntent::Interactive`] may restart
    /// a session into a visible window, and only
    /// [`BrowserIntent::ChallengeEscalation`] may restart one into
    /// off-screen headed.
    pub async fn browser(&self, intent: BrowserIntent) -> Result<Arc<ManagedBrowser>, AppError> {
        let existing = self.browser.lock().map_err(|_| AppError::Internal)?.clone();
        match acquire_action(intent, existing.as_ref().map(|live| live.window_mode())) {
            AcquireAction::Reuse => return existing.ok_or(AppError::BrowserUnavailable),
            AcquireAction::Restart => {
                let live = existing.ok_or(AppError::BrowserUnavailable)?;
                return self.restart_browser(&live, intent).await;
            }
            AcquireAction::Launch => {}
        }
        let executable = Self::chromium_executable();
        // A missing binary is the one launch failure with a precise fix:
        // fail here with directions instead of the generic
        // `BrowserUnavailable` copy, which blames the path for every
        // launch failure including transient ones. Bare names
        // (`google-chrome`) resolve through PATH at spawn, so only check
        // paths that name a location.
        if executable.components().count() > 1 && !executable.exists() {
            return Err(AppError::InvalidInput(
                "Chromium executable not found. Set CLINCH_CHROMIUM_PATH to your Chrome or Chromium binary, then retry.",
            ));
        }
        let browser = Arc::new(
            ManagedBrowser::launch_with_options(
                &executable,
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

    /// Stamp a fresh journal run id for the dispatch starting now. Every
    /// `record()` line from here on carries it, so `recent_journal` shows
    /// only this run's lines — never earlier runs', restarts', or
    /// out-of-band journaling. Session-lend ids use the same
    /// counter-synthesis idiom (`lend-<n>`).
    pub fn begin_journal_run(&self) -> Result<(), AppError> {
        let id = format!(
            "journal-{}",
            self.next_journal_run.fetch_add(1, Ordering::Relaxed)
        );
        *self.journal_run.lock().map_err(|_| AppError::Internal)? = Some(id);
        Ok(())
    }

    pub async fn record(&self, outcome: &str) -> Result<(), AppError> {
        let run_id = self
            .journal_run
            .lock()
            .map_err(|_| AppError::Internal)?
            .clone();
        sqlx::query("INSERT INTO session_events (outcome, run_id) VALUES (?, ?)")
            .bind(outcome)
            .bind(run_id)
            .execute(self.database().await?)
            .await
            .map_err(|_| AppError::StorageUnavailable)?;
        Ok(())
    }

    /// Oldest-first recent journal lines for a failed run's error payload,
    /// scoped to the current dispatch's journal run id: `record()` stamps
    /// the id at dispatch entry, so the card carries only the failed run's
    /// lines. The old comment claimed the operation semaphore made the
    /// unscoped newest-N read safe — false across restarts and consecutive
    /// runs, which is exactly what leaked `startup_build` and earlier runs'
    /// lines into FAILED cards. Best-effort: a failed read yields an empty
    /// journal, never a second error.
    pub async fn recent_journal(&self, limit: usize) -> Vec<String> {
        let Ok(database) = self.database().await else {
            return Vec::new();
        };
        let Ok(run_id) = self.journal_run.lock().map(|guard| guard.clone()) else {
            return Vec::new();
        };
        // A `None` id binds NULL, which `= NULL` never matches: journaling
        // outside a dispatch can never surface in a FAILED card.
        let lines: Vec<String> = sqlx::query_scalar(
            "SELECT outcome FROM session_events WHERE run_id = ? ORDER BY rowid DESC LIMIT ?",
        )
        .bind(run_id)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(database)
        .await
        .unwrap_or_default();
        lines.into_iter().rev().collect()
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
        // Off-screen-first note: session establishment itself stays interactive
        // (the user may need to see login/2FA), but every macro replay runs
        // off-screen headed via `run_task()` on a `BrowserIntent::Background` browser.
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
    pub async fn bridge_server_on(
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
                connections: server.connection_infos(),
            },
            None => BridgeStatus {
                running: false,
                port: crate::ws_server::BRIDGE_PORT,
                extensions: 0,
                connections: Vec::new(),
            },
        })
    }

    /// Request the companion extension's live session (cookies +
    /// User-Agent) for `portal` over the loopback bridge. Shared by portal
    /// sync and L1.5 challenge lending: both fire only on an explicit UI
    /// action — the tap is the consent event — never from detection alone.
    /// Bridge failures map to static user-facing strings; raw payloads are
    /// never surfaced. Long-polls up to the bridge response timeout.
    /// `source` selects one companion (the card's source picker); `None`
    /// broadcasts.
    async fn request_bridge_session(
        &self,
        portal: &url::Url,
        source: Option<u64>,
    ) -> Result<crate::ws_server::BridgeSession, AppError> {
        use crate::ws_server::BridgeError;
        self.bridge_server()
            .await?
            .request_sync(portal, crate::ws_server::RESPONSE_TIMEOUT, source)
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
                _ => AppError::WorkflowFailed(Vec::new()),
            })
    }

    /// Zero-touch sync via the Clinch Companion extension: the live session
    /// (cookies + User-Agent) arrives over loopback, is validated against the
    /// portal scope, and is injected into the managed browser — raw values are
    /// never persisted. Long-polls up to the bridge response timeout.
    pub async fn bridge_sync(&self, portal_url: &str) -> Result<SessionStatus, AppError> {
        let portal = session_sync::validate_portal(portal_url)
            .map_err(|_| AppError::InvalidInput("Enter a valid HTTPS portal URL."))?;
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        self.database().await?;
        // Request first: a missing companion fails fast without opening a window.
        let session = self.request_bridge_session(&portal, None).await?;
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

    /// Consent-gated session lending: the rung between automatic handling
    /// and Take Control. The card tap is the consent event — this never
    /// fires from detection alone. Pulls the site's cookies from the Clinch
    /// Companion extension, injects them with the server's expiry
    /// semantics preserved (persistent: "sync once, stay logged in"),
    /// re-navigates, and re-probes.
    ///
    /// One attempt per run: repeat taps return the recorded outcome. The
    /// page URL is resolved server-side from the run registry — a host
    /// string from the frontend is never trusted here.
    /// Remember a lendable run so the card's consent tap can sync a
    /// session. Re-registration refreshes the entry; the registry is
    /// session memory capped like `completed_runs` — oldest evicted first.
    pub fn remember_session_lend(
        &self,
        run_id: String,
        page_url: String,
        origin: SessionLendOrigin,
    ) {
        if let Ok(mut lends) = self.session_lends.lock() {
            if let Some((_, state)) = lends.iter_mut().find(|(id, _)| *id == run_id) {
                state.page_url = page_url;
                state.origin = origin;
                state.outcome = None;
                return;
            }
            lends.push_back((
                run_id,
                SessionLendState {
                    page_url,
                    origin,
                    outcome: None,
                },
            ));
            while lends.len() > MAX_COMPLETED_RUNS {
                lends.pop_front();
            }
        }
    }

    pub async fn lend_session(&self, request: LendRequest) -> Result<LendOutcome, AppError> {
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        let run_id = request.lend_id;
        let source = request.source_connection_id;
        let (page_url, origin) =
            {
                let mut lends = self.session_lends.lock().map_err(|_| AppError::Internal)?;
                let (_, state) = lends.iter_mut().find(|(id, _)| *id == run_id).ok_or(
                    AppError::InvalidInput("This run has no pending session to sync."),
                )?;
                // Second tap: the recorded outcome, no second bridge request.
                if let Some(outcome) = state.outcome.clone() {
                    return Ok(outcome);
                }
                (state.page_url.clone(), state.origin)
            };
        let host = url::Url::parse(&page_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| page_url.clone());
        // The tap is consent: journal before any bridge I/O.
        self.journal_line(format!("session_lend_requested: {host}"))
            .await;
        let outcome = self
            .lend_session_inner(&page_url, &host, origin, source)
            .await;
        if let Ok(mut lends) = self.session_lends.lock()
            && let Some((_, state)) = lends.iter_mut().find(|(id, _)| *id == run_id)
        {
            state.outcome = Some(outcome.clone());
        }
        Ok(outcome)
    }

    /// The lend attempt itself: bridge request, persistent injection,
    /// re-navigation, origin-aware re-probe. Every exit journals exactly
    /// one `session_lent:` / `session_lend_failed:` line with counts and
    /// hosts only — cookie values never appear.
    async fn lend_session_inner(
        &self,
        page_url: &str,
        host: &str,
        origin: SessionLendOrigin,
        source: Option<u64>,
    ) -> LendOutcome {
        let failed = |label: &str| LendOutcome {
            cleared: false,
            cookies_lent: 0,
            reason: Some(label.to_owned()),
            final_frame: None,
        };
        let portal = match url::Url::parse(page_url) {
            Ok(url) if url.scheme() == "https" => url,
            _ => {
                self.journal_line(format!("session_lend_failed: {host} · bad page url"))
                    .await;
                return failed("The synced page URL is not usable.");
            }
        };
        let session = match self.request_bridge_session(&portal, source).await {
            Ok(session) => session,
            Err(AppError::InvalidInput(reason)) => {
                self.journal_line(format!("session_lend_failed: {host} · bridge: {reason}"))
                    .await;
                return failed(reason);
            }
            Err(_) => {
                self.journal_line(format!("session_lend_failed: {host} · bridge unavailable"))
                    .await;
                return failed("The session sync failed unexpectedly.");
            }
        };
        let Some(browser) = self.browser.lock().ok().and_then(|guard| (*guard).clone()) else {
            self.journal_line(format!("session_lend_failed: {host} · browser unavailable"))
                .await;
            return failed("The managed browser is no longer attached.");
        };
        // Identity first: the session must not arrive under a mismatched UA.
        if browser
            .mirror_user_agent(&session.user_agent)
            .await
            .is_err()
        {
            self.journal_line(format!("session_lend_failed: {host} · identity mismatch"))
                .await;
            return failed("The session could not be applied: browser identity mismatch.");
        }
        let count = session.cookies.len();
        // Persistent injection: the server's `expires` is preserved, so
        // Chromium writes the session to the app profile's cookie
        // database — "sync once, stay logged in". No lifetime is invented.
        if browser.inject(&session.cookies).await.is_err() {
            self.journal_line(format!("session_lend_failed: {host} · injection failed"))
                .await;
            return failed("The session could not be injected.");
        }
        if count == 0 {
            // Nothing to lend with: skip re-navigation, keep the card.
            let label = match origin {
                SessionLendOrigin::Challenge => "persistent",
                SessionLendOrigin::GuestLanding => "not synced",
            };
            self.journal_line(format!("session_lent: {host} · 0 cookies · {label}"))
                .await;
            return LendOutcome {
                cleared: false,
                cookies_lent: 0,
                reason: Some("The companion found no usable cookies for this site.".to_owned()),
                final_frame: None,
            };
        }
        if browser.navigate(&portal).await.is_err() {
            self.journal_line(format!(
                "session_lend_failed: {host} · re-navigation failed"
            ))
            .await;
            return failed("The managed browser could not re-open the page.");
        }
        self.finish_session_lend(&browser, host, count, origin)
            .await
    }

    /// Post-injection half of a session lend: re-probe the live page and
    /// record the outcome. Extracted so `lend_session_inner` stays within
    /// the line budget; the two halves share nothing but this call.
    async fn finish_session_lend(
        &self,
        browser: &ManagedBrowser,
        host: &str,
        count: usize,
        origin: SessionLendOrigin,
    ) -> LendOutcome {
        // Origin-aware re-probe: a challenge run checks the gate, a guest
        // landing checks whether the page now reads as authenticated. A
        // stale or rejected session reads as still logged out, and the
        // card simply reappears — the probe self-heals.
        let (cleared, outcome_label, reason) = match origin {
            SessionLendOrigin::Challenge => {
                if browser.challenge_detected().await.is_none() {
                    (true, "cleared", None)
                } else {
                    (
                        false,
                        "persistent",
                        Some("The human check is still there after syncing.".to_owned()),
                    )
                }
            }
            SessionLendOrigin::GuestLanding => {
                if browser.challenge_detected().await.is_some() {
                    (
                        false,
                        "not synced",
                        Some("A human check appeared after syncing.".to_owned()),
                    )
                } else {
                    // Three-way reading, not two: Authenticated confirms the
                    // session took; LoggedOut means the guest prompts are
                    // still there, so the transferred cookies were not a
                    // session; Unknown means the guest prompts vanished but
                    // no signed-in marker is visible — the page moved in the
                    // login direction, so report it as unconfirmed rather
                    // than as "still logged out". (Sites that keep "Log out"
                    // behind an avatar menu never read as Authenticated.)
                    match browser.auth_state().await {
                        AuthState::Authenticated => (true, "synced (persisted)", None),
                        AuthState::LoggedOut => (
                            false,
                            "not synced",
                            Some(
                                "Still logged out after syncing — the companion may not hold a session for this site."
                                    .to_owned(),
                            ),
                        ),
                        AuthState::Unknown => (
                            false,
                            "unconfirmed",
                            Some(format!(
                                "Synced {count} cookies — the preview above is the fresh post-sync state. No signed-in marker is visible, so the session is unconfirmed."
                            )),
                        ),
                    }
                }
            }
        };
        let (frame, _) = self.capture_final_frame().await;
        self.journal_line(format!(
            "session_lent: {host} · {count} cookies · {outcome_label}"
        ))
        .await;
        LendOutcome {
            cleared,
            cookies_lent: count,
            reason,
            final_frame: frame,
        }
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
    /// off-screen targets outright: an invisible overlay can never be clicked,
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

    pub async fn playbooks(&self) -> Result<playbook_store::PlaybookStore, AppError> {
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
    pub fn remember_completed_run(
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
    pub async fn approve_playbook_step(
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
    ///
    /// Multi-action prompts ("log out me from the reddit and re-open the
    /// reddit") split deterministically into verb-led segments and run
    /// sequentially through the same single-prompt dispatch, sharing one
    /// journal run so a FAILED card shows every segment's lines.
    pub async fn dispatch_natural_command(
        &self,
        prompt: String,
        emit: impl FnMut(PlaybookEvent) + Send,
    ) -> Result<DispatchOutcome, AppError> {
        // Fresh journal run id first — before any journaling — so a FAILED
        // card's `recent_journal` shows only this dispatch's lines. This is
        // the single entry point for every dispatch lane (saved, single,
        // batch, app command, ad-hoc auto-acquire, compound): segments of
        // one compound prompt share this run id, keeping their lines
        // attributable to the single user prompt that produced them.
        self.begin_journal_run()?;
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
        let mut emit = emit;
        let saved = self
            .playbooks()
            .await?
            .list_playbooks()
            .await
            .map_err(|_| AppError::StorageUnavailable)?;
        // A saved replay of the FULL prompt wins over splitting: a taught
        // compound workflow is more specific than any dispatch-time
        // decomposition, and splitting a saved prompt would silently change
        // what the user taught. Only a prompt no saved workflow claims is
        // split into sequential segments. (An ephemeral match on the full
        // prompt does not block the split — each segment re-resolves on its
        // own inside `dispatch_one_prompt`.)
        let full_prompt_saved = {
            let connected = self
                .session_origin
                .lock()
                .map_err(|_| AppError::Internal)?
                .clone();
            matches!(
                orchestration_engine::resolve_command(&prompt, connected.as_ref(), &saved),
                Some(orchestration_engine::CommandMatch::Saved { .. })
            )
        };
        if !full_prompt_saved && let Some(segments) = orchestration_engine::split_compound(&prompt)
        {
            return self.dispatch_compound(&saved, segments, &mut emit).await;
        }
        self.dispatch_one_prompt(&saved, prompt, &mut emit).await
    }

    /// Run a multi-action prompt's segments sequentially through
    /// [`Self::dispatch_one_prompt`]. Deterministic: the split itself comes
    /// from the engine's [`orchestration_engine::split_compound`] — no LLM
    /// step planning, no site names in this code.
    ///
    /// Stops at the first segment that errors or does not COMPLETE and
    /// fails the run honestly with the run journal — a partial run never
    /// reports COMPLETED. A segment that needs a portal with none connected
    /// fails exactly like a lone prompt would
    /// ([`AppError::SessionRequired`]), because it *is* dispatched as one.
    async fn dispatch_compound(
        &self,
        saved: &[playbook_store::PlaybookSummary],
        segments: Vec<String>,
        emit: &mut (impl FnMut(PlaybookEvent) + Send),
    ) -> Result<DispatchOutcome, AppError> {
        // `split_compound` promises at least two verb-led segments; an
        // empty vec would be a contract break, not a user error.
        if segments.is_empty() {
            return Err(AppError::Internal);
        }
        let total = segments.len();
        let _ = self.record(&format!("compound: {total} segments")).await;
        // The full decomposition lands in the journal up front, so a FAILED
        // card shows every segment even when segment 1 fails and the run
        // stops there.
        for (index, segment) in segments.iter().enumerate() {
            let _ = self
                .record(&format!(
                    "compound_segment: {}/{} '{segment}'",
                    index + 1,
                    total
                ))
                .await;
        }
        let mut last: Option<DispatchOutcome> = None;
        for segment in segments {
            match self
                .dispatch_one_prompt(saved, segment.clone(), &mut *emit)
                .await
            {
                Err(error) => return Err(error),
                Ok(outcome) => {
                    if outcome.result.status != orchestration_engine::SequenceStatus::Completed {
                        let journal = self.recent_journal(64).await;
                        return Err(AppError::WorkflowFailed(journal));
                    }
                    last = Some(outcome);
                }
            }
        }
        // Every segment completed, so the run settles on the last segment's
        // outcome — already settled by its own lane.
        last.ok_or(AppError::Internal)
    }

    /// Dispatch one prompt down the saved/single/batch lanes (or the ad-hoc
    /// auto-acquire lane with no portal connected). The live portal is
    /// re-read here rather than threaded from the caller, so compound
    /// segments after the first see whatever portal the previous segment
    /// left behind.
    async fn dispatch_one_prompt(
        &self,
        saved: &[playbook_store::PlaybookSummary],
        prompt: String,
        emit: &mut (impl FnMut(PlaybookEvent) + Send),
    ) -> Result<DispatchOutcome, AppError> {
        let connected = self
            .session_origin
            .lock()
            .map_err(|_| AppError::Internal)?
            .clone();
        match orchestration_engine::resolve_command(&prompt, connected.as_ref(), saved) {
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
                    let mut outcome = self
                        .dispatch_single_ephemeral(portal, prompt, intent, emit)
                        .await?;
                    // The connected lane used to return the inner outcome
                    // unsettled — no final frame, no L1 challenge handling.
                    // Every ephemeral lane settles the same way.
                    self.settle_ephemeral_outcome(&mut outcome).await;
                    Ok(outcome)
                }
                DispatchLane::Batch => {
                    let portal = connected.ok_or(AppError::SessionRequired)?;
                    let orchestration_engine::CommandMatch::Ephemeral { intent } = matched else {
                        return Err(AppError::Internal);
                    };
                    let mut outcome = self
                        .dispatch_plural_batch(portal, prompt, intent, emit)
                        .await?;
                    self.settle_ephemeral_outcome(&mut outcome).await;
                    Ok(outcome)
                }
            },
            None if connected.is_none() => {
                // Ad-hoc without a connected portal: resolve against a
                // synthetic search origin so free-form prompts still yield an
                // ephemeral intent, then auto-acquire the browser and run.
                // The Portal URL field stays an optional override, never a
                // prerequisite. (The origin is a grammar-parse hint only —
                // it is never navigated to.)
                let fallback_origin = url::Url::parse("https://www.google.com/").ok();
                let fallback_ref = fallback_origin.as_ref();
                match orchestration_engine::resolve_command(&prompt, fallback_ref, saved) {
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
                let (final_url, page_title) = self.describe_final_page().await;
                let caption = caption_observed_state(
                    ObservedVerdict::Completed,
                    &ObservedState {
                        final_url: final_url.as_deref(),
                        page_title: page_title.as_deref(),
                        acted_on: None,
                        note: None,
                    },
                );
                let _ = self.record(&caption).await;
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
                    lend_id: None,
                    route_log: None,
                    telemetry_log: Some(format!(
                        "{caption}
{line}"
                    )),
                    // Lifecycle commands show no page: no final frame.
                    // Lifecycle commands show no page: no challenge to detect.
                    final_frame: None,
                    challenge: None,
                    auth_url: None,
                    final_url: None,
                    page_title: None,
                })
            }
        }
    }

    /// Ad-hoc dispatch without a prior portal connection: auto-acquire the
    /// browser (lazy launch), resolve the entry via the tiered resolver
    /// (entity → LLM → direct-open ladder), journal the target,
    /// `ensure_at_entry_url`, then `reanchor_portal` before running. The
    /// derived entry origin becomes the run portal, so the Portal URL input
    /// stays an optional override. The resolver never proposes a search
    /// page: an ungroundable prompt misses honestly instead of landing the
    /// browser on a results page.
    /// Cold-start in-page goal for the ad-hoc lane: the prompt names a site
    /// and carries an artifact noun, and the resolver grounded the site
    /// itself. Returns the artifact noun to pursue on the live page. A
    /// missing proposal is never an in-page goal — with no destination
    /// there is nothing to pursue it on. Plural prompts are excluded by the
    /// caller: batching owns those.
    fn cold_in_page_goal(
        prompt: &str,
        source: Option<orchestration_engine::RouteSource>,
    ) -> Option<String> {
        source?;
        let grammar = orchestration_engine::parse_grammar(prompt, None);
        match (grammar.site_context, grammar.artifact_noun) {
            (Some(_), Some(artifact)) => Some(artifact),
            _ => None,
        }
    }

    /// Follow-up fast-path probe: does the prompt name the site the live
    /// browser already shows, with an artifact noun to pursue on it?
    /// Returns the portal (current origin, path reset) plus the noun.
    /// The site token is content-bearing (never a stopword), so the
    /// substring check on the host mirrors `detect_in_page_goal` — no
    /// site list, no grounding call. `None` keeps the normal ladder.
    async fn follow_up_in_page_goal(&self, prompt: &str) -> Option<(url::Url, String)> {
        let grammar = orchestration_engine::parse_grammar(prompt, None);
        let browser = self.browser(BrowserIntent::Background).await.ok()?;
        let current = browser.current_url().await.ok()??;
        let (_, artifact) =
            orchestration_engine::follow_up_on_origin(&grammar, current.host_str())?;
        let mut portal = current;
        portal.set_path("/");
        portal.set_query(None);
        portal.set_fragment(None);
        *self.session_origin.lock().ok()? = Some(portal.clone());
        Some((portal, artifact))
    }

    /// Live portal for the live-page fallback: the current browser URL
    /// with the path reset to the origin, recorded as the session origin
    /// like `follow_up_in_page_goal` does so the run machinery's origin
    /// check passes. `None` when no page is live, so the caller falls
    /// through to the normal flow.
    async fn live_portal(&self) -> Option<url::Url> {
        let browser = self.browser(BrowserIntent::Background).await.ok()?;
        let current = browser.current_url().await.ok()??;
        let mut portal = current;
        portal.set_path("/");
        portal.set_query(None);
        portal.set_fragment(None);
        *self.session_origin.lock().ok()? = Some(portal.clone());
        Some(portal)
    }

    /// Already-on-origin follow-up: journal the skipped ladder, pursue the
    /// artifact noun in-page on the live portal, then settle. Extracted so
    /// `dispatch_adhoc_auto_acquire` stays within the line budget.
    async fn dispatch_follow_up_goal(
        &self,
        portal: url::Url,
        prompt: String,
        noun: String,
        emit: impl FnMut(PlaybookEvent) + Send,
    ) -> Result<DispatchOutcome, AppError> {
        let _ = self
            .record(&format!(
                "route: already_on_origin ({}) · ladder skipped",
                portal.host_str().unwrap_or("?")
            ))
            .await;
        let mut outcome = self
            .dispatch_in_page_goal(portal, prompt, noun, emit)
            .await?;
        self.settle_ephemeral_outcome(&mut outcome).await;
        Ok(outcome)
    }

    /// The site-only grounding ladder for the funnel: the same three rungs
    /// Tier 3 / Tier 3b share
    /// ([`orchestration_engine::resolve_site_entry_url`]) — user shortcut,
    /// composite directory with the plausibility veto, fenced domain
    /// grounder — run over the funnel's site slot alone, on a blocking
    /// thread like the proposal path. Returns the route plus whether the
    /// grounder rung was env-configured, so the miss copy can name the
    /// setup fix. A missing shortcut store degrades to an empty one: stale
    /// data must not veto the rungs below it.
    async fn resolve_site_via_ladder(
        &self,
        site: &str,
    ) -> (Option<orchestration_engine::ResolvedRoute>, bool) {
        let shortcuts = self.load_shortcut_map().await.unwrap_or_default();
        let shortcut_store = orchestration_engine::InMemoryShortcuts::new(shortcuts);
        // Composite directory rung: Brave's sanctioned search API when
        // `CLINCH_BRAVE_API_KEY` is set, DuckDuckGo's keyless HTML endpoint
        // as the zero-config fallback. Backend HTTP in memory only — the
        // browser never sees a search page.
        let site_search = orchestration_engine::ChainedSiteSearch::new();
        // Fenced domain grounder: the live adapter when
        // `CLINCH_GROUNDER_PROVIDER` selects one, the declining stub
        // otherwise. Unconfigured or offline stays a normal outcome: the
        // ladder degrades to the honest miss.
        let live_grounder = orchestration_engine::LlmDomainGrounder::from_env();
        let stub_grounder = orchestration_engine::StubDomainGrounder;
        let region_hint = orchestration_engine::system_region_hint();
        let grounder_configured = live_grounder.is_some();
        // The directory rung is synchronous network I/O; it runs on a
        // blocking thread so it can never stall the async runtime's
        // workers, exactly like the proposal path.
        let site_owned = site.to_owned();
        let route = tokio::task::spawn_blocking(move || {
            let domain_grounder: &dyn orchestration_engine::DomainGrounder = match &live_grounder {
                Some(grounder) => grounder,
                None => &stub_grounder,
            };
            let ctx = orchestration_engine::ResolutionContext {
                llm: None,
                parser: None,
                shortcuts: Some(&shortcut_store),
                site_search: Some(&site_search as &dyn orchestration_engine::SiteSearchClient),
                domain_grounder: Some(domain_grounder),
                region_hint: region_hint.as_str(),
            };
            orchestration_engine::resolve_site_entry_url(&site_owned, &ctx)
        })
        .await
        .ok()
        .flatten();
        (route, grounder_configured)
    }

    /// Funnel entry for ad-hoc open-verb prompts (work item B). Claims only
    /// non-plural prompts whose first content verb is an open-class verb —
    /// the saved lane and the connected batch lane never reach here.
    /// Returns `None` when the funnel declines so the caller keeps the
    /// existing dispatch untouched; `Some` carries the terminal outcome or
    /// error once claimed. `portal` is the portal the caller knows (the
    /// connected portal, or the just-attached live one); `None` keeps the
    /// portal-independent decisions and reports the portal-dependent ones
    /// against the unknown.
    pub async fn dispatch_funnel(
        &self,
        prompt: String,
        intent: &macro_engine::SemanticIntent,
        portal: Option<url::Url>,
        emit: &mut (impl FnMut(PlaybookEvent) + Send),
    ) -> Option<Result<DispatchOutcome, AppError>> {
        let host = portal
            .as_ref()
            .and_then(|portal| portal.host_str())
            .map(str::to_owned);
        let plan = orchestration_engine::funnel_plan(&prompt, intent.is_plural, host.as_deref());
        if matches!(
            plan.decision,
            orchestration_engine::FunnelDecision::Declined
        ) {
            return None;
        }
        for line in &plan.journal_lines {
            let _ = self.record(line).await;
        }
        Some(self.execute_funnel_plan(prompt, plan, portal, emit).await)
    }

    /// Execute a claimed funnel plan. Every arm journals its route; a
    /// claimed prompt is goal-shaped, so the funnel never touches
    /// `SiteSearch` — search is not its answer.
    async fn execute_funnel_plan(
        &self,
        prompt: String,
        plan: orchestration_engine::FunnelPlan,
        portal: Option<url::Url>,
        emit: &mut (impl FnMut(PlaybookEvent) + Send),
    ) -> Result<DispatchOutcome, AppError> {
        // The funnel's parser decides *routing* (the `AskParser` arm
        // below): it names a site slot on the aside-cleaned prompt, which
        // re-enters the site-only ladder — never a search page.
        match plan.decision {
            orchestration_engine::FunnelDecision::Declined => Err(AppError::Internal),
            orchestration_engine::FunnelDecision::AlreadyOnOrigin { site, object } => {
                let portal = portal.ok_or(AppError::Internal)?;
                self.execute_funnel_already_on_origin(prompt, &site, object, portal, emit)
                    .await
            }
            orchestration_engine::FunnelDecision::GroundSite { site, object } => {
                self.execute_funnel_ground_site(prompt, site, object, &plan.cleaned, portal, emit)
                    .await
            }
            orchestration_engine::FunnelDecision::ImplicitSite { object } => {
                let portal = portal.ok_or(AppError::Internal)?;
                let noun = orchestration_engine::object_noun(object).to_owned();
                self.execute_funnel_in_page_goal(
                    prompt,
                    noun,
                    portal.clone(),
                    format!(
                        "funnel_route: implicit site {} · SiteSearch skipped",
                        portal.host_str().unwrap_or("?")
                    ),
                    emit,
                )
                .await
            }
            orchestration_engine::FunnelDecision::AskParser => {
                self.execute_funnel_ask_parser(prompt, plan, portal, emit)
                    .await
            }
        }
    }

    /// Already-on-origin arm: the site slot matches the live portal through
    /// the shared alias-aware host check. An object is pursued in-page;
    /// with no object the portal is already the answer and the run
    /// completes without pursuing anything — and without consulting
    /// `SiteSearch` either way.
    async fn execute_funnel_already_on_origin(
        &self,
        prompt: String,
        site: &str,
        object: Option<orchestration_engine::ObjectClass>,
        portal: url::Url,
        emit: &mut (impl FnMut(PlaybookEvent) + Send),
    ) -> Result<DispatchOutcome, AppError> {
        let route_line = format!(
            "funnel_route: already_on_origin '{site}' ({}) · SiteSearch skipped",
            portal.host_str().unwrap_or("?")
        );
        if let Some(class) = object {
            self.execute_funnel_in_page_goal(
                prompt,
                orchestration_engine::object_noun(class).to_owned(),
                portal,
                route_line,
                emit,
            )
            .await
        } else {
            let _ = self.record(&route_line).await;
            let name = orchestration_engine::ephemeral_name(&prompt);
            let mut outcome = DispatchOutcome {
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
                lend_id: None,
                route_log: None,
                telemetry_log: None,
                final_frame: None,
                challenge: None,
                auth_url: None,
                final_url: None,
                page_title: None,
            };
            self.settle_ephemeral_outcome(&mut outcome).await;
            Ok(outcome)
        }
    }

    /// Shared in-page arm: journal the route line, pursue the canonical
    /// object noun on the live portal, then settle. The noun comes from
    /// the funnel's closed object vocabulary — never a raw prompt
    /// fragment.
    async fn execute_funnel_in_page_goal(
        &self,
        prompt: String,
        noun: String,
        portal: url::Url,
        route_line: String,
        emit: &mut (impl FnMut(PlaybookEvent) + Send),
    ) -> Result<DispatchOutcome, AppError> {
        let _ = self.record(&route_line).await;
        let mut outcome = self
            .dispatch_in_page_goal(portal, prompt, noun, &mut *emit)
            .await?;
        self.settle_ephemeral_outcome(&mut outcome).await;
        Ok(outcome)
    }

    /// Ground-site arm: the site slot is not the live portal. A typed
    /// domain in the prompt is ground truth and lands directly (Tier 0 is
    /// preserved: `"open amazon.in"` never becomes a ladder query for
    /// `"amazon"`). Otherwise the site slot alone runs the shared
    /// site-only ladder — never a search. A ladder miss with an object and
    /// a live portal falls back to pursuing the object in-page (the
    /// live-page-fallback behavior, kept inside the funnel); with no
    /// object or no portal it is the honest miss.
    async fn execute_funnel_ground_site(
        &self,
        prompt: String,
        site: String,
        object: Option<orchestration_engine::ObjectClass>,
        cleaned: &str,
        portal: Option<url::Url>,
        emit: &mut (impl FnMut(PlaybookEvent) + Send),
    ) -> Result<DispatchOutcome, AppError> {
        if let Some(entry) = orchestration_engine::explicit_url_in_prompt(cleaned) {
            let route_log =
                format!("route_proposed: funnel '{site}' → {entry} · source: ExplicitDomain");
            let _ = self.record(&route_log).await;
            return self
                .execute_funnel_land(
                    prompt,
                    FunnelLanding {
                        site: &site,
                        source: Some(orchestration_engine::RouteSource::ExplicitDomain),
                        entry,
                        route_log: Some(route_log),
                    },
                    object,
                    emit,
                )
                .await;
        }
        let (route, grounder_configured) = self.resolve_site_via_ladder(&site).await;
        let Some(route) = route else {
            let _ = self
                .record(&format!("funnel_miss: site='{site}' ungrounded"))
                .await;
            if let (Some(class), Some(portal)) = (object, portal) {
                let noun = orchestration_engine::object_noun(class).to_owned();
                return self
                    .execute_funnel_in_page_goal(
                        prompt,
                        noun.clone(),
                        portal,
                        format!(
                            "funnel_route: ladder miss for '{site}' · pursuing '{noun}' in-page · SiteSearch skipped"
                        ),
                        emit,
                    )
                    .await;
            }
            let proposed = ProposedEntry {
                direct_open_miss: true,
                grounder_configured,
                ..ProposedEntry::default()
            };
            return Err(
                Self::direct_open_miss_error(&proposed).unwrap_or(AppError::InvalidInput(
                    "The derived intent is not runnable.",
                )),
            );
        };
        let mut route_log = format!(
            "route_proposed: funnel '{site}' → {}{} · source: {:?}",
            route.url.host_str().unwrap_or("?"),
            route.url.path(),
            route.source,
        );
        if let Some(backend) = route.directory_backend {
            route_log.push_str(" · via ");
            route_log.push_str(backend);
        }
        if let Some(veto) = route.directory_veto.as_deref() {
            route_log.push_str(" · ");
            route_log.push_str(veto);
        }
        let _ = self.record(&route_log).await;
        self.execute_funnel_land(
            prompt,
            FunnelLanding {
                site: &site,
                source: Some(route.source),
                entry: route.url.as_str().to_owned(),
                route_log: Some(route_log),
            },
            object,
            emit,
        )
        .await
    }

    /// Land a grounded funnel site. With an object, the existing cold-goal
    /// machinery lands the resolved entry and pursues the canonical noun
    /// on the live page — the entry is the ladder's answer, `SiteSearch` is
    /// never re-entered for the site, and the site slot threads into the
    /// settle contract. Without an object the landing itself is the goal: a
    /// direct site open completes without semantic noun pursuit.
    async fn execute_funnel_land(
        &self,
        prompt: String,
        landing: FunnelLanding<'_>,
        object: Option<orchestration_engine::ObjectClass>,
        emit: &mut (impl FnMut(PlaybookEvent) + Send),
    ) -> Result<DispatchOutcome, AppError> {
        match object {
            Some(class) => {
                self.dispatch_cold_in_page_goal(
                    prompt,
                    landing.source,
                    Some(landing.entry),
                    orchestration_engine::object_noun(class).to_owned(),
                    landing.route_log,
                    Some(landing.site),
                    &mut *emit,
                )
                .await
            }
            None => {
                self.dispatch_funnel_site_open(prompt, landing.site, landing.source, landing.entry)
                    .await
            }
        }
    }

    /// Pure site open: no object, so the landing is the goal. Validates the
    /// entry like any user-directed destination, records the session
    /// origin, lands, then runs the settle contract BEFORE settle: the
    /// observed landing must belong to the site slot — a search-shaped or
    /// mismatched landing is FAILED, never Completed.
    async fn dispatch_funnel_site_open(
        &self,
        prompt: String,
        site: &str,
        source: Option<orchestration_engine::RouteSource>,
        entry: String,
    ) -> Result<DispatchOutcome, AppError> {
        if !orchestration_engine::entry_url_valid(source, entry.as_str()) {
            return Err(AppError::InvalidInput(
                "The derived intent is not runnable.",
            ));
        }
        let site_url = url::Url::parse(&entry)
            .map_err(|_| AppError::InvalidInput("The derived intent is not runnable."))?;
        let mut portal = site_url.clone();
        portal.set_path("/");
        portal.set_query(None);
        portal.set_fragment(None);
        *self.session_origin.lock().map_err(|_| AppError::Internal)? = Some(portal.clone());
        let browser = self.browser(BrowserIntent::Background).await?;
        macro_engine::ensure_at_entry_url(&browser, &site_url)
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        let _ = self
            .record(&format!(
                "funnel_site_open: '{site}' → {}",
                portal.host_str().unwrap_or("?")
            ))
            .await;
        // Settle contract (D.1) before settle: the observed landing must
        // belong to the site slot.
        self.verify_funnel_landing(site, Some(&site_url)).await?;
        let name = orchestration_engine::ephemeral_name(&prompt);
        let mut outcome = DispatchOutcome {
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
            lend_id: None,
            route_log: None,
            telemetry_log: None,
            final_frame: None,
            challenge: None,
            auth_url: None,
            final_url: None,
            page_title: None,
        };
        self.settle_ephemeral_outcome(&mut outcome).await;
        Ok(outcome)
    }

    /// Funnel settle contract (D.1): after the funnel lands, COMPLETED
    /// requires the observed landing to belong to the resolved site slot
    /// (alias-aware). A search-shaped landing on a non-matching domain —
    /// or any domain mismatch — is FAILED with an honest journal line
    /// naming the site, never a quiet COMPLETED. In-page object goals keep
    /// their existing verifier semantics; this fires only where the funnel
    /// threaded a site slot.
    async fn verify_funnel_landing(
        &self,
        site: &str,
        entry: Option<&url::Url>,
    ) -> Result<(), AppError> {
        let browser = self
            .browser(BrowserIntent::Background)
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        // Re-read the landing now (URL plus title): navigation merely not
        // erroring does not mean the destination was reached.
        let landed = browser
            .current_url()
            .await
            .map_err(|_| AppError::BrowserUnavailable)?
            .ok_or(AppError::BrowserUnavailable)?;
        let title = browser.page_title().await;
        if Self::open_landing_valid(site, entry, &landed) {
            let _ = self
                .record(&format!(
                    "funnel_landing_read: {} · title {}",
                    landed.host_str().unwrap_or("?"),
                    if title.as_deref().is_some_and(|t| !t.trim().is_empty()) {
                        "present"
                    } else {
                        "empty"
                    }
                ))
                .await;
            return Ok(());
        }
        let _ = self
            .record(&format!(
                "funnel_settle_miss: did not reach '{site}' · landed {}",
                landed.host_str().unwrap_or("?")
            ))
            .await;
        self.journal_observed_failure(None, &format!("landing does not belong to '{site}'"))
            .await;
        Err(AppError::WorkflowFailed(self.recent_journal(16).await))
    }

    /// Whether an observed direct-open landing is the intended destination:
    /// the host belongs to the resolved site slot (alias-aware) and, when
    /// the entry is known, is the entry's own site (`www.` folded); a deep
    /// entry path must also survive the landing — a redirect elsewhere on
    /// the same host (e.g. to a login page) is not the destination. Pure.
    #[must_use]
    pub fn open_landing_valid(site: &str, entry: Option<&url::Url>, landed: &url::Url) -> bool {
        if !orchestration_engine::funnel_landing_matches(site, landed) {
            return false;
        }
        let Some(entry) = entry else {
            return true;
        };
        if !macro_engine::same_site_host(
            landed.host_str().unwrap_or(""),
            entry.host_str().unwrap_or(""),
        ) {
            return false;
        }
        let want = entry.path().trim_end_matches('/');
        want.is_empty() || landed.path().trim_end_matches('/').starts_with(want)
    }

    /// Journal line for a batch that stopped on a failed click dispatch:
    /// names the click (1-based position and label) and how many landed.
    #[must_use]
    pub fn batch_click_failure_line(
        clicks_completed: usize,
        failed_index: usize,
        label: &str,
    ) -> String {
        format!(
            "batch:partial_failure: click {} '{label}' failed to dispatch · {clicks_completed} click(s) landed before it",
            failed_index + 1
        )
    }

    /// Live-page verification for the generic noun-hunt lane: re-read URL,
    /// title and headings and require a positive noun signal. A failed
    /// browser read fails closed.
    async fn noun_landing_verified(
        &self,
        browser: &std::sync::Arc<browser_driver::ManagedBrowser>,
        portal: &url::Url,
        noun: &str,
    ) -> bool {
        let Ok(Some(current)) = browser.current_url().await else {
            return false;
        };
        let title = browser.page_title().await;
        let (elements, _, _) = browser.ax_snapshot(portal).await;
        macro_engine::verify_noun_landing(noun, &current, portal, title.as_deref(), &elements)
    }

    /// Tier 2B, routing lane: the fenced parser gets one bounded shot at
    /// the aside-cleaned prompt — asides never reach it. Only a
    /// `site_context` slot routes; anything else is the honest miss, never
    /// a search page.
    async fn execute_funnel_ask_parser(
        &self,
        prompt: String,
        plan: orchestration_engine::FunnelPlan,
        portal: Option<url::Url>,
        emit: &mut (impl FnMut(PlaybookEvent) + Send),
    ) -> Result<DispatchOutcome, AppError> {
        let parsed = orchestration_engine::parse_prompt_bounded(&self.intent_parser, &plan.cleaned);
        let Some(site) = parsed.and_then(|slots| slots.site_context) else {
            let _ = self
                .record("funnel_tier2b: parser declined · honest miss")
                .await;
            return Err(AppError::InvalidInput(
                "Which site should I open? I couldn't tell from that prompt — try the full domain (for example 'open amazon.in'), or save a site shortcut and try again.",
            ));
        };
        let _ = self.record(&format!("funnel_tier2b: site='{site}'")).await;
        // The parser's site re-enters the ladder: already-on-origin first,
        // then the site-only ladder — never a search fallback.
        let on_origin = portal
            .as_ref()
            .and_then(|portal| portal.host_str())
            .is_some_and(|host| orchestration_engine::site_matches_host(&site, host));
        if on_origin {
            let portal = portal.ok_or(AppError::Internal)?;
            return self
                .execute_funnel_already_on_origin(
                    prompt,
                    &site,
                    plan.slots.object_slot,
                    portal,
                    emit,
                )
                .await;
        }
        self.execute_funnel_ground_site(
            prompt,
            site,
            plan.slots.object_slot,
            &plan.cleaned,
            portal,
            emit,
        )
        .await
    }

    /// Cold-start in-page goal: the prompt names a site and carries an
    /// artifact noun ("open my profile on the reddit"), and the ladder
    /// grounded the site itself — never a search page. Land the site, then
    /// pursue the noun on the live page: the navigated URL is observed from
    /// real controls, never predicted from the prompt. Verbs decide nothing
    /// here; only the (site, artifact) noun pair does, so
    /// "open/show/take me to my profile" all take this path. The caller
    /// checks the noun first, so this always handles the goal.
    /// Extracted to keep `dispatch_adhoc_auto_acquire` within the line
    /// budget.
    ///
    /// `site_slot` carries the funnel's resolved site for the settle
    /// contract (D.1): `Some` when the funnel threaded a site through the
    /// ladder — the observed landing must belong to it, checked
    /// immediately after the final-page capture and before challenge/auth
    /// handling. `None` (the old cold path) keeps the existing behavior.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_cold_in_page_goal(
        &self,
        prompt: String,
        source: Option<orchestration_engine::RouteSource>,
        entry: Option<String>,
        noun: String,
        route_log: Option<String>,
        site_slot: Option<&str>,
        emit: impl FnMut(PlaybookEvent) + Send,
    ) -> Result<DispatchOutcome, AppError> {
        let entry = entry.ok_or(AppError::InvalidInput(
            "The derived intent is not runnable.",
        ))?;
        let site_url = url::Url::parse(&entry)
            .map_err(|_| AppError::InvalidInput("The derived intent is not runnable."))?;
        // Strict validation before any navigation or session write —
        // the same provenance-aware bar as the normal path. The site
        // was named by the user, so structural validation suffices.
        let valid = orchestration_engine::entry_url_valid(source, site_url.as_str());
        if !valid {
            return Err(AppError::InvalidInput(
                "The derived intent is not runnable.",
            ));
        }
        // The entry origin becomes the run portal, recorded as the
        // session origin so the run machinery's origin check passes.
        let mut portal = site_url.clone();
        portal.set_path("/");
        portal.set_query(None);
        portal.set_fragment(None);
        *self.session_origin.lock().map_err(|_| AppError::Internal)? = Some(portal.clone());
        let browser = self.browser(BrowserIntent::Background).await?;
        // Land the site before acting: the pursuit snapshots the live
        // page, so acting must start from the destination, not
        // about:blank.
        macro_engine::ensure_at_entry_url(&browser, &site_url)
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        let _ = self
            .record(&format!(
                "in_page_goal_cold: '{noun}' → {}",
                portal.host_str().unwrap_or("?")
            ))
            .await;
        let mut outcome = self
            .dispatch_in_page_goal(portal, prompt, noun, emit)
            .await?;
        // Keep the site route line above the goal line so Session
        // Activity reads proposal → goal in order.
        if let Some(site_line) = route_log {
            outcome.route_log = Some(match outcome.route_log.take() {
                Some(goal_line) => format!("{site_line}\n{goal_line}"),
                None => site_line,
            });
        }
        // Settle (final frame + challenge/auth branching) runs here,
        // exactly like the normal path. The funnel's settle contract (D.1)
        // fires first when a site slot was threaded: the observed landing
        // must belong to the resolved site — a search-shaped or mismatched
        // landing is FAILED here, before challenge/auth success handling
        // can report a quiet success. In-page object goals keep their
        // existing verifier semantics; only the site-domain check is added.
        if let Some(site) = site_slot {
            self.verify_funnel_landing(site, None).await?;
        }
        self.settle_ephemeral_outcome(&mut outcome).await;
        Ok(outcome)
    }

    /// Whether the funnel owns this prompt's dispatch: open-verb-led and
    /// non-plural. This is the single source of truth for the preemption
    /// gates in both dispatch lanes ([`Self::dispatch_adhoc_auto_acquire`]
    /// and [`Self::dispatch_single_ephemeral`]); `dispatch_funnel`
    /// re-derives the same decision from the plan, so a claimed prompt
    /// always takes the funnel and never reaches the old proposal
    /// machinery below either gate.
    fn funnel_owns_prompt(prompt: &str, is_plural: bool) -> bool {
        !is_plural && orchestration_engine::funnel_claims(prompt)
    }

    async fn dispatch_adhoc_auto_acquire(
        &self,
        prompt: String,
        mut intent: macro_engine::SemanticIntent,
        mut emit: impl FnMut(PlaybookEvent) + Send,
    ) -> Result<DispatchOutcome, AppError> {
        // Verb-led preemption: a prompt starting with a closed-vocabulary
        // verb phrase ("log out from reddit") routes before the
        // funnel/search machinery — saved replay already won (this is an
        // ephemeral lane), so nothing taught is bypassed. Settles here
        // because the ad-hoc lane's own settle runs at its end, past this
        // early return.
        if let Some(action) = orchestration_engine::detect_verb_led_action(&prompt) {
            let mut outcome = self
                .dispatch_verb_led_action(prompt, action, &mut emit)
                .await?;
            self.settle_ephemeral_outcome(&mut outcome).await;
            return Ok(outcome);
        }
        // Funnel first: open-verb-led, non-plural prompts route through the
        // funnel before the old follow-up / proposal / search machinery.
        // The gate is pure (no browser): only a claimed prompt attaches
        // the live portal. The funnel declines anything it does not claim,
        // so unclaimed prompts keep the existing pipeline untouched. The
        // return inside makes the preemption terminal: a claimed prompt
        // can never fall through to the machinery below.
        if Self::funnel_owns_prompt(&prompt, intent.is_plural) {
            let portal = self.live_portal().await;
            if let Some(outcome) = self
                .dispatch_funnel(prompt.clone(), &intent, portal, &mut emit)
                .await
            {
                return outcome;
            }
        }
        // Fast path: already on the named origin → pursue the artifact in-page.
        if !intent.is_plural
            && let Some((portal, noun)) = self.follow_up_in_page_goal(&prompt).await
        {
            return self
                .dispatch_follow_up_goal(portal, prompt, noun, emit)
                .await;
        }
        // Tiered resolution first so the destination is journaled even when
        // the browser cannot start (mirrors the connected lane ordering).
        // `propose_entry_url` validates every tier (https, no credentials,
        // allowlisted host); an ungroundable prompt journals a miss and the
        // lane fails honestly below instead of navigating anywhere.
        let proposed = self.propose_entry_url(&prompt, &mut intent, None).await;
        let route_log = proposed.log.clone();
        // Muse-style follow-up: the ladder could not ground this
        // direct-open and a page is already live. Try the target noun
        // in-page on the live origin before the miss error.
        if let Some(noun) = Self::live_page_fallback_noun(&prompt, intent.is_plural, &proposed)
            && let Some(portal) = self.live_portal().await
        {
            let _ = self
                .record(&format!(
                    "in_page_goal_fallback: '{noun}' on {} · trying live page before the miss error",
                    portal.host_str().unwrap_or("?")
                ))
                .await;
            match self
                .dispatch_in_page_goal(portal, prompt.clone(), noun, |_| {})
                .await
            {
                Ok(mut outcome) => {
                    self.settle_ephemeral_outcome(&mut outcome).await;
                    return Ok(outcome);
                }
                // In-page miss: continue the normal flow.
                Err(AppError::WorkflowFailed(_)) => {}
                Err(other) => return Err(other),
            }
        }
        // Ask-and-learn: the ladder could not ground this direct open.
        // Fail here with guidance instead of the generic "not runnable"
        // or, worse, a scraped search page.
        if let Some(err) = Self::direct_open_miss_error(&proposed) {
            return Err(err);
        }
        // Cold-start in-page goal: the prompt names a site and carries an
        // artifact noun ("open my profile on the reddit"), and the ladder
        // grounded the site itself — never a search page.
        let cold_noun = if intent.is_plural {
            None
        } else {
            Self::cold_in_page_goal(&prompt, proposed.source)
        };
        if let Some(noun) = cold_noun {
            return self
                .dispatch_cold_in_page_goal(
                    prompt,
                    proposed.source,
                    intent.entry_url.clone(),
                    noun,
                    route_log,
                    None,
                    emit,
                )
                .await;
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
        // Delegate to the connected lanes: they reuse the attached browser,
        // journal the target, `ensure_at_entry_url` via pre-navigation, and
        // `reanchor_portal` before snapshotting. Batch intents keep the batch
        // lane so plural prompts never truncate to one click.
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
        self.settle_ephemeral_outcome(&mut outcome).await;
        Ok(outcome)
    }

    /// Post-delegation settle shared by the ad-hoc auto-acquire lane and
    /// the connected-portal lane: capture the final-frame evidence, then
    /// run the three-way settle branch — challenge gate, guest landing,
    /// or silent success. Every ephemeral lane ends here.
    async fn settle_ephemeral_outcome(&self, outcome: &mut DispatchOutcome) {
        // Settle evidence: one-shot viewport of the live target, captured
        // after the landing. The streaming screencast is armed on the
        // pre-navigation session and does not survive cross-origin
        // navigation, so without this the settled card freezes on the
        // launch placeholder instead of showing the destination.
        self.attach_final_frame(outcome).await;
        let completed = outcome.result.status == orchestration_engine::SequenceStatus::Completed;
        // Caption from the live re-read `attach_final_frame` just took, not
        // from the intended entry. Journaled before any completion record.
        let verdict = if completed {
            ObservedVerdict::Completed
        } else {
            ObservedVerdict::Failed
        };
        let caption = caption_observed_state(
            verdict,
            &ObservedState {
                final_url: outcome.final_url.as_deref(),
                page_title: outcome.page_title.as_deref(),
                acted_on: None,
                note: None,
            },
        );
        let caption = self.journal_line(caption).await;
        outcome.telemetry_log = Some(match outcome.telemetry_log.take() {
            Some(existing) => format!("{existing}\n{caption}"),
            None => caption,
        });
        // A completed run that landed on a human-verification gate is not
        // a silent success. Interactive gates (reCAPTCHA checkbox) skip L1:
        // no off-screen re-navigation can click a checkbox, so escalation
        // would burn ~30s polling for a miracle — the card offers session
        // lending and L2 Take Control immediately. Interstitials
        // (Cloudflare / Turnstile) keep the automatic L1 path first; only a
        // persistent challenge keeps the card, routing the check to the
        // user.
        if completed && let Some((challenge_url, kind)) = self.detect_challenge_kind().await {
            let host = url::Url::parse(&challenge_url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_else(|| challenge_url.clone());
            // Register the run for a possible consent tap before branching:
            // the lend command resolves the page URL from this registry,
            // never from a frontend-supplied host string. The key is
            // decoupled from the save-workflow run_id — pure direct opens
            // are deliberately not remembered, so settle synthesizes a key
            // for them.
            let lend_id = outcome.run_id.clone().unwrap_or_else(|| {
                format!("lend-{}", self.next_lend_id.fetch_add(1, Ordering::Relaxed))
            });
            self.remember_session_lend(
                lend_id.clone(),
                challenge_url.clone(),
                SessionLendOrigin::Challenge,
            );
            if kind == ChallengeKind::InteractiveGate {
                self.journal_line(format!(
                    "challenge_detected: {host} · interactive gate (L1 skipped)"
                ))
                .await;
                outcome.lend_id = Some(lend_id);
                outcome.challenge = Some(challenge_url);
                return;
            }
            // Every escalation outcome is journaled — cleared, persistent,
            // or failed — so the thread explains why the card did or did
            // not appear. Persistent and failed escalations both keep the
            // L2 Take Control card.
            match self.auto_escalate_challenge(&challenge_url).await {
                Ok(true) => {
                    let line = self
                        .journal_line(format!("challenge_auto_escalated: {host} · cleared"))
                        .await;
                    outcome.telemetry_log = Some(match outcome.telemetry_log.take() {
                        Some(existing) => format!("{line}\n{existing}"),
                        None => line,
                    });
                    // The pre-escalation frame shows the interstitial; show
                    // the cleared page instead.
                    self.attach_final_frame(outcome).await;
                    // The headed session's job is done: shut it down
                    // gracefully so no phantom window lingers. Clearance
                    // persists in the app profile on disk, so the next
                    // acquisition lazily launches off-screen headed again.
                    self.stand_down_escalated_browser().await;
                }
                Ok(false) => {
                    self.journal_line(format!("challenge_auto_escalated: {host} · persistent"))
                        .await;
                    outcome.lend_id = Some(lend_id.clone());
                    outcome.challenge = Some(challenge_url);
                }
                Err(err) => {
                    // Static labels only: AppError's Debug could carry a
                    // payload, and nothing here is worth leaking to a log.
                    let detail = match err {
                        AppError::BrowserUnavailable => "browser unavailable",
                        AppError::Busy => "busy",
                        AppError::Internal => "internal",
                        _ => "unexpected",
                    };
                    self.journal_line(format!(
                        "challenge_auto_escalated: {host} · failed ({detail})"
                    ))
                    .await;
                    outcome.lend_id = Some(lend_id);
                    outcome.challenge = Some(challenge_url);
                }
            }
        } else if completed {
            // No gate — but a clean guest landing is not a useful silent
            // success for an authenticated action studio. Probe the auth
            // state: a logged-out reading renders the auth-sync card (one
            // tap pulls the daily browser's session over the Companion
            // bridge and persists it to the app profile); authenticated or
            // unclassifiable pages keep today's silent behavior. The
            // challenge check above runs first, so a gated page carrying
            // login copy never lands here.
            //
            // Note: a run whose L1 escalation cleared keeps the silent
            // path — the headed browser is stood down right after, so a
            // sync tap would have nothing attached to work with.
            //
            // Every reading is journaled, not just the logged-out one: a
            // silent Unknown is otherwise indistinguishable from "the probe
            // never ran", which is exactly what made the first live test's
            // missing card undiagnosable.
            let probe_line = match self.detect_auth_state().await {
                AuthState::LoggedOut => {
                    if let Some(page_url) = outcome.final_url.clone() {
                        let host = url::Url::parse(&page_url)
                            .ok()
                            .and_then(|url| url.host_str().map(str::to_owned))
                            .unwrap_or_else(|| page_url.clone());
                        // The lend command resolves the page URL from this registry,
                        // never from a frontend-supplied host string. The key is
                        // decoupled from the save-workflow run_id — pure direct
                        // opens are deliberately not remembered, so settle
                        // synthesizes a key for them.
                        let lend_id = outcome.run_id.clone().unwrap_or_else(|| {
                            format!("lend-{}", self.next_lend_id.fetch_add(1, Ordering::Relaxed))
                        });
                        self.remember_session_lend(
                            lend_id.clone(),
                            page_url.clone(),
                            SessionLendOrigin::GuestLanding,
                        );
                        outcome.lend_id = Some(lend_id);
                        outcome.auth_url = Some(page_url);
                        format!("auth_state_detected: {host} · logged out")
                    } else {
                        "auth_state_detected: logged out · no final url".to_string()
                    }
                }
                AuthState::Authenticated => "auth_state_detected: authenticated".to_string(),
                AuthState::Unknown => auth_unknown_label(outcome.final_url.as_deref()),
            };
            let line = self.journal_line(probe_line).await;
            outcome.telemetry_log = Some(match outcome.telemetry_log.take() {
                Some(existing) => format!("{existing}\n{line}"),
                None => line,
            });
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
            lend_id: None,
            route_log: None,
            telemetry_log: None,
            // Saved replays render the live screencast while running; the
            // settled card keeps the last live frame.
            // Saved replays report the live page as-is; challenge
            // detection is an ad-hoc-lane concern.
            final_frame: None,
            challenge: None,
            auth_url: None,
            final_url: None,
            page_title: None,
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
    pub async fn load_shortcut_map(&self) -> Option<std::collections::HashMap<String, String>> {
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
    pub async fn offer_shortcut_after_landing(&self, proposed: &ProposedEntry) -> Option<String> {
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
    /// Gate for the live-page fallback: only a high-confidence verb-led
    /// direct-open whose target the ladder could NOT ground as a site
    /// gets a chance at the live page. Grounded prompts ("open claude for
    /// me") keep today's behavior, low-confidence prompts ("what are the
    /// settings") and plurals stay out.
    pub fn live_page_fallback_noun(
        prompt: &str,
        is_plural: bool,
        proposed: &ProposedEntry,
    ) -> Option<String> {
        if is_plural {
            return None;
        }
        let grammar = orchestration_engine::parse_grammar(prompt, None);
        if !orchestration_engine::is_direct_open(prompt, &grammar) {
            return None;
        }
        if !proposed.direct_open_miss {
            return None;
        }
        let noun = grammar.target_noun?.trim().to_string();
        if noun.is_empty() {
            return None;
        }
        Some(noun)
    }

    pub fn direct_open_miss_error(proposed: &ProposedEntry) -> Option<AppError> {
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
    pub async fn propose_entry_url(
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
        // is never consulted by this tier.
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
        // Composite directory rung: Brave's sanctioned search API when
        // `CLINCH_BRAVE_API_KEY` is set, DuckDuckGo's keyless HTML endpoint
        // as the zero-config fallback. Backend HTTP in memory only — the
        // browser never sees a search page. The rung is never unconfigured
        // (DDG works out of the box), so the miss line names which backends
        // were tried instead of a setup hint.
        let site_search = orchestration_engine::ChainedSiteSearch::new();
        let directory_label = site_search.backend_label();
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
        // grounder reads as a setup hint rather than a dead end. The
        // directory rung is always live (DDG keyless fallback); the label
        // names which backends the chain tried.
        let grounder_configured = live_grounder.is_some();
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
                llm: None,
                parser: None,
                shortcuts: Some(&shortcut_store),
                site_search: Some(&site_search as &dyn orchestration_engine::SiteSearchClient),
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
            let line = format!(
                "route_proposed:{}{} · source: {:?}{}{}",
                route.url.host_str().unwrap_or("?"),
                route.url.path(),
                route.source,
                // Backend transparency: name which directory backend served
                // a SiteSearch hit (`· via ddg`); other sources carry none.
                route
                    .directory_backend
                    .map(|backend| format!(" · via {backend}"))
                    .unwrap_or_default(),
                // A vetoed directory hit rides the fallthrough route so the
                // journal reads as a veto, not a silent miss.
                route
                    .directory_veto
                    .as_deref()
                    .map(|veto| format!(" · {veto}"))
                    .unwrap_or_default(),
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
                // In-page-goal prompts carry the site in `site_context`
                // while `target_noun` is empty there; either way the
                // grounded name feeds the consent-gated shortcut offer.
                let grammar = orchestration_engine::parse_grammar(prompt, connected_origin);
                let site = grammar
                    .target_noun
                    .or(grammar.site_context)
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
                let directory_state = format!("directory attempted ({directory_label}), no match");
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
            .request_sync(portal, crate::ws_server::RESPONSE_TIMEOUT, None)
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
        mut emit: impl FnMut(PlaybookEvent) + Send,
    ) -> Result<DispatchOutcome, AppError> {
        // Verb-led preemption: a prompt starting with a closed-vocabulary
        // verb phrase ("log out from reddit") routes before the
        // funnel/search machinery — saved replay already won (this is an
        // ephemeral lane), so nothing taught is bypassed. `dispatch_one_prompt`
        // settles this lane's outcome, so the verb-led outcome returns
        // unsettled like every other path here.
        if let Some(action) = orchestration_engine::detect_verb_led_action(&prompt) {
            return self
                .dispatch_verb_led_action(prompt, action, &mut emit)
                .await;
        }
        // Funnel first: open-verb-led, non-plural prompts claim the funnel
        // before the old connected in-page detector, so claiming and
        // journaling are never bypassed (e.g. "open settings on reddit").
        // The connected portal is already known, so no extra browser
        // attach. The funnel declines anything it does not claim, so
        // unclaimed prompts keep the existing pipeline untouched. The
        // return inside makes the preemption terminal: a claimed prompt
        // can never fall through to the machinery below.
        if Self::funnel_owns_prompt(&prompt, intent.is_plural)
            && let Some(outcome) = self
                .dispatch_funnel(prompt.clone(), &intent, Some(portal.clone()), &mut emit)
                .await
        {
            return outcome;
        }
        // Muse-style follow-up: the prompt explicitly names the connected
        // portal as its site and carries an artifact noun ("open my profile
        // in reddit" while on reddit.com). Pursue it on the live page — no
        // new browser, no entry-URL resolution, no navigation before acting.
        if let Some(noun) = orchestration_engine::detect_in_page_goal(&prompt, Some(&portal)) {
            return self.dispatch_in_page_goal(portal, prompt, noun, emit).await;
        }
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
                lend_id: None,
                route_log,
                telemetry_log,
                // Settle (final frame + L1 challenge handling) runs in
                // settle_ephemeral_outcome after this returns; the inner
                // outcome carries none itself.
                // Inner outcome: settle_ephemeral_outcome detects challenges
                // after delegation returns.
                final_frame: None,
                challenge: None,
                auth_url: None,
                final_url: None,
                page_title: None,
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
            lend_id: None,
            route_log,
            // Per-snapshot lines live inside the macro engine here, which
            // has no journal access — but the re-anchor line (if the anchor
            // changed) still surfaces above the outcome.
            telemetry_log,
            // Settle (final frame + L1 challenge handling) runs in
            // settle_ephemeral_outcome after this returns; the inner outcome
            // carries none itself.
            // Inner outcome: settle_ephemeral_outcome detects challenges
            // after delegation returns.
            final_frame: None,
            challenge: None,
            auth_url: None,
            final_url: None,
            page_title: None,
        })
    }

    /// Execute a Muse-style in-page follow-up: pursue the artifact noun on the
    /// live portal page without resolving a new entry URL or navigating
    /// first. The pursuit is the macro engine's bounded observe-act loop
    /// (click a control mentioning the noun; unfold one header menu when
    /// the noun isn't directly visible). Settle — final frame plus
    /// challenge handling — runs in `settle_ephemeral_outcome` after this
    /// returns, exactly like every other ephemeral lane.
    /// Build the gear-2 escalation navigator for one dispatch: opt-in via
    /// `CLINCH_ESCALATION_MODEL` (and `CLINCH_ESCALATION_PROVIDER`,
    /// defaulting to groq). Unset keeps today's behavior — the model phase
    /// misses honestly with no escalation pass.
    fn model_escalation() -> Option<macro_engine::ModelEscalation> {
        orchestration_engine::LlmPageNavigator::escalation_from_env().map(|navigator| {
            let model_name = navigator.model_name().to_owned();
            macro_engine::ModelEscalation {
                navigator: std::sync::Arc::new(navigator) as _,
                model_name,
            }
        })
    }

    async fn dispatch_in_page_goal(
        &self,
        portal: url::Url,
        prompt: String,
        noun: String,
        _emit: impl FnMut(PlaybookEvent) + Send,
    ) -> Result<DispatchOutcome, AppError> {
        let goal_line = format!(
            "in_page_goal: '{noun}' on {}",
            portal.host_str().unwrap_or("?")
        );
        let _ = self.record(&goal_line).await;
        // Background intent: reuse the attached session, never open a
        // window, never navigate before acting.
        let browser = self.browser(BrowserIntent::Background).await?;
        let name = orchestration_engine::ephemeral_name(&prompt);
        // The model-guided phase is opt-in: `CLINCH_NAVIGATOR_PROVIDER`
        // names `groq` or `ollama`; unset keeps the follow-up purely
        // deterministic. Built once per dispatch and shared across the
        // model phase's steps.
        let navigator: Option<std::sync::Arc<dyn macro_engine::PageNavigator>> =
            orchestration_engine::LlmPageNavigator::from_env()
                .map(|navigator| std::sync::Arc::new(navigator) as _);
        let _ = self
            .record(&format!(
                "in_page_goal_navigator: {}",
                if navigator.is_some() {
                    "model-guided phase armed"
                } else {
                    "deterministic only"
                }
            ))
            .await;
        // Gear-2 escalation is opt-in separately: when armed, one
        // additional bounded model pass runs under the escalation model
        // after the main pass's tail fails verification.
        let escalation = Self::model_escalation();
        let _ = self
            .record(&format!(
                "in_page_goal_escalation: {}",
                escalation
                    .as_ref()
                    .map_or("unset".to_owned(), |escalation| format!(
                        "{} armed",
                        escalation.model_name
                    ))
            ))
            .await;
        // Verb-spec branch: the closed noun table maps the artifact noun to
        // its verb spec (vocabulary + verifier). Identity goals ("my
        // profile", "my account") get the chrome worker with memory and
        // verification; settings goals ("settings", "preferences") get the
        // same worker with the same memory shape; log-out goals get the
        // worker with the signed-out verifier and no memory; every other
        // artifact keeps the generic noun-hunt path below.
        if let Some(spec) = orchestration_engine::spec_for_noun(&noun) {
            let _ = self
                .record(&format!("in_page_goal_class: {}", spec.kind.as_str()))
                .await;
            return self
                .dispatch_verb_spec_goal(
                    &browser, &portal, &name, spec, goal_line, navigator, escalation,
                )
                .await;
        }
        match macro_engine::pursue_page_goal(&browser, &portal, &noun, navigator, escalation).await
        {
            Ok(macro_engine::PageGoalOutcome::Navigated {
                label,
                landed,
                hit_lines,
            }) => {
                for line in &hit_lines {
                    let _ = self.record(line).await;
                }
                // A URL change is not evidence: the live landing must
                // positively name the noun before the run may complete.
                if !self.noun_landing_verified(&browser, &portal, &noun).await {
                    let _ = self
                        .record(&format!(
                            "in_page_goal_miss: '{label}' → {} (navigation not verified)",
                            landed.host_str().unwrap_or("?")
                        ))
                        .await;
                    self.journal_observed_failure(Some(&label), "navigation not verified")
                        .await;
                    return Err(AppError::WorkflowFailed(self.recent_journal(16).await));
                }
                let _ = self
                    .record(&format!(
                        "in_page_goal_done: '{label}' → {} (verified at completion)",
                        landed.host_str().unwrap_or("?")
                    ))
                    .await;
            }
            Ok(macro_engine::PageGoalOutcome::AlreadyThere { landed }) => {
                if !self.noun_landing_verified(&browser, &portal, &noun).await {
                    let _ = self
                        .record("in_page_goal_miss: already-there claim not verified")
                        .await;
                    self.journal_observed_failure(None, "already-there claim not verified")
                        .await;
                    return Err(AppError::WorkflowFailed(self.recent_journal(16).await));
                }
                let _ = self
                    .record(&format!(
                        "in_page_goal_done: already at {}",
                        landed.host_str().unwrap_or("?")
                    ))
                    .await;
            }
            Err(macro_engine::IntentError::NoMatch(diagnostic)) => {
                let _ = self
                    .record(&format!("in_page_goal_miss: {diagnostic}"))
                    .await;
                self.journal_observed_failure(None, &format!("miss: {diagnostic}"))
                    .await;
                let journal = self.recent_journal(16).await;
                return Err(AppError::WorkflowFailed(journal));
            }
            Err(macro_engine::IntentError::Browser(_)) => {
                let _ = self.record("in_page_goal_miss: browser error").await;
                return Err(AppError::BrowserUnavailable);
            }
            Ok(
                macro_engine::PageGoalOutcome::Verified { .. }
                | macro_engine::PageGoalOutcome::SignedOut,
            ) => {
                // `pursue_page_goal` never yields the account-home
                // variants; defensive so the enum can grow without
                // breaking this lane.
                return Err(AppError::Internal);
            }
        }
        // A pure in-page goal ends at the landing like a pure direct open:
        // the navigation IS the task, and there are no playbook steps.
        Ok(DispatchOutcome {
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
            lend_id: None,
            route_log: Some(goal_line),
            telemetry_log: None,
            final_frame: None,
            challenge: None,
            auth_url: None,
            final_url: None,
            page_title: None,
        })
    }

    /// Shared outcome builder for in-page follow-ups: a pure goal ends at
    /// the landing like a pure direct open — the navigation IS the task.
    fn in_page_goal_outcome(
        name: &str,
        goal_line: String,
        final_url: Option<String>,
    ) -> DispatchOutcome {
        DispatchOutcome {
            kind: "ephemeral",
            name: name.to_owned(),
            result: orchestration_engine::SequenceOutcome {
                completed_steps: 0,
                total_steps: 0,
                status: orchestration_engine::SequenceStatus::Completed,
                stopped_at: None,
            },
            steps: Vec::new(),
            run_id: None,
            lend_id: None,
            route_log: Some(goal_line),
            telemetry_log: None,
            final_frame: None,
            challenge: None,
            auth_url: None,
            final_url,
            page_title: None,
        }
    }

    /// `account_home` dispatch: memory fast path first, live chrome worker
    /// second, verifier-gated success, honest miss. The identity row is an
    /// observed fact from the user's own run — written automatically,
    /// announced in the journal, revoked by "Forget this site".
    /// Memory fast path for `account_home`: recall the remembered
    /// identity href, validate and navigate it, verify the live page with
    /// the account-home verb verifier. Returns `Some(outcome)` on a
    /// verified live hit, `None` on miss or stale (stale and unverified
    /// rows are deleted here). Extracted so
    /// `dispatch_account_home_goal` stays within the line budget.
    async fn recall_account_home(
        &self,
        browser: &std::sync::Arc<browser_driver::ManagedBrowser>,
        portal: &url::Url,
        origin_host: &str,
        name: &str,
        goal_line: String,
        spec: &'static orchestration_engine::VerbSpec,
    ) -> Result<Option<DispatchOutcome>, AppError> {
        let class_key = spec.kind.as_str();
        let remembered = self
            .playbooks()
            .await?
            .recall_identity(origin_host, class_key)
            .await
            .map_err(|_| AppError::StorageUnavailable)?;
        let Some(remembered) = remembered else {
            let _ = self.record("in_page_goal_memory: miss").await;
            return Ok(None);
        };
        let _ = self
            .record(&format!("in_page_goal_memory: hit {}", remembered.href))
            .await;
        if let Some(landed) = self
            .navigate_remembered_identity(browser, portal, &remembered)
            .await
        {
            // Verifier-backed completion: the remembered URL must still
            // read as the account home on the live page. Memory is a
            // shortcut to the goal, not a claim the goal was reached.
            if macro_engine::verify_verb(
                &**browser,
                portal,
                spec,
                None,
                remembered.username.as_deref(),
            )
            .await
            {
                let _ = self
                    .record(&format!("in_page_goal_done: memory → {}", landed.as_str()))
                    .await;
                return Ok(Some(Self::in_page_goal_outcome(
                    name,
                    goal_line,
                    Some(landed.as_str().to_owned()),
                )));
            }
            let _ = self
                .record(&format!(
                    "in_page_goal_memory: remembered URL no longer verifies → {}",
                    landed.as_str()
                ))
                .await;
        }
        // Stale: drop only this goal class's row and fall through to the
        // live chrome read. A stale profile row must not evict the
        // origin's settings row — the rows are independent discoveries.
        let _ = self
            .playbooks()
            .await?
            .forget_identity_for_origin_and_class(origin_host, class_key)
            .await;
        let _ = self.record("in_page_goal_memory: stale → rediscover").await;
        Ok(None)
    }

    /// Shared verb dispatch: one match on the verb kind routes to the
    /// per-verb dispatcher, each of which runs the action engine's three
    /// gears (deterministic chrome worker → generalist model loop → honest
    /// miss) with the verb's verifier as the only completion decider. The
    /// noun-led path (`dispatch_in_page_goal`) and the verb-led path
    /// (`dispatch_verb_led_action`) share this, so memory, verification,
    /// and journaling stay one implementation.
    async fn dispatch_verb_spec_goal(
        &self,
        browser: &std::sync::Arc<browser_driver::ManagedBrowser>,
        portal: &url::Url,
        name: &str,
        spec: &'static orchestration_engine::VerbSpec,
        goal_line: String,
        navigator: Option<std::sync::Arc<dyn macro_engine::PageNavigator>>,
        escalation: Option<macro_engine::ModelEscalation>,
    ) -> Result<DispatchOutcome, AppError> {
        match spec.kind {
            orchestration_engine::VerbKind::AccountHome => {
                self.dispatch_account_home_goal(
                    browser, portal, name, spec, goal_line, navigator, escalation,
                )
                .await
            }
            orchestration_engine::VerbKind::Settings => {
                self.dispatch_settings_goal(
                    browser, portal, name, spec, goal_line, navigator, escalation,
                )
                .await
            }
            orchestration_engine::VerbKind::LogOut => {
                self.dispatch_log_out_goal(
                    browser, portal, name, spec, goal_line, navigator, escalation,
                )
                .await
            }
            orchestration_engine::VerbKind::Notifications => {
                self.dispatch_notifications_goal(
                    browser, portal, name, spec, goal_line, navigator, escalation,
                )
                .await
            }
        }
    }

    /// `notifications` dispatch: no memory shape — the action engine with
    /// the surface verifier. `Verified` means the same-site notifications
    /// surface (page or revealed panel) was observed; anything else is the
    /// honest miss.
    async fn dispatch_notifications_goal(
        &self,
        browser: &std::sync::Arc<browser_driver::ManagedBrowser>,
        portal: &url::Url,
        name: &str,
        spec: &'static orchestration_engine::VerbSpec,
        goal_line: String,
        navigator: Option<std::sync::Arc<dyn macro_engine::PageNavigator>>,
        escalation: Option<macro_engine::ModelEscalation>,
    ) -> Result<DispatchOutcome, AppError> {
        match macro_engine::pursue_verb_goal(&**browser, portal, spec, navigator, escalation).await
        {
            Ok(macro_engine::PageGoalOutcome::Verified {
                label,
                landed,
                hit_lines,
                ..
            }) => {
                for line in &hit_lines {
                    let _ = self.record(line).await;
                }
                let _ = self
                    .record(&format!(
                        "in_page_goal_done: verified '{label}' → {}",
                        landed.as_str()
                    ))
                    .await;
                Ok(Self::in_page_goal_outcome(
                    name,
                    goal_line,
                    Some(landed.as_str().to_owned()),
                ))
            }
            Err(macro_engine::IntentError::NoMatch(diagnostic)) => {
                let _ = self
                    .record(&format!("in_page_goal_miss: {diagnostic}"))
                    .await;
                self.journal_observed_failure(None, &format!("miss: {diagnostic}"))
                    .await;
                let journal = self.recent_journal(16).await;
                Err(AppError::WorkflowFailed(journal))
            }
            Err(macro_engine::IntentError::Browser(_)) => {
                let _ = self.record("in_page_goal_miss: browser error").await;
                Err(AppError::BrowserUnavailable)
            }
            Ok(
                macro_engine::PageGoalOutcome::Navigated { .. }
                | macro_engine::PageGoalOutcome::AlreadyThere { .. }
                | macro_engine::PageGoalOutcome::SignedOut,
            ) => {
                // The notifications lane only yields `Verified`; defensive.
                Err(AppError::Internal)
            }
        }
    }

    /// Verb-led action dispatch: the prompt starts with a closed-vocabulary
    /// verb phrase ("log out from reddit"), detected by
    /// [`orchestration_engine::detect_verb_led_action`]. Saved replay
    /// already won (this runs in the ephemeral lanes only), so this
    /// preempts the funnel/search machinery:
    ///
    /// * empty site context ("log out") → act on the live portal; no
    ///   portal is [`AppError::SessionRequired`].
    /// * site context matching the live origin → act in-page without
    ///   grounding (the funnel's already-on-origin rule, same
    ///   alias-aware matcher; the ladder runs only on a miss).
    /// * otherwise → ground the site context through the normal site
    ///   ladder (preposition stripped, no site list), land it, then act.
    ///
    /// The verb's in-page goal runs through the shared
    /// [`Self::dispatch_verb_spec_goal`], so memory, the action engine's
    /// three gears, and the verifier wall are the same implementation the
    /// noun-led path uses. Returns unsettled like
    /// [`Self::dispatch_in_page_goal`] — the lane settles.
    async fn dispatch_verb_led_action(
        &self,
        prompt: String,
        action: orchestration_engine::VerbLedAction,
        _emit: &mut (impl FnMut(PlaybookEvent) + Send),
    ) -> Result<DispatchOutcome, AppError> {
        let spec = action.spec;
        let _ = self
            .record(&format!(
                "verb_led: '{}' · site context '{}'",
                spec.kind.as_str(),
                if action.site_text.is_empty() {
                    "(live portal)"
                } else {
                    action.site_text.as_str()
                }
            ))
            .await;
        let site = orchestration_engine::verb_site_context(&action.site_text);
        // A bare verb ("log out") or an aside-only remainder ("log out for
        // me") acts on the live portal — the remainder names no site.
        let live = self.live_portal().await;
        let (portal, entry) = if let Some(site) = site {
            // Already-on-origin: the named site IS the live portal —
            // act in-page without grounding, like the funnel does. The
            // alias-aware matcher is the same rule the funnel and the
            // directory veto share; the ladder runs only on a miss.
            let on_origin = live
                .as_ref()
                .and_then(|portal| portal.host_str())
                .is_some_and(|host| orchestration_engine::site_matches_host(&site, host));
            if on_origin {
                let portal = live.clone().ok_or(AppError::Internal)?;
                let _ = self
                    .record(&format!(
                        "verb_led: already_on_origin '{site}' ({}) · ladder skipped",
                        portal.host_str().unwrap_or("?")
                    ))
                    .await;
                (portal, None)
            } else {
                // Ground the site context through the normal ladder,
                // then land it like the cold in-page goal does.
                let (route, _) = self.resolve_site_via_ladder(&site).await;
                let Some(route) = route else {
                    let _ = self
                        .record(&format!("verb_led_miss: site='{site}' ungrounded"))
                        .await;
                    self.journal_observed_failure(None, "site ungrounded").await;
                    let journal = self.recent_journal(16).await;
                    return Err(AppError::WorkflowFailed(journal));
                };
                let valid =
                    orchestration_engine::entry_url_valid(Some(route.source), route.url.as_str());
                if !valid {
                    return Err(AppError::InvalidInput(
                        "The derived intent is not runnable.",
                    ));
                }
                let _ = self
                    .record(&format!(
                        "route_proposed: verb-led '{site}' → {} · source: {:?}",
                        route.url.as_str(),
                        route.source
                    ))
                    .await;
                let mut portal = route.url.clone();
                portal.set_path("/");
                portal.set_query(None);
                portal.set_fragment(None);
                *self.session_origin.lock().map_err(|_| AppError::Internal)? = Some(portal.clone());
                (portal, Some(route.url))
            }
        } else {
            let portal = live.clone().ok_or(AppError::SessionRequired)?;
            (portal, None)
        };
        // Background intent: reuse the attached session, never open a
        // window. With a grounded site the browser lands it first — the
        // pursuit snapshots the live page, so acting must start from the
        // destination, not about:blank. With the live portal there is no
        // navigation before acting.
        let browser = self.browser(BrowserIntent::Background).await?;
        if let Some(entry) = entry {
            macro_engine::ensure_at_entry_url(&browser, &entry)
                .await
                .map_err(|_| AppError::BrowserUnavailable)?;
        }
        let name = orchestration_engine::ephemeral_name(&prompt);
        let goal_line = format!(
            "verb_led_goal: '{}' on {}",
            spec.kind.as_str(),
            portal.host_str().unwrap_or("?")
        );
        let navigator: Option<std::sync::Arc<dyn macro_engine::PageNavigator>> =
            orchestration_engine::LlmPageNavigator::from_env()
                .map(|navigator| std::sync::Arc::new(navigator) as _);
        let _ = self
            .record(&format!(
                "in_page_goal_navigator: {}",
                if navigator.is_some() {
                    "model-guided phase armed"
                } else {
                    "deterministic only"
                }
            ))
            .await;
        let escalation = Self::model_escalation();
        let _ = self
            .record(&format!(
                "in_page_goal_escalation: {}",
                escalation
                    .as_ref()
                    .map_or("unset".to_owned(), |escalation| format!(
                        "{} armed",
                        escalation.model_name
                    ))
            ))
            .await;
        self.dispatch_verb_spec_goal(
            &browser, &portal, &name, spec, goal_line, navigator, escalation,
        )
        .await
    }

    async fn dispatch_account_home_goal(
        &self,
        browser: &std::sync::Arc<browser_driver::ManagedBrowser>,
        portal: &url::Url,
        name: &str,
        spec: &'static orchestration_engine::VerbSpec,
        goal_line: String,
        navigator: Option<std::sync::Arc<dyn macro_engine::PageNavigator>>,
        escalation: Option<macro_engine::ModelEscalation>,
    ) -> Result<DispatchOutcome, AppError> {
        let origin_host = portal.host_str().unwrap_or("?").to_lowercase();
        let class_key = spec.kind.as_str();

        // 1. Memory: a previous run revealed this origin's profile URL from
        // the live page. A live hit completes here; miss or stale falls
        // through to the chrome worker.
        if let Some(outcome) = self
            .recall_account_home(browser, portal, &origin_host, name, goal_line.clone(), spec)
            .await?
        {
            return Ok(outcome);
        }

        // 2. Live chrome worker with verifier, then the generalist model
        // loop, then the honest miss: one action engine, three gears. The
        // guest short-circuit lives inside the worker, so a signed-out
        // landing still returns `SignedOut` before the model phase.
        match macro_engine::pursue_verb_goal(&**browser, portal, spec, navigator, escalation).await
        {
            Ok(macro_engine::PageGoalOutcome::Verified {
                label,
                landed,
                username,
                hit_lines,
                ..
            }) => {
                for line in &hit_lines {
                    let _ = self.record(line).await;
                }
                let _ = self
                    .record(&format!(
                        "in_page_goal_done: verified '{label}' → {}",
                        landed.as_str()
                    ))
                    .await;
                match self
                    .playbooks()
                    .await?
                    .remember_identity(
                        &origin_host,
                        class_key,
                        username.as_deref(),
                        landed.as_str(),
                        "page_menu",
                    )
                    .await
                {
                    Ok(_) => {
                        let _ = self
                            .record(&format!(
                                "in_page_goal_memory: write {origin_host} {class_key}"
                            ))
                            .await;
                    }
                    Err(error) => {
                        let _ = self
                            .record(&format!("in_page_goal_memory: write failed ({error})"))
                            .await;
                    }
                }
                Ok(Self::in_page_goal_outcome(
                    name,
                    goal_line,
                    Some(landed.as_str().to_owned()),
                ))
            }
            Ok(macro_engine::PageGoalOutcome::SignedOut) => {
                let _ = self
                    .record("in_page_goal_signed_out: guest landing · no pursuit")
                    .await;
                // Settle's own auth probe renders the lend/Take Control
                // card from the live page — no separate error path needed.
                Ok(Self::in_page_goal_outcome(name, goal_line, None))
            }
            Ok(
                macro_engine::PageGoalOutcome::Navigated { .. }
                | macro_engine::PageGoalOutcome::AlreadyThere { .. },
            ) => {
                // `pursue_chrome_action` never yields these for the
                // account-home spec; defensive.
                Err(AppError::Internal)
            }
            Err(macro_engine::IntentError::NoMatch(diagnostic)) => {
                let _ = self
                    .record(&format!("in_page_goal_miss: {diagnostic}"))
                    .await;
                self.journal_observed_failure(None, &format!("miss: {diagnostic}"))
                    .await;
                let journal = self.recent_journal(16).await;
                Err(AppError::WorkflowFailed(journal))
            }
            Err(macro_engine::IntentError::Browser(_)) => {
                let _ = self.record("in_page_goal_miss: browser error").await;
                Err(AppError::BrowserUnavailable)
            }
        }
    }

    /// Memory fast path for `settings`: recall the remembered settings
    /// destination, validate and navigate it, then verify the live page
    /// with the settings verb verifier. Returns `Some(outcome)` on a
    /// verified live hit, `None` on miss or stale (stale and unverified
    /// rows are deleted here, scoped to the settings goal class so the
    /// origin's identity row survives). Extracted so
    /// `dispatch_settings_goal` stays within the line budget.
    async fn recall_settings_destination_fast_path(
        &self,
        browser: &std::sync::Arc<browser_driver::ManagedBrowser>,
        portal: &url::Url,
        origin_host: &str,
        name: &str,
        goal_line: String,
        spec: &'static orchestration_engine::VerbSpec,
    ) -> Result<Option<DispatchOutcome>, AppError> {
        let remembered = self
            .playbooks()
            .await?
            .recall_settings_destination(origin_host)
            .await
            .map_err(|_| AppError::StorageUnavailable)?;
        let Some(destination) = remembered else {
            let _ = self.record("in_page_goal_memory: miss").await;
            return Ok(None);
        };
        let _ = self
            .record(&format!("in_page_goal_memory: hit {}", destination.href))
            .await;
        if let Some(landed) = self
            .navigate_remembered_href(browser, portal, &destination.href, None)
            .await
        {
            // Verifier-backed completion: the remembered URL must still
            // read as the settings destination on the live page. Memory is
            // a shortcut to the goal, not a claim the goal was reached —
            // an unverified landing is stale, not a completion.
            if macro_engine::verify_verb(&**browser, portal, spec, None, None).await {
                let _ = self
                    .record(&format!("in_page_goal_done: memory → {}", landed.as_str()))
                    .await;
                return Ok(Some(Self::in_page_goal_outcome(
                    name,
                    goal_line,
                    Some(landed.as_str().to_owned()),
                )));
            }
            let _ = self
                .record(&format!(
                    "in_page_goal_memory: remembered URL no longer verifies → {}",
                    landed.as_str()
                ))
                .await;
        }
        // Stale: drop only the settings row and fall through to the live
        // settings worker — the origin's identity row (if any) is a
        // different goal class and survives.
        let _ = self
            .playbooks()
            .await?
            .forget_identity_for_origin_and_class(
                origin_host,
                orchestration_engine::VerbKind::Settings.as_str(),
            )
            .await;
        let _ = self.record("in_page_goal_memory: stale → rediscover").await;
        Ok(None)
    }

    /// `settings` dispatch: memory fast path first, live settings action
    /// engine second (chrome worker → model loop → honest miss),
    /// verifier-gated memory write. Mirrors
    /// `dispatch_account_home_goal`'s shape: the memory row is an observed
    /// fact from the user's own run — written automatically from a
    /// verified landing only, announced in the journal, revoked by
    /// "Forget this site". An unverified navigation is checked against
    /// the live page and becomes the honest miss when it fails — it never
    /// completes the run, and never becomes memory.
    async fn dispatch_settings_goal(
        &self,
        browser: &std::sync::Arc<browser_driver::ManagedBrowser>,
        portal: &url::Url,
        name: &str,
        spec: &'static orchestration_engine::VerbSpec,
        goal_line: String,
        navigator: Option<std::sync::Arc<dyn macro_engine::PageNavigator>>,
        escalation: Option<macro_engine::ModelEscalation>,
    ) -> Result<DispatchOutcome, AppError> {
        let origin_host = portal.host_str().unwrap_or("?").to_lowercase();

        // 1. Memory: a previous run verified this origin's settings URL. A
        // live hit completes here; miss or stale falls through to the
        // settings worker.
        if let Some(outcome) = self
            .recall_settings_destination_fast_path(
                browser,
                portal,
                &origin_host,
                name,
                goal_line.clone(),
                spec,
            )
            .await?
        {
            return Ok(outcome);
        }

        // 2. Live settings worker, then the generalist model loop, then the
        // honest miss: one action engine, three gears. The verifier is the
        // only completion decider — an unverified `Navigated` is checked
        // against the live page here and becomes a miss when it fails.
        match macro_engine::pursue_verb_goal(&**browser, portal, spec, navigator, escalation).await
        {
            Ok(macro_engine::PageGoalOutcome::Navigated {
                label,
                landed,
                hit_lines,
            }) => {
                for line in &hit_lines {
                    let _ = self.record(line).await;
                }
                // The worker reached a token-named landing but could not
                // verify it from the page itself. The run completes only
                // when the live page now passes the verb's verifier —
                // memory still stays untouched on this arm: memory rows are
                // the deterministic worker's own verified landings.
                if macro_engine::verify_verb(&**browser, portal, spec, Some(&label), None).await {
                    let _ = self
                        .record(&format!(
                            "in_page_goal_done: '{label}' → {} (verified at completion)",
                            landed.as_str()
                        ))
                        .await;
                    Ok(Self::in_page_goal_outcome(
                        name,
                        goal_line,
                        Some(landed.as_str().to_owned()),
                    ))
                } else {
                    let _ = self
                        .record(&format!(
                            "in_page_goal_miss: '{label}' → {} (navigation not verified)",
                            landed.as_str()
                        ))
                        .await;
                    self.journal_observed_failure(Some(&label), "navigation not verified")
                        .await;
                    let journal = self.recent_journal(16).await;
                    Err(AppError::WorkflowFailed(journal))
                }
            }
            Ok(macro_engine::PageGoalOutcome::Verified {
                label,
                landed,
                hit_lines,
                ..
            }) => {
                for line in &hit_lines {
                    let _ = self.record(line).await;
                }
                let _ = self
                    .record(&format!(
                        "in_page_goal_done: verified '{label}' → {}",
                        landed.as_str()
                    ))
                    .await;
                match self
                    .playbooks()
                    .await?
                    .remember_settings_destination(&origin_host, landed.as_str())
                    .await
                {
                    Ok(_) => {
                        let _ = self
                            .record(&format!(
                                "in_page_goal_memory: write {origin_host} settings"
                            ))
                            .await;
                    }
                    Err(error) => {
                        let _ = self
                            .record(&format!("in_page_goal_memory: write failed ({error})"))
                            .await;
                    }
                }
                Ok(Self::in_page_goal_outcome(
                    name,
                    goal_line,
                    Some(landed.as_str().to_owned()),
                ))
            }
            Err(macro_engine::IntentError::NoMatch(diagnostic)) => {
                let _ = self
                    .record(&format!("in_page_goal_miss: {diagnostic}"))
                    .await;
                self.journal_observed_failure(None, &format!("miss: {diagnostic}"))
                    .await;
                let journal = self.recent_journal(16).await;
                Err(AppError::WorkflowFailed(journal))
            }
            Err(macro_engine::IntentError::Browser(_)) => {
                let _ = self.record("in_page_goal_miss: browser error").await;
                Err(AppError::BrowserUnavailable)
            }
            Ok(
                macro_engine::PageGoalOutcome::AlreadyThere { .. }
                | macro_engine::PageGoalOutcome::SignedOut,
            ) => {
                // `pursue_chrome_action` never yields these for the settings
                // spec; defensive.
                Err(AppError::Internal)
            }
        }
    }

    /// `log_out` dispatch: no memory shape — a signed-out session leaves
    /// nothing to remember — just the spec-parameterized action engine with
    /// the signed-out verifier. `Verified` means the live page reads signed
    /// out after the click; `AlreadyThere` means the pre-click probe read
    /// signed out, and it is accepted only after the verifier still reads
    /// signed out — the probe alone is not completion evidence.
    async fn dispatch_log_out_goal(
        &self,
        browser: &std::sync::Arc<browser_driver::ManagedBrowser>,
        portal: &url::Url,
        name: &str,
        spec: &'static orchestration_engine::VerbSpec,
        goal_line: String,
        navigator: Option<std::sync::Arc<dyn macro_engine::PageNavigator>>,
        escalation: Option<macro_engine::ModelEscalation>,
    ) -> Result<DispatchOutcome, AppError> {
        match macro_engine::pursue_verb_goal(&**browser, portal, spec, navigator, escalation).await
        {
            Ok(macro_engine::PageGoalOutcome::Verified {
                label,
                landed,
                hit_lines,
                tried_lines,
                ..
            }) => {
                for line in &tried_lines {
                    let _ = self.record(line).await;
                }
                for line in &hit_lines {
                    let _ = self.record(line).await;
                }
                let _ = self
                    .record(&format!(
                        "in_page_goal_done: signed out via '{label}' → {}",
                        landed.as_str()
                    ))
                    .await;
                Ok(Self::in_page_goal_outcome(
                    name,
                    goal_line,
                    Some(landed.as_str().to_owned()),
                ))
            }
            Ok(macro_engine::PageGoalOutcome::AlreadyThere { landed }) => {
                // Completion is verifier-backed: re-read the live auth
                // state now, because the probe that produced `AlreadyThere`
                // may predate a page change. A signed-in re-read is a
                // miss, never a completion.
                if macro_engine::verify_verb(&**browser, portal, spec, None, None).await {
                    let _ = self
                        .record(&format!(
                            "in_page_goal_done: already signed out at {}",
                            landed.as_str()
                        ))
                        .await;
                    Ok(Self::in_page_goal_outcome(
                        name,
                        goal_line,
                        Some(landed.as_str().to_owned()),
                    ))
                } else {
                    let _ = self
                        .record("in_page_goal_miss: page no longer reads signed out")
                        .await;
                    self.journal_observed_failure(None, "page no longer reads signed out")
                        .await;
                    let journal = self.recent_journal(16).await;
                    Err(AppError::WorkflowFailed(journal))
                }
            }
            Err(macro_engine::IntentError::NoMatch(diagnostic)) => {
                let _ = self
                    .record(&format!("in_page_goal_miss: {diagnostic}"))
                    .await;
                self.journal_observed_failure(None, &format!("miss: {diagnostic}"))
                    .await;
                let journal = self.recent_journal(16).await;
                Err(AppError::WorkflowFailed(journal))
            }
            Err(macro_engine::IntentError::Browser(_)) => {
                let _ = self.record("in_page_goal_miss: browser error").await;
                Err(AppError::BrowserUnavailable)
            }
            Ok(
                macro_engine::PageGoalOutcome::Navigated { .. }
                | macro_engine::PageGoalOutcome::SignedOut,
            ) => {
                // `pursue_chrome_action` never yields these for the log-out
                // spec; defensive.
                Err(AppError::Internal)
            }
        }
    }

    /// Validate a remembered href before navigating: absolute `https`,
    /// no embedded credentials, same site as the portal (modulo `www.`),
    /// non-root path. Identity rows additionally require the path to
    /// still carry the remembered username; settings destinations carry
    /// none. Pure — unit-provable without a browser;
    /// `navigate_remembered_href` runs the same check before touching
    /// the browser.
    pub fn validate_remembered_href(
        href: &str,
        portal: &url::Url,
        username: Option<&str>,
    ) -> Option<url::Url> {
        let url = url::Url::parse(href).ok()?;
        if url.scheme() != "https" || url.path() == "/" || url.path().is_empty() {
            return None;
        }
        // A remembered href is navigation-only: credentials embedded in
        // it are rejected, never sent.
        if !url.username().is_empty() || url.password().is_some() {
            return None;
        }
        if !macro_engine::same_site_host(
            url.host_str().unwrap_or(""),
            portal.host_str().unwrap_or(""),
        ) {
            return None;
        }
        if let Some(username) = username
            && !url.path().to_lowercase().contains(&username.to_lowercase())
        {
            return None;
        }
        Some(url)
    }

    /// Navigate a validated remembered href, then confirm the live page
    /// agrees. Returns the live URL on agreement, `None` when anything
    /// disagrees (stale row, redirect to login, …). Shared by the
    /// account-home and settings memory fast paths — the same navigation
    /// and live-page checks, never reimplemented per goal class.
    async fn navigate_remembered_href(
        &self,
        browser: &std::sync::Arc<browser_driver::ManagedBrowser>,
        portal: &url::Url,
        href: &str,
        username: Option<&str>,
    ) -> Option<url::Url> {
        let url = Self::validate_remembered_href(href, portal, username)?;
        browser.navigate(&url).await.ok()?;
        let current = browser.current_url().await.ok().flatten()?;
        // The live page is the truth: a logout or account switch redirects
        // away from the remembered URL, which must read as stale.
        if !macro_engine::same_site_host(
            current.host_str().unwrap_or(""),
            url.host_str().unwrap_or(""),
        ) {
            return None;
        }
        if current.path() == "/" || current.path().is_empty() {
            return None;
        }
        if let Some(username) = username
            && !current
                .path()
                .to_lowercase()
                .contains(&username.to_lowercase())
        {
            return None;
        }
        Some(current)
    }

    /// Navigate a remembered identity href after Rust-side validation, then
    /// confirm the live page agrees. Returns the live URL on agreement,
    /// `None` when anything disagrees (stale row, redirect to login, …).
    async fn navigate_remembered_identity(
        &self,
        browser: &std::sync::Arc<browser_driver::ManagedBrowser>,
        portal: &url::Url,
        remembered: &playbook_store::IdentityMemory,
    ) -> Option<url::Url> {
        self.navigate_remembered_href(
            browser,
            portal,
            &remembered.href,
            remembered.username.as_deref(),
        )
        .await
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
            Ok(macro_engine::ExecuteOutcome::Completed(clicks)) => {
                // The consented plan executed: one `click_hit_test:` line
                // of per-click evidence per dispatch.
                for click in &clicks {
                    let _ = self.record(&click.hit_line).await;
                }
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
                reason: macro_engine::CLICK_FAILED_REASON,
                clicks_completed,
                failed_candidate_index,
                failed_candidate_label,
                ..
            }) => {
                Self::finish_batch_run(journal.as_ref(), &journal_id, "failed", clicks_completed)
                    .await;
                let _ = self
                    .record(&Self::batch_click_failure_line(
                        clicks_completed,
                        failed_candidate_index,
                        &failed_candidate_label,
                    ))
                    .await;
                Self::emit_batch_blocked(&events, run_id);
                Ok(outcome(
                    orchestration_engine::SequenceStatus::Failed,
                    clicks_completed,
                    Some(failed_candidate_index),
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
    pub fn count_noun_matches(elements: &[browser_driver::AxElement], noun: &str) -> usize {
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
    pub fn snapshot_telemetry_line(total_nodes: usize, noun_matches: usize, noun: &str) -> String {
        format!(
            "ax_snapshot_telemetry: total_nodes={total_nodes}, target_noun_matches={noun_matches} (noun='{noun}')"
        )
    }

    /// Combine one run's journaled diagnostic lines into the multi-line
    /// `telemetryLog` surfaced in Session Activity: resync counter, resync
    /// check, resync action(s), then snapshot stats — in journal order.
    /// `None` when nothing was journaled, so paths that never snapshot
    /// stay silent instead of emitting an empty block.
    pub fn combine_telemetry(lines: &[String]) -> Option<String> {
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }

    /// Journal one diagnostic line fail-open and hand it back, so the exact
    /// text in `session_events` also travels on the outcome for immediate
    /// UI render.
    pub async fn journal_line(&self, line: String) -> String {
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
    pub fn snapshot_journal_lines(
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
    pub fn intent_approval_content(
        approval: &orchestration_engine::IntentApproval,
    ) -> ApprovalContent {
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
    pub async fn approve_batch(
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
    pub fn batch_outcome(
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
            lend_id: None,
            route_log,
            telemetry_log,
            // Settle (final frame + L1 challenge handling) runs in
            // settle_ephemeral_outcome after this returns; the batch outcome
            // carries neither itself.
            final_frame: None,
            challenge: None,
            auth_url: None,
            final_url: None,
            page_title: None,
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
                window_mode: browser.window_mode(),
            },
            None => ContextStatus {
                attached: false,
                headless: true,
                window_mode: browser_driver::WindowMode::Offscreen,
            },
        })
    }

    /// Best-effort one-shot viewport capture of the attached browser, for a
    /// settling run's final frame. Peeks at the live session without
    /// launching. Returns the frame plus, when capture fails, a static
    /// failure label so the caller can surface the cause on the outcome —
    /// the run outcome itself is never affected by a capture failure.
    ///
    /// A capture raced by a committing navigation fails with
    /// [`browser_driver::BrowserError::PageChanging`]; a bounded settle
    /// poll plus retries cover a still-loading page. Any remaining failure
    /// is journaled with a static label — never the error's Debug, which
    /// could carry a payload — so a missing preview stays diagnosable
    /// instead of silently empty.
    async fn capture_final_frame(&self) -> (Option<String>, Option<String>) {
        let browser = self.browser.lock().ok().and_then(|guard| (*guard).clone());
        let Some(browser) = browser else {
            return (None, None);
        };
        // Settle: don't photograph a half-loaded page. Bounded and
        // fail-open — a dead page reads as complete, and the attempt below
        // surfaces the real error with its label.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !browser.document_complete().await {
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        // SPAs keep fetching after readyState: wait for network/DOM
        // quietness (no new resources, no mutations across two 250ms
        // polls) so the frame doesn't freeze a loading spinner. Same
        // bounded, fail-open contract as above.
        let quiet_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut quiet_polls = 0;
        while quiet_polls < 2 {
            if std::time::Instant::now() >= quiet_deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            if browser.page_quiet().await {
                quiet_polls += 1;
            } else {
                quiet_polls = 0;
            }
        }
        // A capture raced by a committing navigation surfaces as
        // PageChanging; give the new document a bounded moment to settle,
        // then retry. Other failures get no retry: the page is gone or the
        // connection is broken, and waiting cannot fix that.
        let mut attempt = browser.viewport().await;
        let mut retries = 0;
        while matches!(attempt, Err(browser_driver::BrowserError::PageChanging)) && retries < 2 {
            retries += 1;
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            attempt = browser.viewport().await;
        }
        match attempt {
            Ok(viewport) => (Some(viewport.data), None),
            Err(error) => {
                let label = Self::final_frame_failure_label(&error);
                self.journal_line(format!("final_frame_capture_failed: {label}"))
                    .await;
                (None, Some(label.to_string()))
            }
        }
    }

    /// Static failure label for a final-frame capture error — never the
    /// error's Debug, which could carry a payload.
    pub fn final_frame_failure_label(error: &browser_driver::BrowserError) -> &'static str {
        match error {
            browser_driver::BrowserError::Timeout => "timeout",
            browser_driver::BrowserError::Connection => "connection",
            browser_driver::BrowserError::PageChanging => "page changing",
            browser_driver::BrowserError::Launch => "launch",
            browser_driver::BrowserError::InvalidAction => "invalid action",
            _ => "unexpected",
        }
    }

    /// Attach a final-frame capture to the outcome. A capture failure is
    /// appended to the card's telemetry as `final_frame_capture_failed:
    /// <label>` — the journal line alone is invisible in the thread, and a
    /// settled card with no frame and no explanation reads as broken.
    async fn attach_final_frame(&self, outcome: &mut DispatchOutcome) {
        let (frame, failure) = self.capture_final_frame().await;
        outcome.final_frame = frame;
        if let Some(label) = failure {
            let line = format!("final_frame_capture_failed: {label}");
            outcome.telemetry_log = Some(match outcome.telemetry_log.take() {
                Some(existing) => format!("{existing}\n{line}"),
                None => line,
            });
        }
        // Browser chrome for the preview overlay: what page the run
        // settled on. Fail-open — a dead target yields `None`s and the
        // overlay falls back to the entry's anchor host.
        let (final_url, page_title) = self.describe_final_page().await;
        outcome.final_url = final_url;
        outcome.page_title = page_title;
    }

    /// Failure-path counterpart of the settle caption: re-read the live
    /// page and journal what was observed alongside the honest reason, so
    /// the journal payload carried by `WorkflowFailed` states where the
    /// run stopped. Fail-open.
    async fn journal_observed_failure(&self, acted_on: Option<&str>, reason: &str) {
        let (final_url, page_title) = self.describe_final_page().await;
        let caption = caption_observed_state(
            ObservedVerdict::Failed,
            &ObservedState {
                final_url: final_url.as_deref(),
                page_title: page_title.as_deref(),
                acted_on,
                note: Some(reason),
            },
        );
        let _ = self.record(&caption).await;
    }

    /// Best-effort final-page description: the live page's URL plus
    /// document title. `(None, None)` when no browser is attached or the
    /// target died — never an error, never a guessed value.
    async fn describe_final_page(&self) -> (Option<String>, Option<String>) {
        let browser = self.browser.lock().ok().and_then(|guard| (*guard).clone());
        let Some(browser) = browser else {
            return (None, None);
        };
        let url = browser
            .current_url()
            .await
            .ok()
            .flatten()
            .map(|url| url.to_string());
        let title = browser.page_title().await;
        (url, title)
    }

    /// Kind-aware challenge verdict on the attached browser's live page:
    /// the page URL plus whether L1 auto-escalation is worth trying
    /// (`Interstitial`) or must be skipped (`InteractiveGate`).
    async fn detect_challenge_kind(&self) -> Option<(String, ChallengeKind)> {
        let browser = self
            .browser
            .lock()
            .ok()
            .and_then(|guard| (*guard).clone())?;
        browser.challenge_detected_kind().await
    }

    /// Settle-time auth reading on the attached browser's live page.
    /// Fail-open: a detached browser or dead target yields
    /// [`AuthState::Unknown`], never a card. Callers check
    /// [`Self::detect_challenge_kind`] first so a gated page never
    /// reaches the auth-sync card.
    async fn detect_auth_state(&self) -> AuthState {
        let browser = self.browser.lock().ok().and_then(|guard| (*guard).clone());
        match browser {
            Some(browser) => browser.auth_state().await,
            None => AuthState::Unknown,
        }
    }

    /// Revoke a persisted session: delete every cookie the app profile
    /// holds for `host`. The "Forget this site" control behind a synced
    /// badge — the exit path for "sync once, stay logged in". The daily
    /// browser is untouched (one-way was never violated); only Clinch's
    /// own copy is cleared, so the next run reads the site as logged out.
    pub async fn forget_site_session(&self, host: String) -> Result<usize, AppError> {
        let host = host.trim().to_lowercase();
        if host.is_empty() || host.contains(['/', ':', '@', '?', '#', ' ']) {
            return Err(AppError::InvalidInput(
                "That doesn't look like a site host.",
            ));
        }
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        let Some(browser) = self.browser.lock().ok().and_then(|guard| (*guard).clone()) else {
            return Err(AppError::InvalidInput(
                "The managed browser isn't attached right now.",
            ));
        };
        let cleared = browser
            .clear_host_cookies(&host)
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        self.journal_line(format!(
            "session_forgotten: {host} · {cleared} cookies cleared"
        ))
        .await;
        // Identity memory dies with the session: a signed-out origin must
        // never be navigated from a stale remembered destination. The
        // full clear covers identity and settings rows alike.
        if let Ok(store) = self.playbooks().await
            && let Ok(rows) = store.forget_site(&host).await
            && rows > 0
        {
            self.journal_line(format!(
                "identity_forgotten: {host} · {rows} remembered destination(s) cleared"
            ))
            .await;
        }
        Ok(cleared)
    }

    /// Gracefully shut down the attached (headed, post-escalation) browser
    /// and detach it, so the next acquisition lazily launches off-screen headed. The
    /// app profile on disk keeps any clearance cookies; a fresh challenge
    /// simply escalates again. Only used after a *cleared* escalation — a
    /// persistent challenge keeps the headed session alive for L2 Take
    /// Control.
    async fn stand_down_escalated_browser(&self) {
        let previous = self.browser.lock().ok().and_then(|mut guard| guard.take());
        if let Some(browser) = previous {
            // Best-effort: the run already completed. A failed shutdown is
            // journaled so a lingering headed process stays visible.
            if browser.shutdown().await.is_err() {
                self.journal_line("challenge_browser_shutdown_failed".to_string())
                    .await;
            }
        }
    }

    /// L1 challenge escalation: restart the attached session as off-screen
    /// headed (same app-owned profile, cookies carried in memory by the
    /// restart path) and re-probe the challenged URL. Returns `true` when
    /// the interstitial cleared with no human input.
    ///
    /// Bounded: ~30s of polling, then `false` — a real human checkbox never
    /// clears on its own, and the L2 Take Control card is the honest
    /// fallback for exactly that case. Clearance additionally requires the
    /// page to be live on the challenged host: the detector fails closed,
    /// so a `None` from a dead target or a still-loading page never reads
    /// as "cleared". Never launches when nothing is
    /// attached: the caller only escalates a detected challenge, so the
    /// session always exists here.
    async fn auto_escalate_challenge(&self, challenge_url: &str) -> Result<bool, AppError> {
        let parsed = url::Url::parse(challenge_url)
            .map_err(|_| AppError::InvalidInput("Challenge escalation needs a valid page URL."))?;
        if parsed.scheme() != "https" {
            return Err(AppError::InvalidInput(
                "Challenge escalation needs an HTTPS page URL.",
            ));
        }
        // The restart carries cookies in memory and swaps the attached
        // session: the run continues in the headed browser afterwards —
        // no cookie handoff back to an off-screen instance mid-run.
        let browser = self.browser(BrowserIntent::ChallengeEscalation).await?;
        browser
            .navigate(&parsed)
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        let expected_host = parsed.host_str().map(str::to_owned);
        // Cloudflare's automatic verification resolves on its own in a
        // headed browser; poll the detector rather than sleeping blind.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            // Clearance needs a live page on the intended host: the
            // detector fails closed, so a `None` from a dead CDP target
            // or a still-loading page must never read as "cleared".
            let live_host = browser
                .current_url()
                .await
                .ok()
                .flatten()
                .and_then(|url| url.host_str().map(str::to_owned));
            if live_host == expected_host && browser.challenge_detected().await.is_none() {
                // Slow cold starts can create the top-level window after the
                // launch-time sweep; hide again now that the outcome is known.
                browser.hide_windows().await;
                return Ok(true);
            }
            if std::time::Instant::now() >= deadline {
                // Same re-hide as on clearance: the headed session stays
                // attached for L2 Take Control, and must not leave a
                // taskbar button while it waits for the user.
                browser.hide_windows().await;
                return Ok(false);
            }
        }
    }

    /// Lazily attach the app-owned background Chromium (off-screen headed:
    /// no visible window, dedicated Clinch profile) and stream its viewport into
    /// `emit` until released, retaken, or re-acquired. Reuses the live
    /// session when one is already attached. Nothing launches on startup
    /// or on status reads — only dispatch, sync flows, and this call
    /// attach. Fails closed when Chromium cannot start.
    pub async fn acquire_context(
        &self,
        emit: impl Fn(browser_driver::ScreencastFrame) + Send + 'static,
        emit_cursor: impl Fn(browser_driver::CursorEvent) + Send + Sync + 'static,
    ) -> Result<ContextStatus, AppError> {
        let browser = self.browser(BrowserIntent::Background).await?;
        // The cursor sink lives on the browser so input dispatch deep in the
        // action engine can reach the UI; it is replaced on every acquire
        // and cleared on release, so a later session never inherits it.
        browser.set_cursor_emitter(Some(std::sync::Arc::new(emit_cursor)));
        // Subscribe before starting: the opening frames are lost to an
        // unregistered listener otherwise.
        let mut frames = browser
            .screencast_frames()
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        browser
            .start_screencast()
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        if let Ok(mut pump) = self.screencast.lock() {
            if let Some(handle) = pump.take() {
                handle.abort();
            }
            let forwarding = browser.clone();
            *pump = Some(tokio::spawn(async move {
                while let Some(event) = frames.next().await {
                    // Latch the session for the cursor filter before the frame
                    // goes out: any cursor the agent emits from here on is
                    // attributable to this stream.
                    forwarding.note_screencast_session(event.session_id);
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
            window_mode: browser.window_mode(),
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
            // Drop the cursor sink first: no input dispatch after this point
            // may address the UI, and the next acquire installs a fresh one.
            browser.set_cursor_emitter(None);
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
    /// preview card. When `url` is given it must be an absolute HTTPS URL
    /// without credentials; the headed window opens directly on it. This is
    /// the challenge-takeover path: the user solves a human-verification
    /// gate once in a real window, and the shared profile keeps the
    /// clearance for later background runs.
    pub async fn take_control(&self, url: Option<String>) -> Result<ContextStatus, AppError> {
        // The one and only path that may put a window on screen.
        let browser = self.browser(BrowserIntent::Interactive).await?;
        if let Some(url) = url {
            if !orchestration_engine::entry_url_valid(
                Some(orchestration_engine::RouteSource::ExplicitDomain),
                &url,
            ) {
                return Err(AppError::InvalidInput(
                    "Takeover needs an absolute HTTPS page URL without credentials.",
                ));
            }
            let parsed = url::Url::parse(&url)
                .map_err(|_| AppError::InvalidInput("Takeover needs a valid page URL."))?;
            browser
                .navigate(&parsed)
                .await
                .map_err(|_| AppError::BrowserUnavailable)?;
        }
        Ok(ContextStatus {
            attached: true,
            headless: browser.is_headless(),
            window_mode: browser.window_mode(),
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
        _ => AppError::WorkflowFailed(Vec::new()),
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
