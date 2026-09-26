import { useEffect, useRef, useState, type MouseEvent as ReactMouseEvent } from "react";
import type { AgentCursor, ContextStatus } from "../lib/ipc";

/**
 * Human wording for the session badge. Pure so the headed/off-screen/
 * headless distinction is unit-testable: an off-screen headed session
 * must never be labeled "headless" — it owns real windows, just none the
 * user can see.
 */
export function sessionBadgeLabel(
  live: boolean,
  windowMode: ContextStatus["windowMode"],
  finalFrameCaptured: boolean | null,
): string {
  if (!live) {
    return finalFrameCaptured === false ? "last live frame" : "final frame";
  }
  switch (windowMode) {
    case "offscreen":
      return "live · off-screen headed session";
    case "headed":
      return "live · headful, direct control";
    default:
      return "live · headless background session";
  }
}

/**
 * The `object-fit: contain` content box of a frame img: where the page's
 * pixels actually land inside the element box. `object-position: top` (see
 * styles.css) centers the horizontal axis and pins the top.
 * Pure so the letterbox math is unit-testable.
 */
export function contentRect(
  boxWidth: number,
  boxHeight: number,
  naturalWidth: number,
  naturalHeight: number,
): { x: number; y: number; width: number; height: number } {
  const natW = naturalWidth > 0 ? naturalWidth : 16;
  const natH = naturalHeight > 0 ? naturalHeight : 9;
  const scale = Math.min(boxWidth / natW, boxHeight / natH);
  const width = natW * scale;
  const height = natH * scale;
  return { x: (boxWidth - width) / 2, y: 0, width, height };
}

/**
 * Maps an agent cursor (page CSS pixels) onto the rendered content box.
 * Pure so the scaling math is unit-testable.
 */
export function cursorOffset(
  cursor: Pick<AgentCursor, "x" | "y" | "viewport_width" | "viewport_height">,
  rect: { x: number; y: number; width: number; height: number },
): { left: number; top: number } {
  return {
    left: rect.x + (cursor.x / cursor.viewport_width) * rect.width,
    top: rect.y + (cursor.y / cursor.viewport_height) * rect.height,
  };
}

/**
 * A frame img with the agent's pointer drawn over it: an SVG arrow gliding
 * between positions, plus an expanding ripple on every press — the click
 * made visible. `cursor` is `null` (hidden) whenever the card is not live:
 * settled cards show only their frozen frame, never a pointer over a moment
 * that already passed.
 */
function FrameWithCursor({
  cursor,
  src,
  alt,
}: {
  /**
   * May carry the screencast hook's local-only `streaming` hint: when true
   * the sample is one waypoint of a rapid travel stream and the overlay
   * positions it raw (no CSS transition); otherwise it keeps the glide.
   */
  cursor: (AgentCursor & { streaming?: boolean }) | null;
  src: string;
  alt: string;
}) {
  const imgRef = useRef<HTMLImageElement>(null);
  const [box, setBox] = useState({ w: 0, h: 0, natW: 0, natH: 0 });
  const [ripples, setRipples] = useState<{ id: number; left: number; top: number }[]>([]);
  const rippleId = useRef(0);
  const lastRippled = useRef<AgentCursor | null>(null);

  const measure = () => {
    const el = imgRef.current;
    if (!el) return;
    const next = {
      w: el.clientWidth,
      h: el.clientHeight,
      natW: el.naturalWidth,
      natH: el.naturalHeight,
    };
    setBox(prev =>
      prev.w === next.w && prev.h === next.h && prev.natW === next.natW && prev.natH === next.natH
        ? prev
        : next,
    );
  };

  useEffect(() => {
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(measure);
    const el = imgRef.current;
    if (el) ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const rect = box.w > 0 ? contentRect(box.w, box.h, box.natW, box.natH) : null;
  const pos = cursor && rect ? cursorOffset(cursor, rect) : null;
  // A waypoint stream positions the pointer directly: the CSS glide would
  // perpetually chase the ~18ms samples and read as lag. Isolated
  // placements keep the transition.
  const streaming = cursor?.streaming === true;

  // A press lands one ripple that expands and fades. Guarded on the event
  // identity so a resize re-render never re-ripples a stale press.
  useEffect(() => {
    if (!cursor || cursor.kind !== "press" || !rect) return;
    if (lastRippled.current === cursor) return;
    lastRippled.current = cursor;
    const at = cursorOffset(cursor, rect);
    const id = (rippleId.current += 1);
    setRipples(current => [...current.slice(-4), { id, ...at }]);
    const timer = setTimeout(() => setRipples(current => current.filter(r => r.id !== id)), 650);
    return () => clearTimeout(timer);
  }, [cursor, rect]);

  return (
    <div className="frame-wrap">
      <img ref={imgRef} src={src} alt={alt} onLoad={measure} />
      {pos && (
        <div
          className={streaming ? "agent-cursor cursor-streaming" : "agent-cursor"}
          style={{ left: pos.left, top: pos.top }}
          aria-hidden="true"
        >
          <svg width="20" height="20" viewBox="0 0 24 24" aria-hidden="true">
            <path
              d="M5.5 3.2 19.2 11.4l-7.1 1.7-2.7 7.1z"
              fill="#141414"
              stroke="#ffffff"
              strokeWidth="1.6"
              strokeLinejoin="round"
            />
          </svg>
        </div>
      )}
      {ripples.map(r => (
        <span
          key={r.id}
          className="cursor-ripple"
          style={{ left: r.left, top: r.top }}
          aria-hidden="true"
        />
      ))}
    </div>
  );
}

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
  cursor,
  headless,
  windowMode,
  busy,
  finalUrl,
  pageTitle,
  anchorHost,
  finalFrameCaptured,
  onTakeControl,
  onRelease,
}: {
  frame: string | null;
  live: boolean;
  /**
   * The agent's synthetic pointer. Rendered only while `live` — the card
   * gates on it again internally, so a settled card never shows a cursor
   * over its frozen frame even if the caller forgets to null it.
   *
   * May carry the screencast hook's local-only `streaming` hint (see
   * `FrameWithCursor`): waypoint streams position raw, isolated placements
   * glide.
   */
  cursor: (AgentCursor & { streaming?: boolean }) | null;
  headless: boolean;
  /** Window mode behind `headless`: drives the badge wording only. */
  windowMode: ContextStatus["windowMode"];
  busy: boolean;
  finalUrl: string | null;
  pageTitle: string | null;
  anchorHost: string | null;
  /**
   * Whether the settled card may claim "final frame": explicit `false`
   * means the capture missed and the card shows the last live frame
   * instead. `null` (and legacy `undefined`) keep the old wording.
   */
  finalFrameCaptured: boolean | null;
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
          {sessionBadgeLabel(live, windowMode, finalFrameCaptured)}
        </span>
      </div>
      {frame ? (
        <div className="browser-frame">
          <FrameWithCursor
            cursor={live ? cursor : null}
            src={`data:image/jpeg;base64,${frame}`}
            alt="Managed Chromium viewport"
          />
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
            {live ? "● Live" : finalFrameCaptured === false ? "Last live frame" : "Final frame"}
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
            <FrameWithCursor
              cursor={live ? cursor : null}
              src={`data:image/jpeg;base64,${frame}`}
              alt={
                live
                  ? "Managed Chromium viewport, live"
                  : finalFrameCaptured === false
                    ? "Managed Chromium viewport, last live frame"
                    : "Managed Chromium viewport, final frame"
              }
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
