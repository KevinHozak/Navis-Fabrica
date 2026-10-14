import { invoke, isTauri } from "@tauri-apps/api/core";

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

export const defaultSettings: Settings = {
  schemaVersion: 1,
  projectRoot: "C:\\Projects",
  exclusions: [".git", ".worktrees", "node_modules", "target"],
  preferences: { includeHidden: false, scanDepth: 3 },
};

const previewStorageKey = "navis-fabrica.settings.v1";

export const isDesktopApp = () => isTauri();

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
