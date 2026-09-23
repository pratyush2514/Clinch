import { useState } from "react";
import { Command } from "cmdk";
import type { PlaybookSummary, SiteShortcut } from "../lib/ipc";

/** Workflow and playbook names are ASCII identifiers, as the schema requires. */
const NAME_PATTERN = /^[A-Za-z0-9_-]+$/;

/**
 * Everything that is not a task: portal sessions, saved replays, and browser
 * control.
 *
 * These all used to be permanent panels. They are one-off actions, so they live
 * behind ⌘K where they cost no layout, and the thread keeps the window.
 *
 * Typing an identifier offers to replay a recorded macro by that name, which is
 * how the legacy macro lane stays reachable without a form.
 */
export default function CommandPalette({
  open,
  onOpenChange,
  ready,
  portal,
  attached,
  headless,
  playbooks,
  shortcuts,
  onSaveShortcut,
  onDeleteShortcut,
  onConnect,
  onReauthenticate,
  onReplayPlaybook,
  onReplayMacro,
  onTakeControl,
  onRelease,
  onCloseBrowser,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  ready: boolean;
  portal: string;
  attached: boolean;
  headless: boolean;
  playbooks: PlaybookSummary[];
  shortcuts: SiteShortcut[];
  onSaveShortcut: (name: string, url: string) => Promise<void>;
  onDeleteShortcut: (name: string) => Promise<void>;
  onConnect: () => void;
  onReauthenticate: () => void;
  onReplayPlaybook: (playbook: PlaybookSummary) => void;
  onReplayMacro: (workflow: string) => void;
  onTakeControl: (url?: string) => void;
  onRelease: () => void;
  onCloseBrowser: () => void;
}) {
  const [search, setSearch] = useState("");
  const [addingShortcut, setAddingShortcut] = useState(false);
  const [shortcutName, setShortcutName] = useState("");
  const [shortcutUrl, setShortcutUrl] = useState("");
  const [shortcutError, setShortcutError] = useState<string | null>(null);
  const [shortcutBusy, setShortcutBusy] = useState(false);

  function choose(run: () => void) {
    onOpenChange(false);
    setSearch("");
    run();
  }

  async function saveShortcut() {
    setShortcutBusy(true);
    setShortcutError(null);
    try {
      await onSaveShortcut(shortcutName, shortcutUrl);
      setShortcutName("");
      setShortcutUrl("");
      setAddingShortcut(false);
    } catch (error) {
      setShortcutError(
        error instanceof Error ? error.message : "Could not save that shortcut.",
      );
    } finally {
      setShortcutBusy(false);
    }
  }

  const macroName = search.trim();
  const canReplayMacro = ready && Boolean(portal) && NAME_PATTERN.test(macroName);

  return (
    <Command.Dialog
      open={open}
      onOpenChange={onOpenChange}
      label="Clinch commands"
      shouldFilter
    >
      <Command.Input
        value={search}
        onValueChange={setSearch}
        placeholder="Find a command, or type a saved macro name…"
      />
      <Command.List>
        <Command.Empty>No matching commands.</Command.Empty>
        <Command.Group heading="Session">
          <Command.Item onSelect={() => choose(onConnect)}>Connect a portal…</Command.Item>
          <Command.Item
            disabled={!ready || !portal}
            onSelect={() => choose(onReauthenticate)}
          >
            Re-authenticate in app
          </Command.Item>
        </Command.Group>
        {playbooks.length > 0 && (
          <Command.Group heading="Replay a saved playbook">
            {playbooks.map(playbook => (
              <Command.Item
                key={playbook.id}
                value={`${playbook.name} ${playbook.portalUrl}`}
                disabled={!ready}
                onSelect={() => choose(() => onReplayPlaybook(playbook))}
              >
                {playbook.name}
                <small>
                  {playbook.stepCount} step{playbook.stepCount === 1 ? "" : "s"} ·{" "}
                  {playbook.portalUrl}
                </small>
              </Command.Item>
            ))}
          </Command.Group>
        )}
        {canReplayMacro && (
          <Command.Group heading="Recorded macro">
            <Command.Item
              // Forced to match so the typed name always surfaces this item.
              value={search}
              onSelect={() => choose(() => onReplayMacro(macroName))}
            >
              Replay recorded macro “{macroName}”
            </Command.Item>
          </Command.Group>
        )}
        <Command.Group heading="Managed browser">
          <Command.Item
            disabled={!attached || !headless}
            onSelect={() => choose(onTakeControl)}
          >
            Take control (headful window)
          </Command.Item>
          <Command.Item disabled={!attached} onSelect={() => choose(onRelease)}>
            Release background browser
          </Command.Item>
          <Command.Item disabled={!ready} onSelect={() => choose(onCloseBrowser)}>
            Close managed browser &amp; forget session
          </Command.Item>
        </Command.Group>
        <Command.Group heading="Site shortcuts">
          <div className="shortcut-hint">
            “Open amazon” resolves here first — no search, no guessing.
          </div>
          {shortcuts.map(shortcut => (
            <div key={shortcut.name} className="shortcut-row">
              <span className="shortcut-name">{shortcut.name}</span>
              <small className="shortcut-url">{shortcut.url}</small>
              <button
                type="button"
                className="shortcut-remove"
                disabled={shortcutBusy}
                onClick={() => {
                  setShortcutBusy(true);
                  onDeleteShortcut(shortcut.name).finally(() =>
                    setShortcutBusy(false),
                  );
                }}
              >
                Remove
              </button>
            </div>
          ))}
          {addingShortcut ? (
            <div className="shortcut-form">
              <input
                value={shortcutName}
                onChange={event => setShortcutName(event.target.value)}
                onKeyDown={event => event.stopPropagation()}
                placeholder="Name — e.g. amazon"
                aria-label="Shortcut name"
              />
              <input
                value={shortcutUrl}
                onChange={event => setShortcutUrl(event.target.value)}
                onKeyDown={event => event.stopPropagation()}
                placeholder="https://…"
                aria-label="Shortcut URL"
              />
              <div className="shortcut-form-actions">
                <button
                  type="button"
                  disabled={shortcutBusy || !shortcutName.trim() || !shortcutUrl.trim()}
                  onClick={() => void saveShortcut()}
                >
                  {shortcutBusy ? "Saving…" : "Save shortcut"}
                </button>
                <button
                  type="button"
                  disabled={shortcutBusy}
                  onClick={() => {
                    setAddingShortcut(false);
                    setShortcutError(null);
                  }}
                >
                  Cancel
                </button>
              </div>
              {shortcutError && (
                <div className="shortcut-error">{shortcutError}</div>
              )}
            </div>
          ) : (
            <Command.Item
              onSelect={() => {
                setAddingShortcut(true);
                setShortcutError(null);
              }}
            >
              Add site shortcut…
            </Command.Item>
          )}
        </Command.Group>
      </Command.List>
    </Command.Dialog>
  );
}
