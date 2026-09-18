import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";

export type PickedElement = {
  tag: string;
  selectors: string[];
  rect: { x: number; y: number; width: number; height: number };
  text: string;
};

export default function ElementPicker({ disabled, onPick, report }: {
  disabled: boolean;
  onPick: (selector: string, picked: PickedElement) => void;
  report: (text: string) => void;
}) {
  const [picking, setPicking] = useState(false);
  const [picked, setPicked] = useState<PickedElement | null>(null);

  async function pick() {
    setPicking(true);
    setPicked(null);
    try {
      await invoke("picker_enable");
      // Bounded wait: the Rust side times out and the overlay is torn down
      // in `picker_pick` even when nothing is clicked.
      const element = await invoke<PickedElement>("picker_pick", { timeoutMs: 60_000 });
      setPicked(element);
      const primary = element.selectors[0] ?? "";
      if (primary) {
        onPick(primary, element);
        report(`Picked ${element.tag} · ${primary}${element.text ? ` · “${element.text.slice(0, 80)}”` : ""}.`);
      }
    } catch {
      report("Element picking ended without a selection. The overlay was removed.");
      try { await invoke("picker_disable"); } catch { /* overlay already gone */ }
    } finally {
      setPicking(false);
    }
  }

  async function cancel() {
    try { await invoke("picker_disable"); } catch { /* best effort */ }
    setPicking(false);
    report("Element picking cancelled.");
  }

  return (
    <div className="element-picker" aria-live="polite">
      <div className="actions">
        <button type="button" disabled={disabled || picking} onClick={() => void pick()}>
          {picking ? "Picking… click an element" : "Pick Element"}
        </button>
        {picking && <button type="button" onClick={() => void cancel()}>Cancel</button>}
      </div>
      {picking && (
        <p className="notice">Hover highlights elements in the managed Chromium window. Click to capture its selector chain.</p>
      )}
      {picked && !picking && (
        <p className="target">
          Picked <code>{picked.tag}</code> · <code>{picked.selectors[0]}</code>
          {picked.selectors.length > 1 && <span> (+{picked.selectors.length - 1} fallbacks)</span>}
          {picked.text && <span> · “{picked.text.slice(0, 80)}”</span>}
        </p>
      )}
    </div>
  );
}
