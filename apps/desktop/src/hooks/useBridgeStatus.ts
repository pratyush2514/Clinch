import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { BridgeStatus } from "../lib/ipc";

/**
 * Live view of the Companion bridge, polled while a session-lending card is
 * mounted. The Sync button stays in an honest "Waiting for companion…"
 * state until at least one companion is attached — the tap can no longer
 * fail with "not connected" when the browser was merely idle (MV3
 * suspension) or still starting.
 *
 * Cheap: one local IPC round-trip per interval, no browser I/O.
 */
export function useBridgeStatus(active: boolean, intervalMs = 2000): BridgeStatus | null {
  const [status, setStatus] = useState<BridgeStatus | null>(null);
  useEffect(() => {
    if (!active) return;
    let cancelled = false;
    const poll = () => {
      invoke<BridgeStatus>("bridge_status")
        .then((s) => {
          if (!cancelled) setStatus(s);
        })
        .catch(() => {
          // Backend unreachable mid-poll: keep the last known state rather
          // than flashing the button.
        });
    };
    poll();
    const timer = setInterval(poll, intervalMs);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [active, intervalMs]);
  return status;
}
