import { invoke } from "@tauri-apps/api/core";

export type Task = { task_id: string; kind: string; files: string[]; acceptance: string[]; provider_id: string | null; model_id: string | null; route_requirements?: { complexity?: "simple" | "standard" | "complex"; pace?: "fast" | "balanced" | "thorough" | "economy"; require_tools?: boolean; local_only?: boolean; context_tokens?: number; max_input_tokens?: number; max_output_tokens?: number; allowed_candidates?: { provider_id: string; model_id: string }[] }; limits: { max_attempts: number; max_tokens: number | null; max_cost_micros: number | null; max_seconds: number | null } };
export type BatchJob = { issue_id: string; plan: { tasks: Task[] }; dependencies: { issue_id: string; resolution: { kind: string } }[] };
export type DispatchRefreshConsent = { mode: "disabled" } | { mode: "providers"; kinds: ("codex" | "ollama" | "openRouter")[] };
export type BatchRequest = Record<string, unknown> & { batchId: string; projectPath: string; settingsPath: string; jobs: BatchJob[]; verification: { issueId: string; reviewer: { task_id: string; provider_id: string; model_id: string; reasoning_effort?: string | null }; acceptanceCriteria: string[]; checks: { id: string }[] }[]; policy: { windowSeconds: number; maxConcurrency: number; permittedEffects: string[]; budget: { maxTokens: number; maxCostMicros: number; maxSeconds: number } }; providerObservationFingerprint?: string | null; resolvedRoutes?: Record<string, unknown>; dispatchRefreshConsent?: DispatchRefreshConsent; routeCommitments?: Record<string, unknown> };
export type PlannedTaskRouting = { task_id: string; provider_id?: string | null; model_id?: string | null; assignment_source: string; eligible: boolean; reasons: string[]; route_provenance?: unknown };
export type PlannedIssue = { issue_id: string; eligibility?: string; routing_reasons?: string[]; tasks?: PlannedTaskRouting[] };
export type BatchPreview = { request: BatchRequest; plan: { warnings: string[]; worker_count: number; issues?: PlannedIssue[] }; providerObservationFingerprint: string; fingerprint: string };
export type ProviderObservationRefresh = { schemaVersion: number; snapshotFingerprint: string; observedAtMs: number; providers: { providerId: string; kind: string; runtimeInstalled: { state: string; value?: boolean | null; reason?: string | null }; authenticated: { state: string; value?: boolean | null; reason?: string | null }; healthy: { state: string; value?: boolean | null; reason?: string | null }; models: { modelId: string; modelAvailable: { state: string; value?: boolean | null; reason?: string | null } }[] }[] };
export type BatchSnapshot = { batchId: string; state: string; jobIds: number[]; deadlineMs: number; controlNote: string | null };
export type JobStatus = { id: number; issueId: string; state: string; completionGate: string; attemptCount: number; maxAttempts: number; lastAttemptSummary?: string | null; checkpoints?: unknown[]; checkpointsTruncated?: boolean };
export type BatchStatus = { snapshot: BatchSnapshot; jobs: JobStatus[]; summary: Record<string, unknown>; budget: { costUsedMicros: number; maxCostMicros: number; tokensUsed: number; maxTokens: number; unresolved: number; reserved: number; dispatched: number } };
export type BatchEvent = { sequence: number; timestampMs: number; kind: string; payload: unknown };
export type EventPage = { events: BatchEvent[]; nextCursor: number; hasMore: boolean };
export type BatchOutcome = { kind: "preview"; value: BatchPreview } | { kind: "submitted" | "control"; value: BatchSnapshot } | { kind: "status"; value: BatchStatus } | { kind: "events"; value: EventPage } | { kind: "reconciled" | "recovery"; value: unknown };
export type ProviderChoices = { providers: { id: string; name: string; models: { id: string; name: string; reasoningEfforts?: string[] }[] }[] };
export type RecoveryRequest = { jobId: number; decision: "safe_to_retry" | "keep_needs_input" | "mark_failed"; evidence: string; processConfirmedStopped: boolean; effectsReconciled: boolean };
type Operation = "batch-preview" | "submit" | "status" | "events" | "pause" | "resume" | "stop" | "cancel" | "reconcile" | "recover";
type Fields = { dataRoot: string; batchId?: string; batchRequest?: BatchRequest; batchRequestPath?: string; previewFingerprint?: string; cursor?: number; limit?: number; recovery?: RecoveryRequest };
export class RunnerCommandError extends Error {
  constructor(readonly code: string, message: string) {
    super(`${code}: ${message}`);
    this.name = "RunnerCommandError";
  }
}
type Response<T> = { protocolVersion: number; ok: boolean; result: T; error: { code: string; message: string } | null };
async function execute<T>(request: object): Promise<T> {
  const response = await invoke<Response<T>>("runner_core_execute", { request: { protocolVersion: 1, ...request } });
  if (response.protocolVersion !== 1) throw new Error("Unsupported runner protocol");
  if (!response.ok) throw new RunnerCommandError(response.error?.code ?? "RUNNER_ERROR", response.error?.message ?? "Request failed");
  return response.result;
}
export const batchCommand = (operation: Operation, fields: Fields) => execute<BatchOutcome>({ operation, ...fields });
export const runnerProviders = (settingsPath: string) => execute<ProviderChoices>({ operation: "provider-settings", settingsPath });
export const refreshProviderObservations = (dataRoot: string, settingsPath: string, allowLiveProbe: boolean) => execute<ProviderObservationRefresh>({ operation: "provider-observations-refresh", dataRoot, settingsPath, allowLiveProbe });
export const runnerContext = () => invoke<{ dataRoot: string; workerAvailable?: boolean }>("get_runner_context");
