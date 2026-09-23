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
import { SCREENCAST_EVENT, type ContextStatus, type ScreencastFrame } from "../lib/ipc";
import { message } from "../lib/errors";

const DORMANT: ContextStatus = { attached: false, headless: true };

export type Screencast = {
  status: ContextStatus;
  /** Latest JPEG frame as base64, or `null` before the first one arrives. */
  frame: string | null;
  busy: boolean;
  /** Attach the background context if it is not already live. Throws on failure. */
  ensure: () => Promise<void>;
  takeControl: () => Promise<void>;
  release: () => Promise<void>;
};

export function useScreencast(ready: boolean, report: (text: string) => void): Screencast {
  const [status, setStatus] = useState<ContextStatus>(DORMANT);
  const [frame, setFrame] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // Mirrors `status.attached` so `ensure` can read it without being rebuilt on
  // every status change (and without re-arming the thread's callbacks).
  const attached = useRef(false);

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
    listen<ScreencastFrame>(SCREENCAST_EVENT, event => setFrame(event.payload.data))
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
    };
  }, []);

  const ensure = useCallback(async () => {
    if (attached.current) return;
    const next = await invoke<ContextStatus>("acquire_browser_context");
    attached.current = next.attached;
    setStatus(next);
  }, []);

  const takeControl = useCallback(async () => {
    setBusy(true);
    try {
      setStatus(await invoke<ContextStatus>("take_control"));
      report("Managed Chromium is now headful — interact with its window directly.");
    } catch (error) {
      report(message(error));
    } finally {
      setBusy(false);
    }
  }, [report]);

  const release = useCallback(async () => {
    setBusy(true);
    try {
      await invoke("release_browser_context");
      attached.current = false;
      setStatus(DORMANT);
      setFrame(null);
      report("Background browser released — no Chrome process remains.");
    } catch (error) {
      report(message(error));
    } finally {
      setBusy(false);
    }
  }, [report]);

  return { status, frame, busy, ensure, takeControl, release };
}
