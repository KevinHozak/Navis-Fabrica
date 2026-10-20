import type { WorkflowRun } from "./settings";

export function mergeRunSnapshot(previous: WorkflowRun | null, next: WorkflowRun): WorkflowRun {
  if (!previous || previous.id !== next.id) return next;
  if (previous.finishedAtMs !== null && next.finishedAtMs === null) return previous;
  const events = new Map([...next.events, ...previous.events].map(event => [event.sequence, event]));
  const metadata = next.nextSequence < previous.nextSequence && next.finishedAtMs === null ? previous : next;
  return { ...metadata, nextSequence: Math.max(previous.nextSequence, next.nextSequence),
    events: [...events.values()].sort((a, b) => a.sequence - b.sequence).slice(-2000) };
}
