import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Highlight } from "./TaskWorkspace";
type Frame = { data: string; width: number; height: number };
export default function BrowserViewport({ ready, highlight }: { ready: boolean; highlight: Highlight | null }) {
  const [frame, setFrame] = useState<Frame | null>(null);
  useEffect(() => {
    if (!ready) { setFrame(null); return; }
    let active = true;
    let timer: ReturnType<typeof setTimeout>;
    async function refresh() {
      try { const next = await invoke<Frame>("browser_viewport"); if (active) setFrame(next); }
      catch { if (active) setFrame(null); }
      finally { if (active) timer = setTimeout(() => void refresh(), 250); }
    }
    void refresh();
    return () => { active = false; clearTimeout(timer); };
  }, [ready]);
  return <div className="browser-viewport" aria-label="Live browser viewport">
    {ready && frame && frame.width > 0 && frame.height > 0 ? <div className="browser-frame" style={{ aspectRatio: `${frame.width} / ${frame.height}` }}><img src={`data:image/jpeg;base64,${frame.data}`} alt="Managed Chromium live viewport" />
      {highlight && <div className="browser-highlight" style={{ left: `${100 * highlight.x / frame.width}%`, top: `${100 * highlight.y / frame.height}%`, width: `${100 * highlight.width / frame.width}%`, height: `${100 * highlight.height / frame.height}%` }} aria-label={`Active target: ${highlight.selector}`} />}
    </div> : <p>Connect the managed browser to see its live viewport.</p>}
  </div>;
}
