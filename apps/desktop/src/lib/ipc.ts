/**
 * Wire shapes for the Tauri surface.
 *
 * Every type here mirrors a `serde` struct in `src-tauri`. Field names are the
 * camelCase the backend emits, with two deliberate exceptions that follow the
 * Rust derives rather than the convention: `SessionStatus` is an
 * internally-tagged enum on `state`, and `ScreencastFrame` carries no
 * `rename_all`, so its second field stays `session_id` on the wire.
 *
 * Progress and approvals never travel on the global event bus — they stream
 * over a `tauri::ipc::Channel` handed to the command as an argument. The
 * global events are the screencast frame pump and the agent cursor positions.
 */

/** The only global Tauri event the backend emits. */
export const SCREENCAST_EVENT = "browser-screencast-frame";

/** The agent's synthetic pointer position, emitted per input dispatch. */
export const CURSOR_EVENT = "browser-cursor-moved";

/** Notifies sibling views that the saved-playbook list changed. */
export const PLAYBOOKS_CHANGED = "clinch:playbooks-changed";

export type StorageStatus = {
  ready: boolean;
  cookieImportSupported: boolean;
  defaultBrowser: string;
  startupBuild: string;
};

export type SessionStatus =
  | { state: "cookies_imported"; count: number }
  | { state: "manual_login"; reason: string | null };

/** Portal address plus a `ReauthReason`; never a secret. */
export type AuthPanelState = { portal: string; reason: string };

export type ContextStatus = { attached: boolean; headless: boolean; windowMode: "headed" | "offscreen" };

/** No `rename_all` on the Rust struct, so the id stays snake_case. */
export type ScreencastFrame = { data: string; session_id: number };

/**
 * One synthetic pointer position from the agent's trusted-input dispatch.
 * Coordinates are page CSS pixels relative to the viewport origin;
 * `viewport_width`/`viewport_height` name that space, and `session_id`
 * matches the screencast frame latch (the backend sends -1 before the first
 * frame, which the hook drops like a stale session).
 */
export type AgentCursor = {
  x: number;
  y: number;
  kind: "move" | "press" | "release";
  viewport_width: number;
  viewport_height: number;
  session_id: number;
};

type Highlight = {
  selector: string;
  x: number;
  y: number;
  width: number;
  height: number;
  matches: number;
};

/** One queued control on a batch approval card, in document order. */
export type CandidatePreview = {
  index: number;
  label: string;
  role: string;
  isLandmark: boolean;
  container: string | null;
};

export type PlaybookApproval = {
  runId: number;
  stepIndex: number;
  kind: string;
  summary: string;
  candidates: CandidatePreview[];
};

type SequencePhase = "started" | "running" | "completed" | "blocked";

export type PlaybookEvent = {
  runId: number;
  stepIndex: number;
  totalSteps: number;
  phase: SequencePhase;
  highlight: Highlight | null;
  approval: PlaybookApproval | null;
};

export type SequenceOutcome = {
  completedSteps: number;
  totalSteps: number;
  status: string;
  stoppedAt: number | null;
};

export type DispatchOutcome = {
  /** `"saved"` for a stored replay, `"ephemeral"` for a resolved-now run. */
  kind: string;
  name: string;
  result: SequenceOutcome;
  /** Echoed so an ad-hoc run can be saved without rebuilding it client-side. */
  steps: unknown[];
  /** Registry key for `save_run_as_workflow`; absent unless remembered. */
  runId?: string | null;
  /** Registry key for `lend_session` consent taps (challenge and auth-sync
   * cards). Decoupled from `runId`: pure direct opens are deliberately not
   * remembered for save-as-workflow, but their card tap still carries a key. */
  lendId?: string | null;
  routeLog?: string | null;
  telemetryLog?: string | null;
  /** One-shot JPEG viewport (base64) captured when the run settled. */
  finalFrame?: string | null;
  /** Page URL when the run settled on a bot-mitigation interstitial. */
  challenge?: string | null;
  /** Page URL when the run settled on a clean signed-out guest landing.
   * When set (and `challenge` is absent) the thread renders the auth-sync
   * card: sync the daily browser's session via the Companion bridge, or
   * Take Control and log in by hand. Additive. */
  authUrl?: string | null;
  /** Live page URL when the run settled, for the preview overlay chrome. */
  finalUrl?: string | null;
  /** Live page document title when the run settled, for the overlay tab. */
  pageTitle?: string | null;
};

/** Result of one session-lend attempt (`lend_session`). */
export type LendOutcome = {
  cleared: boolean;
  cookiesLent: number;
  reason: string | null;
  /** Fresh settle-time frame when the gate cleared. */
  finalFrame: string | null;
};

/** One connected Companion, as reported by `bridge_status`. `id` is the
 * server-side connection key the card's source picker passes back as
 * `sourceConnectionId`. */
export type BridgeConnectionInfo = {
  id: number;
  /** Browser brand from the companion's HELLO ("Brave", "Chrome", …);
   * empty when the companion predates identity. */
  browser: string;
  /** Companion install id; empty when unknown. Display truncated. */
  installId: string;
  connectedAtSecs: number;
};

/** `bridge_status`: the loopback bridge plus every attached companion. */
export type BridgeStatus = {
  running: boolean;
  port: number;
  extensions: number;
  connections: BridgeConnectionInfo[];
};

export type PlaybookSummary = {
  id: string;
  name: string;
  portalUrl: string;
  stepCount: number;
  updatedAt: string;
};

/** A user-saved site shortcut: the direct-open ladder's learned rung. */
export type SiteShortcut = {
  name: string;
  url: string;
};

export type Action =
  | { type: "navigate"; url: string }
  | { type: "click"; selector: string }
  | { type: "submit"; selector: string }
  | { type: "download_links"; selector: string }
  | { type: "fill"; selector: string; value: string };

type WaitCondition = { selector: string; timeoutMs: number } | null;

type TaskState =
  | "planned"
  | "running"
  | "needs_repair"
  | "completed"
  | "failed"
  | "interrupted";

type StepState =
  | "pending"
  | "running"
  | "completed"
  | "needs_repair"
  | "failed"
  | "interrupted";

type DownloadedFile = { path: string; bytes: number };

export type Task = {
  id: number;
  revision: number;
  workflow: string;
  mode: "record" | "replay";
  state: TaskState;
  elapsedMs: number;
  plan: {
    recording: { steps: { action: Action; wait: WaitCondition }[] };
    steps: { state: StepState; elapsedMs: number; output: { files: DownloadedFile[] } }[];
  };
  repair: { stepIndex: number; selector: string; stage: "target" | "wait"; issue: string } | null;
  failure: string | null;
};

export type TaskGate = { taskId: number; stepIndex: number; action: Action };

export type TaskEvent = { task: Task; highlight: Highlight | null; approval: TaskGate | null };
