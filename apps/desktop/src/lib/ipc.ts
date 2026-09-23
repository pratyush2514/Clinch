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
 * over a `tauri::ipc::Channel` handed to the command as an argument. The one
 * global event is the screencast frame pump.
 */

/** The only global Tauri event the backend emits. */
export const SCREENCAST_EVENT = "browser-screencast-frame";

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

export type SyncRequest = {
  browser: string;
  profile: string;
  portalUrl: string;
  consent: boolean;
};

/** Portal address plus a `ReauthReason`; never a secret. */
export type AuthPanelState = { portal: string; reason: string };

export type ContextStatus = { attached: boolean; headless: boolean };

/** No `rename_all` on the Rust struct, so the id stays snake_case. */
export type ScreencastFrame = { data: string; session_id: number };

export type Viewport = { data: string; width: number; height: number };

export type Highlight = {
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

export type SequencePhase = "started" | "running" | "completed" | "blocked";

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
  routeLog?: string | null;
  telemetryLog?: string | null;
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

export type WaitCondition = { selector: string; timeoutMs: number } | null;

export type TaskState =
  | "planned"
  | "running"
  | "needs_repair"
  | "completed"
  | "failed"
  | "interrupted";

export type StepState =
  | "pending"
  | "running"
  | "completed"
  | "needs_repair"
  | "failed"
  | "interrupted";

export type DownloadedFile = { path: string; bytes: number };

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
