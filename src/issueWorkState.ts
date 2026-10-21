import { RunnerCommandError } from "./batchClient";

export function submissionDisposition(reason: unknown): "admitted" | "refused" | "unknown" {
  if (!(reason instanceof RunnerCommandError)) return "unknown";
  return reason.code === "WORKER_LAUNCH_FAILED" ? "admitted" : "refused";
}

export function previewBlocked(plan: { issues?: { tasks?: { eligible: boolean }[] }[] }): boolean {
  return !plan.issues?.length || plan.issues.some(issue => !issue.tasks?.length || issue.tasks.some(task => !task.eligible));
}
