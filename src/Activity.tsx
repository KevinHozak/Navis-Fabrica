import { useEffect, useState } from "react";
import { isDesktopApp, listWorkflowRuns, onWorkflowRunUpdated, type WorkflowRun, type ProjectEntry } from "./settings";
export type BatchActivity = { batchId: string; projectPath?: string; state: string; needsAttention: boolean };
let latestBatchActivity: BatchActivity | null = null;
export function publishBatchActivity(value: BatchActivity | null) {
  latestBatchActivity = value;
  window.dispatchEvent(new CustomEvent("fabrica-batch-activity", { detail: value }));
}
export function useBatchActivity() {
  const [batch, setBatch] = useState(latestBatchActivity);
  useEffect(() => {
    const changed = (event: Event) => setBatch((event as CustomEvent<BatchActivity | null>).detail);
    window.addEventListener("fabrica-batch-activity", changed);
    return () => window.removeEventListener("fabrica-batch-activity", changed);
  }, []);
  return batch;
}
const active = new Set(["queued", "running", "cancelling"]);
export const sameProjectPath = (a: string, b: string) => a.replaceAll("/", "\\").replace(/\\+$/, "").toLowerCase() === b.replaceAll("/", "\\").replace(/\\+$/, "").toLowerCase();
export function useWorkflowActivity() {
  const [runs, setRuns] = useState<WorkflowRun[] | null>(null);
  useEffect(() => {
    if (!isDesktopApp()) return;
    let alive = true; let cleanup: (() => void) | undefined;
    const refresh = () => { listWorkflowRuns().then(value => { if (alive) setRuns(value); }).catch(() => { if (alive) setRuns(null); }); };
    refresh(); onWorkflowRunUpdated(refresh).then(unlisten => { if (alive) cleanup = unlisten; else unlisten(); }).catch(() => { if (alive) setRuns(null); });
    return () => { alive = false; cleanup?.(); };
  }, []);
  return runs;
}
export function activityLabel(runs: WorkflowRun[] | null) {
  if (runs === null) return "Activity unavailable";
  if (runs.some(r => r.state === "needsInput")) return "Needs attention";
  const count = runs.filter(r => active.has(r.state)).length;
  if (count) return `${count} active`;
  const latest = [...runs].sort((a, b) => b.startedAtMs - a.startedAtMs)[0];
  return latest && ["failed", "interrupted"].includes(latest.state) ? "Needs attention" : "No active work";
}
export function ActivityEntry() {
  const runs = useWorkflowActivity();
  const batch = useBatchActivity();
  return <a className="activity-entry" href="#runs">Activity · {activityLabel(runs)}{batch ? ` · Batch ${batch.needsAttention ? "needs attention" : batch.state}` : ""}</a>;
}
export function ActivityOverview({ projects, onSelect }: { projects: ProjectEntry[]; onSelect: (id: string) => void }) {
  const runs = useWorkflowActivity();
  const batch = useBatchActivity();
  return <section className="panel activity-overview"><h2>Recent activity</h2>{batch && <p><a href="#batches">Local batch {batch.batchId}: {batch.needsAttention ? "needs attention" : batch.state}</a></p>}{runs === null ? <p>Activity unavailable outside the desktop app, or the activity read failed.</p> : runs.length ? [...runs].sort((a,b) => b.startedAtMs-a.startedAtMs).slice(0,20).map(run => {
    const project = projects.find(p => sameProjectPath(p.path,run.cwd));
    return <div className="activity-row" key={run.id}><span><strong>{project?.name ?? run.cwd}</strong><small>{run.state} · {run.workflow?.action ?? run.provider ?? "Configured task"}</small></span>{project && <button className="text-button" onClick={() => onSelect(project.id)}>Open actions & output</button>}</div>;
  }) : <p>No recorded work yet.</p>}<a href="#batches">Open local issue batches</a></section>;
}
