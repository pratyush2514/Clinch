/**
 * Backend errors arrive as `AppError`, an adjacently-tagged enum serialized as
 * `{ code, message? }`. The code is the stable contract; the copy below is the
 * one place that turns each code into something a person can act on.
 * `workflow_failed` may carry a string array of journal lines as its message.
 */

/** The `AppError` discriminant, when the rejection carries one. */
export function errorCode(error: unknown): string | null {
  if (typeof error === "object" && error !== null && "code" in error) {
    const { code } = error as { code: unknown };
    if (typeof code === "string") return code;
  }
  return null;
}

export function message(error: unknown): string {
  if (typeof error === "object" && error !== null && "code" in error) {
    if (error.code === "browser_unavailable")
      return "Chromium could not be started. If this keeps happening, check CLINCH_CHROMIUM_PATH, then close and retry.";
    if (error.code === "busy") return "An operation is already in progress.";
    if (error.code === "storage_unavailable")
      return "Local storage is unavailable. Check app data permissions.";
    if (error.code === "session_required")
      return "Connect this portal and finish signing in in the managed browser before running tasks.";
    if (error.code === "workflow_failed") {
      // Failed runs carry their journal lines as the message payload
      // (see AppError::WorkflowFailed): surface the evidence instead of
      // the generic card copy.
      if ("message" in error && Array.isArray(error.message)) {
        const lines = (error.message as unknown[]).filter(
          (line): line is string => typeof line === "string" && line.length > 0,
        );
        if (lines.length > 0) return lines.join("\n");
      }
      return "The workflow could not finish. Check its saved macro and the latest task checkpoint; no automatic retry was attempted.";
    }
    if (error.code === "picker_unavailable")
      return "Element picking needs the visible managed Chromium window. Replays run headless — click Sync session to reopen it, then pick again.";
    if ("message" in error && typeof error.message === "string") return error.message;
  }
  return "The operation could not finish. Please retry.";
}

/**
 * Whether this failure is fixed by connecting a portal rather than by
 * retrying. The thread offers a one-click sign-in affordance on these instead
 * of leaving the user to find the command palette.
 */
export function needsSession(code: string | null): boolean {
  return code === "session_required";
}

/**
 * Whether a failed run was an account-home pursuit miss — the worker tried
 * the header identity chrome, found nothing it could verify, and journaled
 * its attempts. The managed browser is still sitting on the portal page,
 * so the honest recovery is handing the window to the user.
 *
 * The `"account-home:"` prefix is Clinch's own journal contract (see
 * `identity_miss_diagnostic` in the macro-engine), not user text.
 */
export function isPursuitMiss(code: string | null, messageText: string): boolean {
  return code === "workflow_failed" && messageText.includes("account-home:");
}
