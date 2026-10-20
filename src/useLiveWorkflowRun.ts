import { useEffect, useState } from "react";
import { listWorkflowRuns, onWorkflowRunEvent, onWorkflowRunUpdated, type WorkflowRun } from "./settings";
import { mergeRunSnapshot } from "./workflowRunState";

export function useLiveWorkflowRun() {
  const [run, setRun] = useState<WorkflowRun | null>(null);
  const running = run !== null && ["queued", "running", "needsInput", "cancelling"].includes(run.state);
  useEffect(() => {
    if (!run) return;
    let active = true;
    let frame: number | null = null;
    let pending: WorkflowRun["events"] = [];
    const cleanups: (() => void)[] = [];
    const keep = (unlisten: () => void) => { if (active) cleanups.push(unlisten); else unlisten(); };
    const receive = (current: WorkflowRun) => { if (active && current.id === run.id) setRun(previous => mergeRunSnapshot(previous, current)); };
    const refresh = () => { listWorkflowRuns().then(runs => { const current = runs.find(item => item.id === run.id); if (current) receive(current); }).catch(() => {}); };
    const updates = onWorkflowRunUpdated(receive).then(keep).catch(() => {});
    const output = onWorkflowRunEvent(([id, event]) => {
      if (!active || id !== run.id) return;
      pending.push(event);
      if (pending.length > 2000) pending = pending.slice(-2000);
      if (frame !== null) return;
      frame = window.requestAnimationFrame(() => {
        frame = null;
        const events = pending.splice(0);
        if (active) setRun(previous => previous ? mergeRunSnapshot(previous, { ...previous,
          nextSequence: Math.max(previous.nextSequence, ...events.map(item => item.sequence + 1)), events }) : previous);
      });
    }).then(keep).catch(() => {});
    // Subscribe first, then recover output emitted before the start response.
    void Promise.all([updates, output]).then(() => { if (active) refresh(); });
    const timer = running ? window.setInterval(refresh, 5000) : undefined;
    return () => { active = false; window.clearInterval(timer); if (frame !== null) window.cancelAnimationFrame(frame); cleanups.forEach(cleanup => cleanup()); };
  }, [run?.id, running]);
  return { run, setRun, running };
}
