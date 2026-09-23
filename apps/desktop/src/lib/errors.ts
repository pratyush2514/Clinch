/**
 * Backend errors arrive as `AppError`, an adjacently-tagged enum serialized as
 * `{ code, message? }`. The code is the stable contract; the copy below is the
 * one place that turns each code into something a person can act on.
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
      return "Could not open Chromium. Check CLINCH_CHROMIUM_PATH, then close and retry.";
    if (error.code === "busy") return "An operation is already in progress.";
    if (error.code === "storage_unavailable")
      return "Local storage is unavailable. Check app data permissions.";
    if (error.code === "session_required")
      return "Connect this portal and finish signing in in the managed browser before running tasks.";
    if (error.code === "workflow_failed")
      return "The workflow could not finish. Check its saved macro and the latest task checkpoint; no automatic retry was attempted.";
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
