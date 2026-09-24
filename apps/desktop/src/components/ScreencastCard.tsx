import { useEffect, useRef, type MouseEvent as ReactMouseEvent } from "react";

/**
 * The managed browser's viewport, inline in the thread.
 *
 * The live card belongs to whichever entry is running; once an entry settles it
 * keeps the frame it ended on as evidence of what the run saw. Controls appear
 * only on the live card, because releasing the browser from a finished entry
 * would act on a run that is no longer there.
 *
 * "Take Control" is the one affordance that can put a window on screen, so it
 * is offered only while the session is headless.
 *
 * "Open Preview" raises a near-fullscreen overlay dressed as a browser
 * window: tab label from the settle-time document title, a URL pill from the
 * settle-time page URL, and a live/final badge. On a running entry the image
 * keeps streaming, so the overlay is a large live view; on a settled entry it
 * is the frozen final frame, labeled as such.
 */
export default function ScreencastCard({
  frame,
  live,
  headless,
  busy,
  finalUrl,
  pageTitle,
  anchorHost,
  onTakeControl,
  onRelease,
}: {
  frame: string | null;
  live: boolean;
  headless: boolean;
  busy: boolean;
  finalUrl: string | null;
  pageTitle: string | null;
  anchorHost: string | null;
  onTakeControl: () => void;
  onRelease: () => void;
}) {
  const preview = useRef<HTMLDialogElement>(null);

  // The enlarged view must not outlive the frame it was opened on: a released
  // browser clears the stream, and a stale modal would claim to be live.
  useEffect(() => {
    if (!frame) preview.current?.close();
  }, [frame]);

  const secure = finalUrl?.startsWith("https://") ?? false;
  const tabLabel = pageTitle?.trim() || hostOf(finalUrl) || anchorHost || "Managed Chromium";
  const urlLabel = finalUrl || (anchorHost ? `https://${anchorHost}` : null);

  const closeOnBackdrop = (event: ReactMouseEvent<HTMLDialogElement>) => {
    if (event.target === preview.current) preview.current?.close();
  };

  return (
    <div className="cast" aria-label="Managed browser viewport">
      <div className="cast-head">
        <span className="eyebrow">MANAGED CHROMIUM</span>
        <span className="cast-state">
          {live
            ? headless
              ? "live · headless background session"
              : "live · headful, direct control"
            : "final frame"}
        </span>
      </div>
      {frame ? (
        <div className="browser-frame">
          <img src={`data:image/jpeg;base64,${frame}`} alt="Managed Chromium viewport" />
        </div>
      ) : (
        <p className="cast-empty">
          Waiting for the first frame from the app-owned Chromium profile.
        </p>
      )}
      <div className="actions">
        {/* Enlarging works on a finished turn too: the frozen frame is the
            record of what that run saw, and it is worth being able to read. */}
        <button type="button" disabled={!frame} onClick={() => preview.current?.showModal()}>
          Open Preview
        </button>
        {live && headless && (
          <button type="button" disabled={busy} onClick={onTakeControl}>
            Take Control
          </button>
        )}
        {live && (
          <button type="button" disabled={busy} onClick={onRelease}>
            Release Browser
          </button>
        )}
      </div>
      <dialog
        ref={preview}
        className="cast-preview"
        aria-label="Full viewport preview"
        onClick={closeOnBackdrop}
      >
        <div className="preview-chrome">
          <span className="preview-traffic" aria-hidden="true">
            <i />
            <i />
            <i />
          </span>
          <span className="preview-tab" title={tabLabel}>
            {tabLabel}
          </span>
          {urlLabel && (
            <span className="preview-url" title={urlLabel}>
              <span aria-hidden="true">{secure ? "🔒" : "🌐"}</span>
              {urlLabel}
            </span>
          )}
          <span className={`preview-badge${live ? " is-live" : ""}`}>
            {live ? "● Live" : "Final frame"}
          </span>
          <button
            type="button"
            className="preview-close"
            aria-label="Close preview"
            onClick={() => preview.current?.close()}
          >
            ×
          </button>
        </div>
        <div className="preview-viewport">
          {frame && (
            <img
              src={`data:image/jpeg;base64,${frame}`}
              alt={live ? "Managed Chromium viewport, live" : "Managed Chromium viewport, final frame"}
            />
          )}
        </div>
      </dialog>
    </div>
  );
}

/** Host of a URL string, or null when it is absent or unparseable. */
function hostOf(url: string | null): string | null {
  if (!url) return null;
  try {
    return new URL(url).host || null;
  } catch {
    return null;
  }
}
