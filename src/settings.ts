import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type Preferences = {
  includeHidden: boolean;
  scanDepth: number;
};

export type Settings = {
  schemaVersion: number;
  projectRoot: string;
  exclusions: string[];
  preferences: Preferences;
  providers: ProviderProfile[];
  providerSelection: ProviderSelection;
  githubAccounts: GitHubAccount[];
  selectedGithubAccountId: string | null;
};

export type GitHubAccount = { id: string; login: string; credentialRef: string | null };

export type SettingsInput = Omit<Settings, "schemaVersion">;

export type ProviderKind = "codex" | "antigravity" | "ollama" | "openRouter" | "custom";
export type ModelProfile = { id: string; name: string; reasoningEfforts: string[]; contextWindowTokens?: number | null; supportsTools?: boolean | null; qualityRating?: number | null };
export type ProviderProfile = {
  id: string;
  kind: ProviderKind;
  name: string;
  executable: string;
  endpoint: string | null;
  secretRef: string | null;
  models: ModelProfile[];
};
export type ProviderSelection = {
  providerId: string | null;
  modelId: string | null;
  reasoningEffort: string | null;
  runtimeId: string | null;
};
export type UsageCategory = "subscriptionWindow" | "apiSpend" | "contextLimit" | "localResource";
export type UsageSignal = {
  id: string;
  providerId: string | null;
  modelId: string | null;
  category: UsageCategory;
  metric: string;
  label: string;
  value: number | null;
  unit: string;
  windowDurationSeconds: number | null;
  resetAtMs: number | null;
  collectedAtMs: number;
  source: string;
  confidence: string;
  status: "available" | "estimated" | "unknown" | "stale" | "unavailable" | "rateLimited";
  note: string | null;
};
export type RoutingComplexity = "simple" | "standard" | "complex";
export type RoutingPace = "fast" | "balanced" | "thorough" | "economy";
export type RoutingCandidate = {
  providerId: string; telemetryProviderId: string; providerName: string; modelId: string; modelName: string; isLocal: boolean;
  contextTokens: number | null; supportsTools: boolean | null; reasoningEfforts: string[];
  qualityRating: number | null; latencyMs: number | null; latencyCollectedAtMs: number | null;
};
export type RoutingDecision = {
  selectedProviderId: string | null; selectedModelId: string | null; requiredContextTokens: number;
  desiredCompletionNote: string;
  candidates: { providerId: string; providerName: string; modelId: string; modelName: string; eligible: boolean; selected: boolean; reasons: string[] }[];
};
export type FallbackTrigger = "providerFailure" | "quotaBoundary" | "quotaUnknown";
export type FallbackPolicy = { enabled: boolean; onProviderFailure: boolean; onQuotaBoundary: boolean; localOnly: boolean; maxHandoffs: number };
export type FallbackDecision = {
  policyVersion: number; accepted: boolean; localOnly: boolean; sourceRunId: string; trigger: FallbackTrigger; sourceProviderId: string;
  targetProviderId: string | null; targetModelId: string | null; checkpointRef: string | null; handoffCount: number;
  reason: string; routing: RoutingDecision | null;
};
export type FallbackEvaluationRequest = {
  sourceRunId: string; policy: FallbackPolicy; trigger: FallbackTrigger; currentProviderId: string;
  visitedProviderIds: string[]; handoffCount: number; sourceRunTerminal: boolean; safeCheckpointRef: string | null;
  effectsReconciled: boolean;
  routing: {
    complexity: RoutingComplexity; pace: RoutingPace; requireTools: boolean; localOnly: boolean;
    desiredCompletionMinutes: number | null; manualProviderId: string | null; manualModelId: string | null;
    candidates: RoutingCandidate[]; usageSignals: UsageSignal[]; nowMs: number;
  };
};
export type RoutingCalibrationSample = { providerId: string; modelId: string; succeeded: boolean; latencyMs: number | null; measuredUsage: number | null; usageUnit: string | null };
export type RoutingCalibrationSummary = {
  providerId: string; modelId: string; sampleCount: number; successRate: number; medianLatencyMs: number | null;
  measuredUsageByUnit: { unit: string; sampleCount: number; medianValue: number }[]; unknownUsageCount: number;
};
export type ProviderAvailability = {
  providerId: string;
  executableFound: boolean;
  executablePath: string | null;
  authenticationReference: "missing" | "referenceConfigured";
  capabilities: {
    discovery: boolean;
    health: boolean;
    modelListing: boolean;
    skillSupport: boolean;
    runStart: boolean;
    runCancel: boolean;
    eventStream: boolean;
    approvalRequests: boolean;
    interactiveInput: boolean;
    checkpoints: boolean;
  };
  note: string;
};
export type AntigravityProbe = { version: string | null; state: "ready" | "authenticationRequired" | "unavailable"; models: string[]; note: string };
export type OllamaModel = { id: string; sizeBytes: number; contextTokens: number | null; toolsSupported: boolean; eligible: boolean; note: string };
export type OllamaProbe = { state: "ready" | "unavailable"; endpoint: string; models: OllamaModel[]; availableMemoryBytes: number | null; note: string };
export type OpenRouterModel = { id: string; name: string; contextLength: number | null; promptPrice: string | null; completionPrice: string | null; inputModalities: string[] | null; outputModalities: string[] | null; supportedParameters: string[] | null };
export type OpenRouterProbe = { state: "ready" | "authenticationRequired" | "rateLimited" | "unavailable"; models: OpenRouterModel[]; note: string };

export type ProjectEntry = {
  id: string;
  name: string;
  path: string;
  isGitRepository: boolean;
  isWorktree: boolean;
  detectedStacks: string[];
  git: GitFacts;
  workflowPrerequisites: WorkflowPrerequisite[];
};

export type GitHubActivity = {
  owner: string;
  repository: string;
  repositoryUrl: string;
  issuesUrl: string;
  pullsUrl: string;
  issueCount: number | null;
  pullRequestCount: number | null;
  recentCommits: { hash: string; subject: string; url: string }[];
  checkedAt: string | null;
  remoteStatus: "unchecked" | "fresh" | "rateLimited" | "unauthorized" | "authExpired" | "unavailable";
  authentication: "notConfigured" | "available" | "expired";
};

export type GitHubIssueSummary = { number: number; title: string; url: string; state: string };
export type GitHubIssueLink = { number: number; title: string; url: string; state: string };
export type GitHubIssue = GitHubIssueSummary & {
  body: string;
  acceptanceCriteria: string[];
  blockedBy: GitHubIssueLink[];
  blockersTruncated: boolean;
  blockersAvailable: boolean;
  parentEpic: GitHubIssueLink | null;
  pullRequestUrls: string[];
  selectable: boolean;
  blocked: boolean;
};

export type WorkflowRunEvent = { sequence: number; timestampMs: number; kind: string; text: string };
export type WorkflowRun = {
  id: string; executable: string; provider: string | null; providerProfileId?: string | null; cwd: string; threadId: string | null; processId: number | null; mutating: boolean; timeoutSeconds: number; state: string;
  modelId: string | null; startedAtMs: number; finishedAtMs: number | null; latencyMs: number | null; exitCode: number | null;
  nextSequence: number; events: WorkflowRunEvent[];
  workflow: WorkflowRunMetadata | null;
  handoff: ProviderHandoff | null;
};
export type ProviderHandoff = { version: number; sourceRunId: string; targetProviderId: string; workingTreeRoot: string; workingTreePaths: string[]; workingTreeFingerprint: string | null; goal: string; scope: string; diff: string; checks: string; pendingWork: string; completedSideEffects: { commitSha: string | null; remoteBranch: string | null; pullRequestUrl: string | null; verified: boolean; verificationMessage: string | null } | null; fallbackDecisions: FallbackDecision[]; createdAtMs: number };
export type HandoffInput = { sourceRunId: string; targetProviderId: string; goal: string; scope: string; diff: string; checks: string; pendingWork: string };
export type WorkflowRunMetadata = {
  action: "dev-work" | "dev-push" | "dev-build";
  operationKey: string;
  intent: string;
  buildProfile?: "release" | "debug";
  issueUrl: string | null;
  scopePaths: string[];
  requestedEffects: ("commit" | "push" | "createPullRequest")[];
  sideEffects: { commitSha: string | null; remoteBranch: string | null; pullRequestUrl: string | null; verified: boolean; verificationMessage: string | null } | null;
  buildArtifact?: { profile: string; path: string; fileVersion: string; productVersion: string; verified: boolean; verificationMessage: string } | null;
};
export type CodexStartRequest = {
  cwd: string; prompt: string; skillId: string | null; timeoutSeconds: number; handoffSourceRunId?: string | null;
  workflow?: WorkflowRunMetadata;
};
export type AntigravityStartRequest = { cwd: string; prompt: string; timeoutSeconds: number; handoffSourceRunId?: string | null };
export type WorkflowRunRequest = {
  executable: string; arguments: string[]; cwd: string; timeoutSeconds: number; mutating: boolean;
};

export type GitFacts = {
  status: "available" | "detached" | "unborn" | "unsafeOwnership" | "gitUnavailable" | "notRepository" | "error";
  branch: string | null;
  dirty: boolean | null;
  latestCommit: { shortHash: string; subject: string } | null;
  remotes: { name: string; url: string | null; github: { owner: string; repository: string; canonicalUrl: string } | null }[];
  upstream: { status: "tracking" | "noUpstream" | "unavailable"; ahead: number | null; behind: number | null };
};

export type WorkflowPrerequisite = {
  name: string;
  requiredFor: string[];
  available: boolean;
};

export type WorkflowSkillStatus = {
  id: "dev-work" | "dev-push" | "dev-build";
  status: "available" | "missing" | "unavailable" | "incompatible";
  compatibleAgents: string[];
};

export async function discoverWorkflowSkills(): Promise<WorkflowSkillStatus[]> {
  if (!isTauri()) return ["dev-work", "dev-push", "dev-build"].map((id) => ({ id: id as WorkflowSkillStatus["id"], status: "unavailable", compatibleAgents: [] }));
  return invoke<WorkflowSkillStatus[]>("discover_workflow_skills");
}

export type ProjectScanProgress = {
  scanId: string;
  directoriesScanned: number;
  projectsFound: number;
  currentPath: string;
};

export type ProjectScanFinished = {
  scanId: string;
  projects: ProjectEntry[];
  errors: string[];
  directoriesScanned: number;
  cancelled: boolean;
  fatalError: string | null;
};

export const defaultSettings: Settings = {
  schemaVersion: 3,
  projectRoot: "C:\\Projects",
  exclusions: [".git", ".worktrees", "node_modules", "target"],
  preferences: { includeHidden: false, scanDepth: 3 },
  providers: [],
  providerSelection: { providerId: null, modelId: null, reasoningEffort: null, runtimeId: null },
  githubAccounts: [],
  selectedGithubAccountId: null,
};

const previewStorageKey = "navis-fabrica.settings.v1";

function validatePreviewSecretReferences(providers: ProviderProfile[]): void {
  for (const provider of providers) {
    const reference = provider.secretRef;
    if (reference == null) continue;

    const key = reference.startsWith("credential-store:")
      ? reference.slice("credential-store:".length)
      : reference.startsWith("provider-managed:")
        ? reference.slice("provider-managed:".length)
        : null;
    if (
      reference.length > 260 ||
      /[\u0000-\u001f\u007f-\u009f]/.test(reference) ||
      key === null ||
      key.length === 0 ||
      !/^[a-zA-Z0-9.:/_-]+$/.test(key)
    ) {
      throw new Error("Use an opaque secret reference such as credential-store:provider-name; secret values are not accepted.");
    }
  }
}

export const isDesktopApp = () => isTauri();

export const startProjectScan = (scanId: string, forceRefresh: boolean) =>
  invoke<void>("start_project_scan", { scanId, forceRefresh });

export const cancelProjectScan = (scanId: string) =>
  invoke<boolean>("cancel_project_scan", { scanId });

export const inspectProviders = (profiles: ProviderProfile[]) =>
  invoke<ProviderAvailability[]>("inspect_providers", { profiles });
export const probeAntigravityCli = (providerId: string) =>
  invoke<AntigravityProbe>("probe_antigravity_cli", { providerId });
export const probeOllama = (providerId: string) =>
  invoke<OllamaProbe>("probe_ollama", { providerId });
export const saveOpenRouterCredential = (providerId: string, apiKey: string) =>
  invoke<Settings>("save_openrouter_credential", { providerId, apiKey });
export const probeOpenRouter = (providerId: string) =>
  invoke<OpenRouterProbe>("probe_openrouter", { providerId });
export const listUsageSignals = () => invoke<UsageSignal[]>("list_usage_signals");
export const recordManualUsageEstimate = (estimate: {
  category: UsageCategory;
  value: number;
  unit: string;
  windowDurationSeconds: number | null;
  resetAtMs: number | null;
}) => invoke<UsageSignal[]>("record_manual_usage_estimate", { estimate });
export const evaluateRouting = (request: {
  complexity: RoutingComplexity; pace: RoutingPace; requireTools: boolean; localOnly: boolean;
  desiredCompletionMinutes: number | null; manualProviderId: string | null; manualModelId: string | null;
  candidates: RoutingCandidate[]; usageSignals: UsageSignal[]; nowMs: number;
}) => invoke<RoutingDecision>("evaluate_routing", { request });
export const evaluateProviderFallback = (request: FallbackEvaluationRequest) =>
  invoke<FallbackDecision>("evaluate_provider_fallback", { request });
export const calibrateRouting = (samples: RoutingCalibrationSample[]) =>
  invoke<RoutingCalibrationSummary[]>("calibrate_routing", { samples });

export const getCachedGitHubActivity = (owner: string, repository: string) =>
  invoke<GitHubActivity>("get_cached_github_activity", { owner, repository });

export const refreshGitHubActivity = (owner: string, repository: string) =>
  invoke<GitHubActivity>("refresh_github_activity", { owner, repository });

export const listGitHubIssues = (owner: string, repository: string) =>
  invoke<GitHubIssueSummary[]>("list_github_issues", { owner, repository });

export const getGitHubIssue = (owner: string, repository: string, reference: string) =>
  invoke<GitHubIssue>("get_github_issue", { owner, repository, reference });

export const listWorkflowRuns = () => invoke<WorkflowRun[]>("list_workflow_runs");
export const startWorkflowRun = (request: WorkflowRunRequest) =>
  invoke<WorkflowRun>("start_workflow_run", { request });
export const startCodexWorkflowRun = (request: CodexStartRequest) =>
  invoke<WorkflowRun>("start_codex_workflow_run", { request });
export const startAntigravityWorkflowRun = (request: AntigravityStartRequest) =>
  invoke<WorkflowRun>("start_antigravity_workflow_run", { request });
export const createProviderHandoff = (input: HandoffInput) =>
  invoke<ProviderHandoff>("create_provider_handoff", { input });
export type OllamaStartRequest = { cwd: string; prompt: string; testCommand: string | null; handoffSourceRunId?: string | null };
export const startOllamaWorkflowRun = (request: OllamaStartRequest) =>
  invoke<WorkflowRun>("start_ollama_workflow_run", { request });
export type OpenRouterStartRequest = { cwd: string; prompt: string; testCommand: string | null; timeoutSeconds: number; handoffSourceRunId?: string | null };
export const startOpenRouterWorkflowRun = (request: OpenRouterStartRequest) =>
  invoke<WorkflowRun>("start_openrouter_workflow_run", { request });
export const respondCodexApproval = (runId: string, requestId: string, decision: string) =>
  invoke<boolean>("respond_codex_approval", { runId, requestId, decision });
export const respondCodexUserInput = (runId: string, requestId: string, answers: Record<string, string[]>) =>
  invoke<boolean>("respond_codex_user_input", { runId, requestId, answers });
export const cancelWorkflowRun = (runId: string) =>
  invoke<boolean>("cancel_workflow_run", { runId });
export const markWorkflowRunNeedsInput = (runId: string) =>
  invoke<boolean>("mark_workflow_run_needs_input", { runId });
export const writeWorkflowRunInput = (runId: string, input: string) =>
  invoke<boolean>("write_workflow_run_input", { runId, input });
export const onWorkflowRunUpdated = (handler: (run: WorkflowRun) => void) =>
  listen<WorkflowRun>("workflow-run-updated", (event) => handler(event.payload));
export const onWorkflowRunEvent = (handler: (payload: [string, WorkflowRunEvent]) => void) =>
  listen<[string, WorkflowRunEvent]>("workflow-run-event", (event) => handler(event.payload));

export const onProjectScanProgress = (handler: (progress: ProjectScanProgress) => void) =>
  listen<ProjectScanProgress>("project-scan-progress", (event) => handler(event.payload));

export const onProjectScanFinished = (handler: (result: ProjectScanFinished) => void) =>
  listen<ProjectScanFinished>("project-scan-finished", (event) => handler(event.payload));

export const onProjectScanInvalidated = (handler: () => void) =>
  listen<void>("project-scan-invalidated", handler);

export const onProjectWatchError = (handler: (message: string) => void) =>
  listen<string>("project-watch-error", (event) => handler(event.payload));

export async function loadSettings(): Promise<Settings> {
  if (isDesktopApp()) return invoke<Settings>("get_settings");

  try {
    const stored = localStorage.getItem(previewStorageKey);
    if (!stored) return defaultSettings;
    const parsed: unknown = JSON.parse(stored);
    if (!parsed || typeof parsed !== "object") return defaultSettings;
    const candidate = parsed as Partial<Settings>;
    return {
      schemaVersion: 2,
      projectRoot: typeof candidate.projectRoot === "string" ? candidate.projectRoot : defaultSettings.projectRoot,
      exclusions: Array.isArray(candidate.exclusions) && candidate.exclusions.every((item) => typeof item === "string")
        ? candidate.exclusions
        : defaultSettings.exclusions,
      preferences: {
        includeHidden: typeof candidate.preferences?.includeHidden === "boolean" ? candidate.preferences.includeHidden : defaultSettings.preferences.includeHidden,
        scanDepth: typeof candidate.preferences?.scanDepth === "number" && candidate.preferences.scanDepth >= 1 && candidate.preferences.scanDepth <= 8
          ? candidate.preferences.scanDepth
          : defaultSettings.preferences.scanDepth,
      },
      providers: Array.isArray(candidate.providers) ? candidate.providers : defaultSettings.providers,
      providerSelection: candidate.providerSelection && typeof candidate.providerSelection === "object"
        ? candidate.providerSelection
        : defaultSettings.providerSelection,
      githubAccounts: Array.isArray(candidate.githubAccounts) ? candidate.githubAccounts : defaultSettings.githubAccounts,
      selectedGithubAccountId: typeof candidate.selectedGithubAccountId === "string" ? candidate.selectedGithubAccountId : null,
    };
  } catch {
    return defaultSettings;
  }
}

export async function saveSettings(settings: SettingsInput): Promise<Settings> {
  if (isDesktopApp()) return invoke<Settings>("save_settings", { settings });

  validatePreviewSecretReferences(settings.providers);
  for (const account of settings.githubAccounts) {
    const reference = account.credentialRef;
    if (reference != null && !/^(credential-store|provider-managed):[a-zA-Z0-9.:/_-]+$/.test(reference)) {
      throw new Error("Use an opaque credential-store: or provider-managed: reference; tokens are not accepted.");
    }
  }
  const saved = { schemaVersion: 3, ...settings };
  localStorage.setItem(previewStorageKey, JSON.stringify(saved));
  return saved;
}
