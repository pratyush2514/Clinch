/**
 * The user's saved site shortcuts, kept current without polling.
 *
 * Saves and deletes happen through the palette, so the list refreshes on
 * demand after each mutation rather than on a timer.
 */

import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { SiteShortcut } from "../lib/ipc";

export function useSiteShortcuts(ready: boolean): {
  shortcuts: SiteShortcut[];
  saveShortcut: (name: string, url: string) => Promise<SiteShortcut>;
  deleteShortcut: (name: string) => Promise<boolean>;
} {
  const [shortcuts, setShortcuts] = useState<SiteShortcut[]>([]);

  const refresh = useCallback(() => {
    invoke<SiteShortcut[]>("list_site_shortcuts")
      .then(setShortcuts)
      .catch(() => {
        /* An unreadable list is not worth interrupting a run for. */
      });
  }, []);

  useEffect(() => {
    if (!ready) return;
    refresh();
  }, [ready, refresh]);

  const saveShortcut = useCallback(
    async (name: string, url: string): Promise<SiteShortcut> => {
      const saved = await invoke<SiteShortcut>("save_site_shortcut", { name, url });
      refresh();
      return saved;
    },
    [refresh],
  );

  const deleteShortcut = useCallback(
    async (name: string): Promise<boolean> => {
      const deleted = await invoke<boolean>("delete_site_shortcut", { name });
      refresh();
      return deleted;
    },
    [refresh],
  );

  return { shortcuts, saveShortcut, deleteShortcut };
}
