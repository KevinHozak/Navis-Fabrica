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

export type ProviderKind = "codex" | "antigravity" | "ollama" | "custom";
export type ModelProfile = { id: string; name: string; reasoningEfforts: string[] };
export type ProviderProfile = {
  id: string;
  kind: ProviderKind;
  name: string;
  executable: string;
  secretRef: string | null;
  models: ModelProfile[];
};
export type ProviderSelection = {
  providerId: string | null;
  modelId: string | null;
  reasoningEffort: string | null;
  runtimeId: string | null;
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
  id: string; executable: string; provider: string | null; cwd: string; threadId: string | null; mutating: boolean; state: string;
  startedAtMs: number; finishedAtMs: number | null; exitCode: number | null;
  nextSequence: number; events: WorkflowRunEvent[];
  workflow: WorkflowRunMetadata | null;
};
export type WorkflowRunMetadata = {
  action: "dev-work" | "dev-push";
  operationKey: string;
  intent: string;
  issueUrl: string | null;
  scopePaths: string[];
  requestedEffects: ("commit" | "push" | "createPullRequest")[];
  sideEffects: { commitSha: string | null; remoteBranch: string | null; pullRequestUrl: string | null; verified: boolean; verificationMessage: string | null } | null;
};
export type CodexStartRequest = {
  cwd: string; prompt: string; skillId: string | null; timeoutSeconds: number;
  workflow?: WorkflowRunMetadata;
};
export type AntigravityStartRequest = { cwd: string; prompt: string; timeoutSeconds: number };
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
