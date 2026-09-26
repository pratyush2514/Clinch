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

const DORMANT: ContextStatus = { attached: false, headless: true, windowMode: "headless" };

type Screencast = {
  status: ContextStatus;
  /** Latest JPEG frame as base64, or `null` before the first one arrives. */
  frame: string | null;
  /**
   * The agent's synthetic pointer position, or `null` when it has nothing to
   * show. Follows the same session latch as frames: positions from a
   * previous session, or arriving before this session's first frame, are
   * dropped at the handler.
   */
  cursor: AgentCursor | null;
  busy: boolean;
  /** Attach the background context if it is not already live. Throws on failure. */
  ensure: () => Promise<void>;
  takeControl: (url?: string) => Promise<void>;
  release: () => Promise<void>;
};

export function useScreencast(ready: boolean, report: (text: string) => void): Screencast {
  const [status, setStatus] = useState<ContextStatus>(DORMANT);
  const [frame, setFrame] = useState<string | null>(null);
  const [cursor, setCursor] = useState<AgentCursor | null>(null);
  const [busy, setBusy] = useState(false);
  // Mirrors `status.attached` so `ensure` can read it without being rebuilt on
  // every status change (and without re-arming the thread's callbacks).
  const attached = useRef(false);
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
      setCursor(event.payload);
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
      report("Background browser released — no Chrome process remains.");
    } catch (error) {
      report(message(error));
    } finally {
      setBusy(false);
    }
  }, [report]);

  return { status, frame, cursor, busy, ensure, takeControl, release };
}
