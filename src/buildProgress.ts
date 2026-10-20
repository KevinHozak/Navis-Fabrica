import type { WorkflowRun } from "./settings";

export function latestBuildStep(run: WorkflowRun): string | null {
  if (run.workflow?.action !== "dev-build") return null;
  for (const event of [...run.events].reverse()) {
    for (const line of event.text.split(/\r?\n/).reverse()) {
      const match = /^BUILD_STEP:([1-6])\/6:(.+)$/.exec(line);
      if (match) return `Step ${match[1]} of 6 · ${match[2]}`;
    }
  }
  return null;
}
