import type { WorkflowRun } from "./settings";

export function latestGitStep(run: WorkflowRun): string | null {
  if (run.workflow?.action !== "dev-push") return null;
  for (const event of [...run.events].reverse()) {
    const match = /^GIT_STEP:[a-z-]+:(.+)$/.exec(event.text);
    if (match) return match[1];
  }
  return null;
}
export function gitOutputText(run: WorkflowRun): string {
  return run.events.filter(event => !event.text.startsWith("NAVIS_SIDE_EFFECTS:") && !event.text.startsWith("NAVIS_PR_MERGE:") && !event.text.startsWith("NAVIS_LOCAL_SYNC:"))
    .map(event => event.text.replace(/^GIT_STEP:[a-z-]+:/, "")).join("\n");
}

export function pullRequestMergeStatus(run: WorkflowRun): string | null {
  if (run.workflow?.action !== "dev-push") return null;
  for (const event of [...run.events].reverse()) {
    if (!event.text.startsWith("NAVIS_PR_MERGE:")) continue;
    try {
      const value = JSON.parse(event.text.slice("NAVIS_PR_MERGE:".length));
      if (value.state === "merged" && /^[a-f0-9]{40,64}$/i.test(value.mergeCommit)) return `PR merged · ${value.mergeCommit.slice(0, 8)}`;
      if (value.state === "queued") return "Auto-merge enabled · waiting for GitHub checks and reviews";
      if (value.state === "waiting") return "Waiting for checks and reviews before merging";
      if (value.state === "stopped") return "Automatic merge stopped · PR preserved";
    } catch { /* Ignore malformed output; never infer a successful merge. */ }
  }
  return null;
}

export function localSyncStatus(run: WorkflowRun): string | null {
  if (run.workflow?.action !== "dev-push") return null;
  for (const event of [...run.events].reverse()) {
    if (!event.text.startsWith("NAVIS_LOCAL_SYNC:")) continue;
    try {
      const value = JSON.parse(event.text.slice("NAVIS_LOCAL_SYNC:".length));
      if (value.state === "synced" && /^[a-f0-9]{40,64}$/i.test(value.commit)) return `Local ${String(value.branch)} synced · ${value.commit.slice(0, 8)}`;
      if (["skipped", "attention", "waiting", "pending"].includes(value.state) && typeof value.message === "string") return value.message;
    } catch { /* Never infer a successful local sync from malformed output. */ }
  }
  return null;
}
