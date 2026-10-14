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
};

export type SettingsInput = Omit<Settings, "schemaVersion">;

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
  schemaVersion: 1,
  projectRoot: "C:\\Projects",
  exclusions: [".git", ".worktrees", "node_modules", "target"],
  preferences: { includeHidden: false, scanDepth: 3 },
};

const previewStorageKey = "navis-fabrica.settings.v1";

export const isDesktopApp = () => isTauri();

export const startProjectScan = (scanId: string) =>
  invoke<void>("start_project_scan", { scanId });

export const cancelProjectScan = (scanId: string) =>
  invoke<boolean>("cancel_project_scan", { scanId });

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
      schemaVersion: 1,
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
    };
  } catch {
    return defaultSettings;
  }
}

export async function saveSettings(settings: SettingsInput): Promise<Settings> {
  if (isDesktopApp()) return invoke<Settings>("save_settings", { settings });

  const saved = { schemaVersion: 1, ...settings };
  localStorage.setItem(previewStorageKey, JSON.stringify(saved));
  return saved;
}
