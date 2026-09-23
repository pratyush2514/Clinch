import { useState } from "react";
import { Command } from "cmdk";
import type { PlaybookSummary } from "../lib/ipc";

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
  onConnect: () => void;
  onReauthenticate: () => void;
  onReplayPlaybook: (playbook: PlaybookSummary) => void;
  onReplayMacro: (workflow: string) => void;
  onTakeControl: () => void;
  onRelease: () => void;
  onCloseBrowser: () => void;
}) {
  const [search, setSearch] = useState("");

  function choose(run: () => void) {
    onOpenChange(false);
    setSearch("");
    run();
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
      </Command.List>
    </Command.Dialog>
  );
}
