import { FormEvent, useEffect, useState } from "react";
import { isDesktopApp, loadSettings, saveSettings, Settings } from "./settings";

type SaveState = "idle" | "saving" | "saved" | "error";

export function SettingsPanel() {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [saveState, setSaveState] = useState<SaveState>("idle");
  const [message, setMessage] = useState("");
  const desktop = isDesktopApp();
  const saving = saveState === "saving";

  function updateSettings(next: Settings) {
    setSettings(next);
    setSaveState("idle");
    setMessage("Unsaved changes.");
  }

  useEffect(() => {
    let active = true;
    loadSettings().then((value) => {
      if (active) setSettings(value);
    }).catch((error: unknown) => {
      if (active) {
        setMessage(error instanceof Error ? error.message : String(error));
        setSaveState("error");
      }
    });
    return () => { active = false; };
  }, []);

  async function handleSave(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!settings) return;
    setSaveState("saving");
    setMessage("");
    try {
      const saved = await saveSettings({
        projectRoot: settings.projectRoot,
        exclusions: settings.exclusions,
        preferences: settings.preferences,
        providers: settings.providers,
        providerSelection: settings.providerSelection,
      });
      setSettings(saved);
      setSaveState("saved");
      setMessage("Your workspace settings are saved.");
    } catch (error) {
      setSaveState("error");
      setMessage(error instanceof Error ? error.message : String(error));
    }
  }

  if (!settings) {
    return (
      <section className="panel settings-panel" aria-labelledby="root-setting-title">
        <div className="settings-loading" role={saveState === "error" ? "alert" : "status"}>
          {saveState === "error" ? <>{message}<button className="text-button" type="button" onClick={() => {
            setSaveState("idle");
            setMessage("");
            loadSettings().then(setSettings).catch((error: unknown) => {
              setSaveState("error");
              setMessage(error instanceof Error ? error.message : String(error));
            });
          }}>Try again</button></> : "Loading your workspace settings…"}
        </div>
      </section>
    );
  }

  return (
    <section className="panel settings-panel" aria-labelledby="root-setting-title">
      <div className="panel-heading">
        <div><h2 id="root-setting-title">Workspace settings</h2><p>Choose where Navis-Fabrica looks and what it skips.</p></div>
        <span className="setting-glyph" aria-hidden="true">⌂</span>
      </div>
      <form className="settings-form" onSubmit={handleSave}>
        <label className="settings-field" htmlFor="project-root">
          <span>Projects folder</span>
          <input id="project-root" autoComplete="off" spellCheck={false} disabled={saving} value={settings.projectRoot} onChange={(event) => updateSettings({ ...settings, projectRoot: event.target.value })} />
          <small>Use an absolute folder path. You can choose a folder that will be created later.</small>
        </label>

        <label className="settings-field" htmlFor="project-exclusions">
          <span>Folders to skip</span>
          <textarea id="project-exclusions" rows={4} spellCheck={false} disabled={saving} value={settings.exclusions.join("\n")} onChange={(event) => updateSettings({ ...settings, exclusions: event.target.value.split(/\r?\n/) })} />
          <small>One relative folder name or path per line.</small>
        </label>

        <div className="settings-options">
          <label className="settings-toggle" htmlFor="include-hidden">
            <input id="include-hidden" type="checkbox" disabled={saving} checked={settings.preferences.includeHidden} onChange={(event) => updateSettings({ ...settings, preferences: { ...settings.preferences, includeHidden: event.target.checked } })} />
            <span><strong>Include hidden folders</strong><small>Show folders that start with a dot.</small></span>
          </label>
          <label className="settings-field settings-depth" htmlFor="scan-depth">
            <span>Search depth</span>
            <select id="scan-depth" disabled={saving} value={settings.preferences.scanDepth} onChange={(event) => updateSettings({ ...settings, preferences: { ...settings.preferences, scanDepth: Number(event.target.value) } })}>
              {[1, 2, 3, 4, 5, 6, 7, 8].map((depth) => <option key={depth} value={depth}>{depth} {depth === 1 ? "level" : "levels"}</option>)}
            </select>
          </label>
        </div>

        <div className="settings-actions">
          <p className={`settings-feedback ${saveState}`} role={saveState === "error" ? "alert" : "status"} aria-live="polite">
            {message || (desktop ? "Settings are ready to edit." : "Browser preview saves changes in this browser.")}
          </p>
          <button className="button button-primary" type="submit" disabled={saving}>
            {saveState === "saving" ? "Saving…" : "Save settings"}
          </button>
        </div>
      </form>
    </section>
  );
}
