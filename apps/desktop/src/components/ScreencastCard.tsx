import { useEffect, useRef } from "react";

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
 */
export default function ScreencastCard({
  frame,
  live,
  headless,
  busy,
  onTakeControl,
  onRelease,
}: {
  frame: string | null;
  live: boolean;
  headless: boolean;
  busy: boolean;
  onTakeControl: () => void;
  onRelease: () => void;
}) {
  const preview = useRef<HTMLDialogElement>(null);

  // The enlarged view must not outlive the frame it was opened on: a released
  // browser clears the stream, and a stale modal would claim to be live.
  useEffect(() => {
    if (!frame) preview.current?.close();
  }, [frame]);

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
      <dialog ref={preview} className="cast-preview" aria-label="Full viewport preview">
        {frame && (
          <img src={`data:image/jpeg;base64,${frame}`} alt="Managed Chromium viewport, enlarged" />
        )}
        <div className="actions">
          <button type="button" onClick={() => preview.current?.close()}>
            Close preview
          </button>
        </div>
      </dialog>
    </div>
  );
}
