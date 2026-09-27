/**
 * The background browser, as the thread sees it.
 *
 * Nothing attaches Chromium on startup or on a status read — `ensure` is the
 * only path here that launches, and the thread calls it when a task is
 * submitted, which is what makes a run zero-friction. Frames arrive on the
 * single global Tauri event and stop when the context is released.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { SCREENCAST_EVENT, CURSOR_EVENT, type AgentCursor, type ContextStatus, type ScreencastFrame } from "../lib/ipc";
import { message } from "../lib/errors";

const DORMANT: ContextStatus = { attached: false, headless: true, windowMode: "offscreen" };

/**
 * Frame-integrity gate for the live stream.
 *
 * A torn JPEG does not fail image decode: the decoder paints whatever scans
 * survive, which lands on the panel as static / colored streaks instead of
 * the page. CDP marks such frames as ordinary — the screencast encoder can
 * emit one when the compositor is torn down mid-paint (a navigation
 * committing, a browser restart) — so the check happens here, the last
 * layer that sees every frame before render: the base64 payload must decode
 * to bytes starting with the JPEG SOI marker (FF D8) and ending with EOI
 * (FF D9). Only the head and tail quanta are decoded, never the whole
 * multi-hundred-kilobyte payload. Handlers drop invalid frames; the panel
 * keeps the last good frame and never blanks on a bad one.
 */
export function isIntactJpegFrame(data: string): boolean {
  // A real frame is far larger; anything this short cannot hold SOI+EOI.
  // CDP base64 is padded, so a length that is not a whole number of quanta
  // is itself a corruption signal.
  if (data.length < 8 || data.length % 4 !== 0) return false;
  let head: string;
  let tail: string;
  try {
    head = atob(data.slice(0, 4));
    tail = atob(data.slice(-8));
  } catch {
    return false; // not base64 at all
  }
  const end = tail.length;
  return (
    head.charCodeAt(0) === 0xff &&
    head.charCodeAt(1) === 0xd8 &&
    end >= 2 &&
    tail.charCodeAt(end - 2) === 0xff &&
    tail.charCodeAt(end - 1) === 0xd9
  );
}

/**
 * Inter-arrival gap (ms) at or below which consecutive cursor samples count
 * as one continuous travel stream. The backend dispatches travel waypoints
 * every ~18ms; 50ms leaves headroom for IPC jitter while genuinely isolated
 * placements — first appearance, post-pause resumes — keep the CSS glide.
 */
export const CURSOR_STREAM_GAP_MS = 50;

/**
 * Whether a cursor sample arriving `gapMs` after the previous one belongs
 * to a travel stream (render raw, no transition) rather than an isolated
 * placement (glide). Pure so the pacing rule is unit-testable.
 */
export function isCursorStream(gapMs: number): boolean {
  return gapMs <= CURSOR_STREAM_GAP_MS;
}

/**
 * A cursor sample plus its delivery context. `streaming` is local-only —
 * derived here from inter-arrival timing, never crossing the IPC boundary —
 * and tells the overlay whether this sample is one waypoint of a rapid
 * travel stream (position it raw) or an isolated placement (glide to it).
 */
export type CursorSample = AgentCursor & { streaming: boolean };

type Screencast = {
  status: ContextStatus;
  /** Latest JPEG frame as base64, or `null` before the first one arrives. */
  frame: string | null;
  /**
   * The agent's synthetic pointer position, or `null` when it has nothing to
   * show. Follows the same session latch as frames: positions from a
   * previous session, or arriving before this session's first frame, are
   * dropped at the handler. Each sample carries the hook's local-only
   * `streaming` flag so the overlay knows whether to track it raw or glide.
   */
  cursor: CursorSample | null;
  busy: boolean;
  /** Attach the background context if it is not already live. Throws on failure. */
  ensure: () => Promise<void>;
  takeControl: (url?: string) => Promise<void>;
  release: () => Promise<void>;
};

export function useScreencast(ready: boolean, report: (text: string) => void): Screencast {
  const [status, setStatus] = useState<ContextStatus>(DORMANT);
  const [frame, setFrame] = useState<string | null>(null);
  const [cursor, setCursor] = useState<CursorSample | null>(null);
  const [busy, setBusy] = useState(false);
  // Mirrors `status.attached` so `ensure` can read it without being rebuilt on
  // every status change (and without re-arming the thread's callbacks).
  const attached = useRef(false);
  /**
   * Arrival time of the previous cursor sample (`performance.now()`). The
   * gap to the next sample decides the pacing flag: ~18ms waypoint streams
   * render raw, isolated placements glide. Reset wherever the cursor is
   * cleared so a stale timestamp can never mark a fresh placement as a
   * stream.
   */
  const lastCursorAt = useRef(0);
  /**
   * Which browser session the stream belongs to: latched from the first
   * frame that arrives while attached. A frame already in flight when the
   * context is released (or one held over from a previous session) must not
   * repaint the stream — each one is dropped at the handler.
   */
  const sessionId = useRef<number | null>(null);
  /**
   * The frame pump: coalesces the CDP screencast to one state update per
   * animation frame (the pump can emit faster than the browser paints, and
   * every setState is a render pass over the whole thread). Lives in a ref
   * so `release` can drain a frame that raced it.
   */
  const pendingFrame = useRef<{ queued: string | null; rafId: number | null }>({
    queued: null,
    rafId: null,
  });

  useEffect(() => {
    attached.current = status.attached;
  }, [status.attached]);

  useEffect(() => {
    if (!ready) return;
    let live = true;
    invoke<ContextStatus>("browser_context_status")
      .then(next => {
        if (live) setStatus(next);
      })
      .catch(() => {
        if (live) setStatus(DORMANT);
      });
    return () => {
      live = false;
    };
  }, [ready]);

  useEffect(() => {
    let live = true;
    let unlisten: UnlistenFn | undefined;
    const flush = () => {
      pendingFrame.current.rafId = null;
      const frame = pendingFrame.current.queued;
      pendingFrame.current.queued = null;
      // Re-check attach: a release may have raced the scheduled paint, and
      // the release path clears the stream itself.
      if (frame !== null && attached.current) setFrame(frame);
    };
    listen<ScreencastFrame>(SCREENCAST_EVENT, event => {
      // Post-release stragglers arrive with the context gone: the release
      // path already cleared the stream, so dropping here keeps the panel
      // from flashing a previous session's last frame.
      if (!attached.current) return;
      if (sessionId.current === null) sessionId.current = event.payload.session_id;
      if (event.payload.session_id !== sessionId.current) return;
      // Integrity gate: a torn JPEG paints static instead of failing
      // decode, so it must never reach the img. Dropped here, the panel
      // simply keeps the last good frame — no render, no blank.
      if (!isIntactJpegFrame(event.payload.data)) return;
      pendingFrame.current.queued = event.payload.data;
      if (pendingFrame.current.rafId === null) {
        pendingFrame.current.rafId = requestAnimationFrame(flush);
      }
    })
      .then(stop => {
        if (live) unlisten = stop;
        else stop();
      })
      .catch(() => {
        /* Without the event bus the thread still runs; it just shows no frames. */
      });
    return () => {
      live = false;
      unlisten?.();
      const id = pendingFrame.current.rafId;
      pendingFrame.current.rafId = null;
      if (id !== null) cancelAnimationFrame(id);
    };
  }, []);

  // The agent's pointer: one lightweight event per synthetic input dispatch
  // (move/press/release), not tied to frame paints. Gated on the same
  // session latch as frames — a cursor without its stream must never render.
  useEffect(() => {
    let live = true;
    let unlisten: UnlistenFn | undefined;
    listen<AgentCursor>(CURSOR_EVENT, event => {
      if (!attached.current) return;
      // The frame stream latches the session first: a cursor arriving before
      // any frame of this session, or carried over from a previous one
      // (including the backend's -1 sentinel), is dropped here.
      if (sessionId.current === null) return;
      if (event.payload.session_id !== sessionId.current) return;
      // Pacing: waypoint streams (~18ms apart) render raw; an isolated
      // placement keeps the short CSS glide. Timed at arrival — the only
      // point that sees the true inter-event gap.
      const now = performance.now();
      const gap = now - lastCursorAt.current;
      lastCursorAt.current = now;
      setCursor({ ...event.payload, streaming: isCursorStream(gap) });
    })
      .then(stop => {
        if (live) unlisten = stop;
        else stop();
      })
      .catch(() => {
        /* Without the event bus the thread still runs; it just shows no cursor. */
      });
    return () => {
      live = false;
      unlisten?.();
    };
  }, []);

  const ensure = useCallback(async () => {
    if (attached.current) return;
    const next = await invoke<ContextStatus>("acquire_browser_context");
    attached.current = next.attached;
    // A fresh attach starts a fresh session: the latch must not carry the
    // previous session's id into the new stream, and any lingering cursor
    // belongs to that old session.
    sessionId.current = null;
    setCursor(null);
    lastCursorAt.current = 0;
    setStatus(next);
  }, []);

  const takeControl = useCallback(
    async (url?: string) => {
      setBusy(true);
      try {
        setStatus(await invoke<ContextStatus>("take_control", { url: url ?? null }));
        report(
          url
            ? "Managed Chromium is now headful on the challenged page — solve the check in its window directly."
            : "Managed Chromium is now headful — interact with its window directly.",
        );
      } catch (error) {
        report(message(error));
      } finally {
        setBusy(false);
      }
    },
    [report],
  );

  const release = useCallback(async () => {
    setBusy(true);
    try {
      await invoke("release_browser_context");
      attached.current = false;
      setStatus(DORMANT);
      // Drain the frame pump before clearing the stream: a release may have
      // raced a scheduled paint, and the flushed frame must not resurrect
      // the previous session's last frame.
      const pending = pendingFrame.current.rafId;
      pendingFrame.current.rafId = null;
      pendingFrame.current.queued = null;
      if (pending !== null) cancelAnimationFrame(pending);
      sessionId.current = null;
      setFrame(null);
      setCursor(null);
      lastCursorAt.current = 0;
      report("Background browser released — no Chrome process remains.");
    } catch (error) {
      report(message(error));
    } finally {
      setBusy(false);
    }
  }, [report]);

  return { status, frame, cursor, busy, ensure, takeControl, release };
}
