import { useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";

type CandidatePreview = {
  index: number;
  label: string;
  role: string;
  isLandmark: boolean;
  container: string | null;
};

type PlaybookApproval = {
  runId: number;
  stepIndex: number;
  kind: string;
  summary: string;
  candidates: CandidatePreview[];
};

type PlaybookEvent = {
  runId: number;
  stepIndex: number;
  totalSteps: number;
  phase: string;
  highlight: unknown;
  approval: PlaybookApproval | null;
};

type DispatchOutcome = {
  kind: string;
  name: string;
  result: {
    completedSteps: number;
    totalSteps: number;
    status: string;
    stoppedAt: number | null;
  };
  steps: unknown[];
  // Registry key for one-click save: present only when this ephemeral run
  // completed and the backend remembered its exact graph plus origin.
  runId?: string | null;
  routeLog?: string | null;
  telemetryLog?: string | null;
};

export default function CommandBar({ ready, busy, portal, report, errorMessage }: {
  ready: boolean; busy: boolean; portal: string;
  report: (text: string) => void; errorMessage: (error: unknown) => string;
}) {
  const [prompt, setPrompt] = useState("");
  const [running, setRunning] = useState(false);
  const [phases, setPhases] = useState<Record<number, string>>({});
  const [approval, setApproval] = useState<PlaybookApproval | null>(null);
  const [outcome, setOutcome] = useState<DispatchOutcome | null>(null);
  const [saveName, setSaveName] = useState("");
  const [saving, setSaving] = useState(false);
  const [savedId, setSavedId] = useState<string | null>(null);

  async function dispatch() {
    if (!prompt.trim() || running) return;
    setRunning(true);
    setPhases({});
    setApproval(null);
    setOutcome(null);
    setSaveName("");
    setSavedId(null);
    const progress = new Channel<PlaybookEvent>();
    progress.onmessage = event => {
      setPhases(current => ({ ...current, [event.stepIndex]: event.phase }));
      setApproval(event.approval);
    };
    try {
      const result = await invoke<DispatchOutcome>("dispatch_natural_command", {
        prompt: prompt.trim(),
        progress,
      });
      setOutcome(result);
      setApproval(null);
      // Pre-fill the save box with the backend slug so one click persists
      // the proven path under a valid playbook name.
      if (result.kind === "ephemeral" && result.result.status === "completed") {
        setSaveName(result.name);
      }
      // Route telemetry renders first so Session Activity shows the resolved
      // entry (or miss) before the terminal outcome line.
      if (result.routeLog) report(result.routeLog);
      // Snapshot telemetry follows, one Session Activity row per line, so
      // resync counter, check, action, and node counts are each visible
      // without polling the backend store.
      if (result.telemetryLog) {
        for (const line of result.telemetryLog.split("\n")) {
          if (line.trim()) report(line);
        }
      }
      report(`“${prompt.trim()}” ran ${result.kind} workflow ${result.name}: ${result.result.status.replaceAll("_", " ")} · ${result.result.completedSteps}/${result.result.totalSteps} steps.`);
    } catch (error) {
      report(errorMessage(error));
    } finally {
      setRunning(false);
    }
  }

  async function decide(approved: boolean) {
    if (!approval) return;
    try {
      await invoke("decide_playbook", { runId: approval.runId, index: approval.stepIndex, approved });
    } catch (error) { report(errorMessage(error)); }
  }

  async function saveOutcome() {
    if (!outcome || outcome.kind !== "ephemeral" || !saveName.trim()) return;
    // Only the legacy rebuild needs the portal re-entered: the registry
    // path carries the run's own origin server-side.
    if (!outcome.runId && !portal) {
      report("Enter the portal URL above first — it anchors the workflow.");
      return;
    }
    setSaving(true);
    try {
      // Prefer the backend registry key: the exact executed graph plus its
      // origin, with no portal re-entry and no step round-trip through the
      // client. Stale or evicted keys fail closed server-side. Runs without
      // a key (older backends) fall back to rebuilding from echoed steps.
      const id = outcome.runId
        ? await invoke<string>("save_run_as_workflow", {
            runId: outcome.runId,
            name: saveName.trim(),
            description: null,
          })
        : await invoke<string>("save_playbook", {
            name: saveName.trim(),
            portalUrl: portal,
            steps: outcome.steps,
          });
      setSavedId(id);
      report(`Saved playbook ${saveName.trim()} (id ${id}) — replay it in one click from the workflow list.`);
      // The replay list lives in another component: notify it instead of
      // threading state across the workspace.
      window.dispatchEvent(new CustomEvent("clinch:playbooks-changed"));
    } catch (error) { report(errorMessage(error)); }
    finally { setSaving(false); }
  }

  return <section className="command-bar" aria-label="Natural language commands">
    <form onSubmit={event => { event.preventDefault(); void dispatch(); }}>
      <fieldset disabled={!ready || busy || running}>
        <input
          placeholder='Try “run the monthly site report”…'
          aria-label="Run a saved workflow in plain language"
          value={prompt}
          onChange={event => setPrompt(event.target.value)}
        />
        <button className="primary" type="submit" disabled={!prompt.trim()}>
          {running ? "Running…" : "Run"}
        </button>
      </fieldset>
    </form>
    {Object.keys(phases).length > 0 && <ol>{Object.entries(phases).map(([index, phase]) => <li key={index}>
      <span>{String(Number(index) + 1).padStart(2, "0")}</span>
      <div><strong>Step {Number(index) + 1}</strong><small>{String(phase).replaceAll("_", " ")}</small></div>
    </li>)}</ol>}
    {approval && <div className="gate-card" role="dialog" aria-label="Command approval">
      <div className="eyebrow">SENTINEL GATE</div>
      <p>Step {approval.stepIndex + 1} · {approval.kind} · <code>{approval.summary}</code>. Execution is paused until you decide.</p>
      {approval.candidates.length > 0 && <ol aria-label="Queued controls">
        {approval.candidates.slice(0, 10).map(candidate => <li key={candidate.index}>
          <span>{candidate.index + 1}</span>
          <div>
            <strong>{candidate.label || "(unnamed)"}</strong>
            <small>{candidate.role}{candidate.container ? ` · ${candidate.container}` : ""}</small>
          </div>
          {candidate.isLandmark && <span className="badge">[Navigation Link]</span>}
        </li>)}
      </ol>}
      <div className="actions">
        <button onClick={() => void decide(false)}>Reject</button>
        <button className="primary" onClick={() => void decide(true)}>Approve &amp; Submit</button>
      </div>
    </div>}
    {outcome && <div aria-live="polite">
      <p className="status">Ran {outcome.kind} workflow {outcome.name}: {outcome.result.status.replaceAll("_", " ")} · {outcome.result.completedSteps}/{outcome.result.totalSteps} steps{outcome.result.stoppedAt != null ? ` · stopped at step ${outcome.result.stoppedAt + 1}` : ""}.</p>
      {outcome.kind === "ephemeral" && outcome.result.status === "completed" && (savedId
        ? <p className="target">Saved as a 1-click playbook (id <code>{savedId}</code>) — find it in the workflow list.</p>
        : <div className="actions">
          <input
            aria-label="Playbook name"
            required pattern="[A-Za-z0-9_-]+" maxLength={64}
            placeholder={`${outcome.name}-playbook`}
            value={saveName}
            onChange={event => setSaveName(event.target.value)}
          />
          <button className="primary" disabled={!saveName.trim() || saving} onClick={() => void saveOutcome()}>
            {saving ? "Saving…" : "Save as Playbook"}
          </button>
        </div>)}
    </div>}
  </section>;
}
