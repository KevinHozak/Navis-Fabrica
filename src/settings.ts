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
};

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
    runStart: boolean;
    runCancel: boolean;
    eventStream: boolean;
    checkpoints: boolean;
  };
  note: string;
};

export type ProjectEntry = {
  id: string;
  name: string;
  path: string;
  isGitRepository: boolean;
  isWorktree: boolean;
  detectedStacks: string[];
};

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
  schemaVersion: 2,
  projectRoot: "C:\\Projects",
  exclusions: [".git", ".worktrees", "node_modules", "target"],
  preferences: { includeHidden: false, scanDepth: 3 },
  providers: [],
  providerSelection: { providerId: null, modelId: null, reasoningEffort: null, runtimeId: null },
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

export const startProjectScan = (scanId: string) =>
  invoke<void>("start_project_scan", { scanId });

export const cancelProjectScan = (scanId: string) =>
  invoke<boolean>("cancel_project_scan", { scanId });

export const inspectProviders = (profiles: ProviderProfile[]) =>
  invoke<ProviderAvailability[]>("inspect_providers", { profiles });

export const onProjectScanProgress = (handler: (progress: ProjectScanProgress) => void) =>
  listen<ProjectScanProgress>("project-scan-progress", (event) => handler(event.payload));

export const onProjectScanFinished = (handler: (result: ProjectScanFinished) => void) =>
  listen<ProjectScanFinished>("project-scan-finished", (event) => handler(event.payload));

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
    };
  } catch {
    return defaultSettings;
  }
}

export async function saveSettings(settings: SettingsInput): Promise<Settings> {
  if (isDesktopApp()) return invoke<Settings>("save_settings", { settings });

  validatePreviewSecretReferences(settings.providers);
  const saved = { schemaVersion: 2, ...settings };
  localStorage.setItem(previewStorageKey, JSON.stringify(saved));
  return saved;
}
