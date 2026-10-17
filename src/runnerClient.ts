import { invoke } from "@tauri-apps/api/core";

export type IssueRecord = { frontmatter: Record<string, unknown>; body: string };
export type EpicProgress = { totalChildren: number; acceptedChildren: number; incompleteChildren: string[]; complete: boolean };
export type ListedIssue = { record: IssueRecord; revision: string; unresolvedDependencies: string[]; unresolvedParents: string[]; epicProgress?: EpicProgress };
export type IssuePlan = { readyToStart: boolean; dependencies: { id: string; state: string; satisfied: boolean }[]; blockers: { id: string; state: string }[]; executionSupported: boolean };
export type IssueSummary = { id: string; title: string; status: string; type: string; priority: unknown; parent: string[]; dependsOn: string[]; revision: string; unresolvedDependencies: string[]; unresolvedParents: string[]; epicProgress?: EpicProgress };
export type IssuePage = { items: IssueSummary[]; total: number; offset: number; limit: number };
type IssueOperation = "issues-page" | "issues-list" | "issues-show" | "issues-update" | "plan";
type CoreResponse<T> = { protocolVersion: number; ok: boolean; result: T; error: { code: string; message: string } | null };

export async function issueCommand<T>(operation: IssueOperation, projectPath: string, configPath: string, fields: { offset?: number; limit?: number; issueId?: string; expectedRevision?: string; record?: IssueRecord } = {}): Promise<T> {
  const response = await invoke<CoreResponse<T>>("runner_core_execute", { request: { protocolVersion: 1, operation, projectPath, configPath, ...fields } });
  if (response.protocolVersion !== 1) throw new Error("Unsupported runner protocol version");
  if (!response.ok) throw new Error(`${response.error?.code ?? "RUNNER_ERROR"}: ${response.error?.message ?? "Request failed"}`);
  return response.result;
}
