import { useEffect, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";

type TaskState = "planned" | "running" | "needs_repair" | "completed" | "failed" | "interrupted";
type StepState = "pending" | "running" | "completed" | "needs_repair" | "failed" | "interrupted";
type Action = { type: "navigate"; url: string } | { type: "click" | "submit" | "download_links"; selector: string }
  | { type: "fill"; selector: string; value: string };
type Wait = { selector: string; timeoutMs: number } | null;
type Task = {
  id: number; revision: number; workflow: string; mode: "record" | "replay"; state: TaskState; elapsedMs: number;
  plan: { recording: { steps: { action: Action; wait: Wait }[] }; steps: { state: StepState; elapsedMs: number; output: { files: { path: string; bytes: number }[] } }[] };
  repair: { stepIndex: number; selector: string; stage: "target" | "wait"; issue: string } | null;
  failure: string | null;
};
export type Highlight = { selector: string; x: number; y: number; width: number; height: number; matches: number };
type Gate = { taskId: number; stepIndex: number; action: Action };
type TaskEvent = { task: Task; highlight: Highlight | null; approval: Gate | null };

function label(action: Action): string {
  switch (action.type) {
    case "navigate": return "Open page";
    case "submit": return "Submit form";
    case "click": return "Follow link";
    case "fill": return "Apply filter";
    case "download_links": return "Download files";
  }
}

export default function TaskWorkspace({ ready, busy, portal, setBusy, report, errorMessage, onHighlight }: {
  onHighlight: (highlight: Highlight | null) => void;
  ready: boolean; busy: boolean; portal: string; setBusy: (busy: boolean) => void;
  report: (text: string) => void; errorMessage: (error: unknown) => string;
}) {
  const [gate, setGate] = useState<Gate | null>(null);
  const [details, setDetails] = useState(false);
  const [deciding, setDeciding] = useState(false);
  const modal = useRef<HTMLDialogElement>(null);
  useEffect(() => { if (gate) modal.current?.showModal(); else modal.current?.close(); }, [gate]);
  const [workflow, setWorkflow] = useState("");
  const [task, setTask] = useState<Task | null>(null);
  const [highlight, setHighlight] = useState<Highlight | null>(null);
  const [runPortal, setRunPortal] = useState("");
  const [saveName, setSaveName] = useState("");
  const [saving, setSaving] = useState(false);
  const [savedId, setSavedId] = useState<string | null>(null);

  useEffect(() => {
    if (!ready) return;
    const saved = localStorage.getItem("clinch-last-task");
    if (!saved || !/^\d+$/.test(saved)) return;
    let active = true;
    invoke<Task>("get_task", { id: Number(saved) }).then(result => { if (active) setTask(result); })
      .catch(() => { if (active) localStorage.removeItem("clinch-last-task"); });
    return () => { active = false; };
  }, [ready]);

  async function run() {
    setBusy(true); setHighlight(null); onHighlight(null);
    setRunPortal(portal); setSaveName(`${workflow}-playbook`); setSavedId(null);
    const progress = new Channel<TaskEvent>();
    progress.onmessage = event => {
      setTask(event.task); setHighlight(event.highlight); onHighlight(event.highlight);
      setGate(event.approval); setDetails(false);
      localStorage.setItem("clinch-last-task", String(event.task.id));
    };
    try {
      // No manual selectors: saved macros replay by workflow name alone.
      // Workflows without a recording fail validation — describe them in
      // plain language instead.
      const result = await invoke<Task>("run_task", {
        request: { workflow, portalUrl: portal, linkSelector: null, downloadSelector: "" }, progress,
      });
      setTask(result); setHighlight(null);
      report(result.state === "completed"
        ? `Task ${result.id} (${result.workflow}) completed in ${result.elapsedMs} ms · ${result.mode === "record" ? "macro recorded" : "CDP replay"}.`
        : `Task ${result.id} (${result.workflow}): ${result.state.replaceAll("_", " ")}. Review the step below before starting another run.`);
    } catch (error) { report(errorMessage(error)); }
    finally { setBusy(false); setGate(null); setHighlight(null); onHighlight(null); }
  }

  async function saveAsPlaybook() {
    if (!task || task.state !== "completed" || !saveName.trim()) return;
    setSaving(true);
    try {
      const id = await invoke<string>("save_playbook", {
        name: saveName.trim(),
        portalUrl: runPortal,
        steps: task.plan.recording.steps.map(step => ({
          kind: "legacy_selector",
          action: step.action,
          wait: step.wait,
        })),
      });
      setSavedId(id);
      report(`Saved playbook ${saveName.trim()} (id ${id}) — replay it in one click from the workflow list.`);
    } catch (error) { report(errorMessage(error)); }
    finally { setSaving(false); }
  }

  async function fileAction(index: number, reveal: boolean) {
    if (!task) return;
    try { await invoke("downloaded_file_action", { id: task.id, index, reveal }); }
    catch (error) { report(errorMessage(error)); }
  }

  async function decide(approved: boolean) {
    if (!gate || deciding) return;
    setDeciding(true);
    try {
      await invoke("task_decision", { id: gate.taskId, index: gate.stepIndex, approved });
      setGate(current => current?.taskId === gate.taskId && current.stepIndex === gate.stepIndex ? null : current);
    }
    catch (error) { report(errorMessage(error)); }
    finally { setDeciding(false); }
  }

  return <section className="task-workspace" aria-label="Task workspace">
    <dialog ref={modal} onCancel={event => { event.preventDefault(); void decide(false); }} aria-labelledby="sentinel-title">
      <div className="eyebrow">SENTINEL GATE</div><h2 id="sentinel-title">Review this action</h2>
      <p>Run {gate?.taskId} · Step {(gate?.stepIndex ?? 0) + 1}. Execution is paused until you decide.</p>
      {gate && <p>{label(gate.action)} · <code>{"selector" in gate.action ? gate.action.selector : gate.action.url}</code></p>}
      {gate?.action.type === "fill" && <p>Value: <code>{gate.action.value}</code></p>}
      {details && <pre>{JSON.stringify(gate?.action, null, 2)}</pre>}
      <div className="actions"><button disabled={deciding} onClick={() => setDetails(!details)}>Review Details</button><button disabled={deciding} onClick={() => void decide(false)}>Reject</button><button className="primary" disabled={deciding} onClick={() => void decide(true)}>Approve &amp; Submit</button></div>
    </dialog>
    <div className="eyebrow">TASK WORKSPACE</div>
    <h3>Replay saved paths.</h3>
    <p>Enter a saved workflow name to replay its recorded path, or describe new work in plain language above. Connect the portal and finish signing in first.</p>
    <form onSubmit={event => { event.preventDefault(); void run(); }}>
      <fieldset disabled={!ready || busy}>
        <label>Workflow name<input required pattern="[A-Za-z0-9_-]+" maxLength={64} value={workflow} onChange={event => setWorkflow(event.target.value)} /></label>
        <button className="primary" disabled={!portal} type="submit">{busy ? "Task in progress…" : "Run Task"}</button>
      </fieldset>
    </form>
    {task && <div className="run-result" aria-live="polite">
      <div className="run-heading"><strong>Run {task.id} · {task.workflow} · {task.mode}</strong><span>{task.state.replaceAll("_", " ")} · {task.elapsedMs} ms</span></div>
      <ol>{task.plan.steps.map((step, index) => <li key={index}>
        <span>{String(index + 1).padStart(2, "0")}</span>
        <div><strong>{label(task.plan.recording.steps[index].action)}</strong><small>{step.state.replaceAll("_", " ")} · {step.elapsedMs} ms</small></div>
      </li>)}</ol>
      {highlight && <p className="target">Current target: <code>{highlight.selector}</code> · {highlight.matches} match(es), {Math.round(highlight.width)} × {Math.round(highlight.height)} at ({Math.round(highlight.x)}, {Math.round(highlight.y)})</p>}
      {task.repair && <p className="status">Step {task.repair.stepIndex + 1} needs selector repair: <code>{task.repair.selector}</code> ({task.repair.issue.replaceAll("_", " ")}).
        {task.repair.stage === "wait" ? " The action already ran; its wait condition failed. It has not been retried." : " Execution stopped at this target."} A bounded repair could not be validated. Check the local provider configuration and selector context.</p>}
      {task.state === "failed" && <p className="status">Execution failed: {task.failure?.replaceAll("_", " ") ?? "unknown error"}. Check the portal session and download availability. Completed steps were saved; no automatic retry was attempted.</p>}
      {task.state === "interrupted" && <p className="status">The app stopped before this run finished. Its last checkpoint is preserved. Review any downloaded files before starting a new run.</p>}
      {task.plan.steps.flatMap(step => step.output.files).map((file, index) => <div className="download-file" key={file.path}>
        <p>Saved locally / {file.bytes} bytes<br /><code>{file.path}</code></p>
        {task.state === "completed" && <div className="actions"><button onClick={() => void fileAction(index, false)}>Open File</button><button onClick={() => void fileAction(index, true)}>Show in Folder</button></div>}
      </div>)}
      {task.state === "completed" && <div className="gate-card" aria-label="Save as playbook">
        <div className="eyebrow">SAVE AS PLAYBOOK</div>
        {savedId
          ? <p>Saved as a 1-click playbook (id <code>{savedId}</code>) — find it in the workflow list.</p>
          : <>
            <p>Task finished. Save this path as a 1-click Playbook?</p>
            <div className="fields"><label>Playbook name<input required pattern="[A-Za-z0-9_-]+" maxLength={64} value={saveName} onChange={event => setSaveName(event.target.value)} /></label></div>
            <div className="actions"><button className="primary" disabled={!saveName.trim() || saving} onClick={() => void saveAsPlaybook()}>{saving ? "Saving…" : "Save"}</button></div>
          </>}
      </div>}
    </div>}
  </section>;
}
