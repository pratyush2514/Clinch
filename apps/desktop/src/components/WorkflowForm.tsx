import { useEffect, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";

const ROLES = ["button", "link", "textbox", "combobox", "menuitem"] as const;

type IntentPreview = {
  role: string;
  name: string;
  description: string;
  backendNodeId: number;
  score: number;
};

type DraftStep = { role: string; label: string };

type SavedPlaybook = {
  id: string;
  name: string;
  portalUrl: string;
  stepCount: number;
  updatedAt: string;
};

type PlaybookApproval = {
  runId: number;
  stepIndex: number;
  kind: string;
  summary: string;
};

type PlaybookEvent = {
  runId: number;
  stepIndex: number;
  totalSteps: number;
  phase: string;
  highlight: unknown;
  approval: PlaybookApproval | null;
};

type SequenceOutcome = {
  completedSteps: number;
  totalSteps: number;
  status: string;
  stoppedAt: number | null;
};

type RunView = {
  id: string;
  name: string;
  phases: Record<number, string>;
  approval: PlaybookApproval | null;
  outcome: SequenceOutcome | null;
  running: boolean;
};

export default function WorkflowForm({ ready, busy, portal, report, errorMessage }: {
  ready: boolean; busy: boolean; portal: string;
  report: (text: string) => void; errorMessage: (error: unknown) => string;
}) {
  const [role, setRole] = useState<string>(ROLES[0]);
  const [label, setLabel] = useState("");
  const [preview, setPreview] = useState<IntentPreview | null>(null);
  const [checking, setChecking] = useState(false);
  const [steps, setSteps] = useState<DraftStep[]>([]);
  const [workflowName, setWorkflowName] = useState("workflow");
  const [saved, setSaved] = useState<SavedPlaybook[]>([]);
  const [run, setRun] = useState<RunView | null>(null);

  async function refreshList() {
    try {
      setSaved(await invoke<SavedPlaybook[]>("list_playbooks"));
    } catch (error) { report(errorMessage(error)); }
  }

  useEffect(() => {
    if (ready) void refreshList();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ready]);

  async function checkMatch() {
    if (!label.trim()) { report("Describe the control first — role alone never matches."); return; }
    setChecking(true);
    setPreview(null);
    try {
      const match = await invoke<IntentPreview>("preview_intent", { role, label: label.trim() });
      setPreview(match);
      report(`Intent matches ${match.role} “${match.name || "(unnamed)"}” at strength ${match.score}/3.`);
    } catch (error) { report(errorMessage(error)); }
    finally { setChecking(false); }
  }

  function addStep() {
    if (!label.trim()) { report("Describe the control first — role alone never matches."); return; }
    setSteps(current => [...current, { role, label: label.trim() }]);
    setLabel("");
    setPreview(null);
  }

  async function saveWorkflow() {
    if (steps.length === 0) { report("Add at least one step before saving."); return; }
    if (!portal) { report("Enter the portal URL on the left first — it anchors the workflow."); return; }
    try {
      const id = await invoke<string>("save_playbook", {
        name: workflowName,
        portalUrl: portal,
        steps: steps.map(step => ({ kind: "semantic", intent: { role: step.role, labelQuery: step.label } })),
      });
      report(`Saved workflow ${workflowName} (id ${id}).`);
      setSteps([]);
      await refreshList();
    } catch (error) { report(errorMessage(error)); }
  }

  async function runWorkflow(playbook: SavedPlaybook) {
    setRun({ id: playbook.id, name: playbook.name, phases: {}, approval: null, outcome: null, running: true });
    const progress = new Channel<PlaybookEvent>();
    progress.onmessage = event => {
      setRun(current => current && current.id === playbook.id
        ? {
          ...current,
          phases: { ...current.phases, [event.stepIndex]: event.phase },
          approval: event.approval,
        }
        : current);
    };
    try {
      const outcome = await invoke<SequenceOutcome>("execute_playbook", { id: playbook.id, progress });
      setRun(current => current && current.id === playbook.id
        ? { ...current, outcome, running: false, approval: null }
        : current);
      report(`Playbook ${playbook.name}: ${outcome.status.replaceAll("_", " ")} · ${outcome.completedSteps}/${outcome.totalSteps} steps.`);
    } catch (error) {
      report(errorMessage(error));
      setRun(current => current && current.id === playbook.id ? { ...current, running: false } : current);
    }
  }

  async function decide(approved: boolean) {
    if (!run?.approval) return;
    try {
      await invoke("decide_playbook", { runId: run.approval.runId, index: run.approval.stepIndex, approved });
    } catch (error) { report(errorMessage(error)); }
  }

  const fragment = JSON.stringify({
    version: 1,
    steps: steps.map(step => ({ kind: "semantic", intent: { role: step.role, labelQuery: step.label } })),
  }, null, 2);

  const runBusy = busy || (run?.running ?? false);

  return <section className="workflow-form" aria-label="Workflow builder">
    <div className="eyebrow">PHASE B / WORKFLOW BUILDER</div>
    <h3>Describe controls, not selectors.</h3>
    <p>Connect a portal first, then preview each intent against the live page. Save named workflows below and replay them with one click; clicks pause for approval like every other run.</p>
    <form onSubmit={event => { event.preventDefault(); void checkMatch(); }}>
      <fieldset disabled={!ready || busy || checking}>
        <div className="fields">
          <label>Role<select value={role} onChange={event => { setRole(event.target.value); setPreview(null); }}>
            {ROLES.map(option => <option key={option} value={option}>{option}</option>)}
          </select></label>
          <label>Label as shown on the page<input placeholder="Pay now" value={label} onChange={event => { setLabel(event.target.value); setPreview(null); }} /></label>
        </div>
        <div className="actions">
          <button type="submit">{checking ? "Checking…" : "Preview match"}</button>
          <button type="button" disabled={!label.trim()} onClick={addStep}>Add step</button>
        </div>
      </fieldset>
    </form>
    {preview && <p className="target">Match: <code>{preview.role}</code> · <code>{preview.name || "(unnamed)"}</code> · strength {preview.score}/3 · node {preview.backendNodeId}</p>}
    {steps.length > 0 && <div className="run-result" aria-live="polite">
      <div className="run-heading"><strong>{steps.length} step{steps.length === 1 ? "" : "s"}</strong></div>
      <ol>{steps.map((step, index) => <li key={index}>
        <span>{String(index + 1).padStart(2, "0")}</span>
        <div><strong>{step.role} · {step.label}</strong></div>
        <div className="actions"><button onClick={() => setSteps(current => current.filter((_, drop) => drop !== index))}>Remove</button></div>
      </li>)}</ol>
      <pre>{fragment}</pre>
      <div className="fields"><label>Workflow name<input required pattern="[A-Za-z0-9_-]+" maxLength={64} value={workflowName} onChange={event => setWorkflowName(event.target.value)} /></label></div>
      <div className="actions"><button className="primary" disabled={!ready || busy || !portal} onClick={() => void saveWorkflow()}>Save Workflow</button></div>
    </div>}
    {saved.length > 0 && <div className="run-result" aria-live="polite">
      <div className="run-heading"><strong>Saved workflows</strong><span>{saved.length} stored</span></div>
      <ol>{saved.map(playbook => <li key={playbook.id}>
        <span>{playbook.id.padStart(2, "0")}</span>
        <div><strong>{playbook.name}</strong><small>{playbook.stepCount} step{playbook.stepCount === 1 ? "" : "s"} · {playbook.portalUrl}</small></div>
        <div className="actions"><button className="primary" disabled={!ready || runBusy || !portal} onClick={() => void runWorkflow(playbook)}>{run?.running && run.id === playbook.id ? "Running…" : "Run"}</button></div>
      </li>)}</ol>
    </div>}
    {run && <div className="run-result" aria-live="polite">
      <div className="run-heading"><strong>Run · {run.name}</strong><span>{run.running ? "running" : "finished"}</span></div>
      <ol>{Object.entries(run.phases).map(([index, phase]) => <li key={index}>
        <span>{String(Number(index) + 1).padStart(2, "0")}</span>
        <div><strong>Step {Number(index) + 1}</strong><small>{String(phase).replaceAll("_", " ")}</small></div>
      </li>)}</ol>
      {run.approval && <div className="gate-card" role="dialog" aria-label="Playbook approval">
        <div className="eyebrow">SENTINEL GATE</div>
        <p>Step {run.approval.stepIndex + 1} · {run.approval.kind} · <code>{run.approval.summary}</code>. Execution is paused until you decide.</p>
        <div className="actions">
          <button onClick={() => void decide(false)}>Reject</button>
          <button className="primary" onClick={() => void decide(true)}>Approve &amp; Submit</button>
        </div>
      </div>}
      {run.outcome && <p className="status">Outcome: {run.outcome.status.replaceAll("_", " ")} · {run.outcome.completedSteps}/{run.outcome.totalSteps} steps{run.outcome.stoppedAt != null ? ` · stopped at step ${run.outcome.stoppedAt + 1}` : ""}.</p>}
    </div>}
  </section>;
}
