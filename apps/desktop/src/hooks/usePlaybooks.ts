/**
 * The saved-playbook list, kept current without polling.
 *
 * Saving happens inside the thread, so this refreshes on the same window event
 * the thread fires rather than on a remount or a timer.
 */

import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { PLAYBOOKS_CHANGED, type PlaybookSummary } from "../lib/ipc";

export function usePlaybooks(ready: boolean): PlaybookSummary[] {
  const [playbooks, setPlaybooks] = useState<PlaybookSummary[]>([]);

  const refresh = useCallback(() => {
    invoke<PlaybookSummary[]>("list_playbooks")
      .then(setPlaybooks)
      .catch(() => {
        /* An unreadable list is not worth interrupting a run for. */
      });
  }, []);

  useEffect(() => {
    if (!ready) return;
    refresh();
    window.addEventListener(PLAYBOOKS_CHANGED, refresh);
    return () => window.removeEventListener(PLAYBOOKS_CHANGED, refresh);
  }, [ready, refresh]);

  return playbooks;
}
