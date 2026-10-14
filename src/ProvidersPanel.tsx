import { FormEvent, useEffect, useMemo, useState } from "react";
import {
  isDesktopApp,
  loadSettings,
  ModelProfile,
  ProviderAvailability,
  ProviderKind,
  ProviderProfile,
  ProviderSelection,
  saveSettings,
  Settings,
  inspectProviders,
} from "./settings";

type SaveState = "idle" | "saving" | "saved" | "error";

const executableDefaults: Record<ProviderKind, string> = {
  codex: "codex",
  antigravity: "antigravity",
  ollama: "ollama",
  custom: "",
};

const capabilityLabels: [keyof ProviderAvailability["capabilities"], string][] = [
  ["discovery", "Executable discovery"], ["health", "Health"], ["modelListing", "Model listing"],
  ["runStart", "Start runs"], ["runCancel", "Cancel runs"], ["eventStream", "Events"],
  ["approvalRequests", "Approval requests"], ["interactiveInput", "User input"], ["checkpoints", "Checkpoints"],
];

export function ProvidersPanel() {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [availability, setAvailability] = useState<ProviderAvailability[]>([]);
  const [kind, setKind] = useState<ProviderKind>("codex");
  const [name, setName] = useState("Codex");
  const [executable, setExecutable] = useState("codex");
  const [secretRef, setSecretRef] = useState("");
  const [modelId, setModelId] = useState("");
  const [modelName, setModelName] = useState("");
  const [efforts, setEfforts] = useState("low, medium, high");
  const [runtimeId, setRuntimeId] = useState("");
  const [saveState, setSaveState] = useState<SaveState>("idle");
  const [message, setMessage] = useState("");
  const desktop = isDesktopApp();

  const selectedProvider = settings?.providers.find((provider) => provider.id === settings.providerSelection.providerId);
  const selectedModel = selectedProvider?.models.find((model) => model.id === settings?.providerSelection.modelId);
  const providerStatus = useMemo(() => new Map(availability.map((item) => [item.providerId, item])), [availability]);

  useEffect(() => {
    let active = true;
    loadSettings().then(async (value) => {
      if (!active) return;
      setSettings(value);
      setRuntimeId(value.providerSelection.runtimeId ?? "");
      if (desktop && value.providers.length) {
        try {
          const status = await inspectProviders(value.providers);
          if (active) setAvailability(status);
        } catch (error) {
          if (active) setMessage(error instanceof Error ? error.message : String(error));
        }
      }
    }).catch((error: unknown) => {
      if (active) {
        setSaveState("error");
        setMessage(error instanceof Error ? error.message : String(error));
      }
    });
    return () => { active = false; };
  }, [desktop]);

  async function persist(next: Settings) {
    setSettings(next);
    setSaveState("saving");
    setMessage("");
    try {
      const saved = await saveSettings({
        projectRoot: next.projectRoot,
        exclusions: next.exclusions,
        preferences: next.preferences,
        providers: next.providers,
        providerSelection: next.providerSelection,
        githubAccounts: next.githubAccounts,
        selectedGithubAccountId: next.selectedGithubAccountId,
      });
      setSettings(saved);
      if (desktop && saved.providers.length) setAvailability(await inspectProviders(saved.providers));
      else setAvailability([]);
      setSaveState("saved");
      setMessage("Provider settings saved. No provider process was started.");
    } catch (error) {
      setSaveState("error");
      setMessage(error instanceof Error ? error.message : String(error));
    }
  }

  function addProvider(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!settings || !name.trim() || !executable.trim() || !modelId.trim() || !modelName.trim()) return;
    const model: ModelProfile = {
      id: modelId.trim(),
      name: modelName.trim(),
      reasoningEfforts: efforts.split(",").map((effort) => effort.trim()).filter(Boolean),
    };
    const provider: ProviderProfile = {
      id: `${kind}-${crypto.randomUUID().slice(0, 8)}`,
      kind,
      name: name.trim(),
      executable: executable.trim(),
      secretRef: secretRef.trim() || null,
      models: [model],
    };
    const providers = [...settings.providers, provider];
    void persist({ ...settings, providers, providerSelection: { providerId: provider.id, modelId: model.id, reasoningEffort: null, runtimeId: runtimeId.trim() || null } });
  }

  function updateSelection(next: ProviderSelection) {
    if (!settings) return;
    void persist({ ...settings, providerSelection: { ...next, runtimeId: runtimeId.trim() || null } });
  }

  if (!settings) return <section className="panel settings-panel"><div className="settings-loading" role={saveState === "error" ? "alert" : "status"}>{saveState === "error" ? message : "Loading provider settings…"}</div></section>;

  return (
    <div className="provider-layout">
      <section className="panel settings-panel" aria-labelledby="provider-selection-title">
        <div className="panel-heading"><div><h2 id="provider-selection-title">Manual selection</h2><p>Provider, model, reasoning effort, and runtime stay independently configurable.</p></div><span className="setting-glyph" aria-hidden="true">◇</span></div>
        <div className="provider-form">
          <label className="settings-field" htmlFor="selected-provider"><span>Provider</span>
            <select id="selected-provider" value={settings.providerSelection.providerId ?? ""} onChange={(event) => {
              const profile = settings.providers.find((item) => item.id === event.target.value);
              updateSelection({ providerId: profile?.id ?? null, modelId: profile?.models[0]?.id ?? null, reasoningEffort: null, runtimeId: settings.providerSelection.runtimeId });
            }}>
              <option value="">Choose a configured provider</option>{settings.providers.map((provider) => <option key={provider.id} value={provider.id}>{provider.name}</option>)}
            </select>
          </label>
          <label className="settings-field" htmlFor="selected-model"><span>Model</span>
            <select id="selected-model" disabled={!selectedProvider} value={settings.providerSelection.modelId ?? ""} onChange={(event) => updateSelection({ ...settings.providerSelection, modelId: event.target.value || null, reasoningEffort: null })}>
              <option value="">Choose a manual model profile</option>{selectedProvider?.models.map((model) => <option key={model.id} value={model.id}>{model.name}</option>)}
            </select>
          </label>
          <label className="settings-field" htmlFor="reasoning-effort"><span>Reasoning effort</span>
            <select id="reasoning-effort" disabled={!selectedModel} value={settings.providerSelection.reasoningEffort ?? ""} onChange={(event) => updateSelection({ ...settings.providerSelection, reasoningEffort: event.target.value || null })}>
              <option value="">Use provider default</option>{selectedModel?.reasoningEfforts.map((effort) => <option key={effort} value={effort}>{effort}</option>)}
            </select>
          </label>
          <label className="settings-field" htmlFor="runtime-id"><span>Runtime ID</span>
            <input id="runtime-id" value={runtimeId} maxLength={160} placeholder="e.g. codex-cli" onChange={(event) => setRuntimeId(event.target.value)} onBlur={() => settings && void persist({ ...settings, providerSelection: { ...settings.providerSelection, runtimeId: runtimeId.trim() || null } })} />
            <small>Stored separately from the provider and model. Choose codex-app-server to run Codex from the Runs view.</small>
          </label>
        </div>
        <div className="provider-feedback" role={saveState === "error" ? "alert" : "status"} aria-live="polite">{message || (desktop ? "Configuration is local to this app." : "Browser preview stores configuration in this browser.")}</div>
      </section>

      <section className="panel settings-panel" aria-labelledby="provider-list-title">
        <div className="panel-heading"><div><h2 id="provider-list-title">Configured providers</h2><p>Executable checks inspect PATH and never launch or install a program. A secret reference stores a pointer, not a secret.</p></div></div>
        {settings.providers.length ? <ul className="provider-list">{settings.providers.map((provider) => {
          const status = providerStatus.get(provider.id);
          return <li className="provider-card" key={provider.id}>
            <div className="provider-card-heading"><div><strong>{provider.name}</strong><span>{provider.kind} · {provider.executable}</span></div>
              <button className="text-button" type="button" onClick={() => {
                const providers = settings.providers.filter((item) => item.id !== provider.id);
                const selection = settings.providerSelection.providerId === provider.id ? { providerId: null, modelId: null, reasoningEffort: null, runtimeId: settings.providerSelection.runtimeId } : settings.providerSelection;
                void persist({ ...settings, providers, providerSelection: selection });
              }}>Remove</button>
            </div>
            <p className="provider-models">Manual models: {provider.models.map((model) => `${model.name} (${model.id})`).join(", ")}</p>
            <div className="provider-status-grid">
              <span className={status?.executableFound ? "status-supported" : "status-unsupported"}>Executable: {status ? status.executableFound ? status.executablePath : "missing" : desktop ? "not checked" : "desktop check unavailable"}</span>
              <span className="status-unsupported">Authentication: {status ? status.authenticationReference === "referenceConfigured" ? "reference configured; validity unknown" : "reference missing" : provider.secretRef ? "reference configured; validity unknown" : "reference missing"}</span>
            </div>
            <div className="provider-capabilities" aria-label="Provider adapter capabilities">{capabilityLabels.map(([key, label]) => {
              const support = status ? status.capabilities[key] ? "supported" : "unsupported" : desktop ? "not checked" : "desktop check unavailable";
              return <span key={key} className={status ? status.capabilities[key] ? "status-supported" : "status-unsupported" : ""}>{label}: {support}</span>;
            })}</div>
          </li>;
        })}</ul> : <div className="provider-empty">No provider profiles yet. Add one below to configure a manual choice.</div>}
      </section>

      <section className="panel settings-panel" aria-labelledby="add-provider-title">
        <div className="panel-heading"><div><h2 id="add-provider-title">Add a provider profile</h2><p>Model names and reasoning options are entered manually; no catalog or model download is requested.</p></div></div>
        <form className="provider-form" onSubmit={addProvider}>
          <label className="settings-field" htmlFor="provider-kind"><span>Provider type</span><select id="provider-kind" value={kind} onChange={(event) => {
            const nextKind = event.target.value as ProviderKind;
            setKind(nextKind);
            setName(nextKind === "custom" ? "Custom runtime" : nextKind[0].toUpperCase() + nextKind.slice(1));
            setExecutable(executableDefaults[nextKind]);
          }}><option value="codex">Codex</option><option value="antigravity">Antigravity</option><option value="ollama">Ollama</option><option value="custom">Custom runtime</option></select></label>
          <label className="settings-field" htmlFor="provider-name"><span>Display name</span><input id="provider-name" maxLength={100} required value={name} onChange={(event) => setName(event.target.value)} /></label>
          <label className="settings-field" htmlFor="provider-executable"><span>Executable name or path</span><input id="provider-executable" maxLength={260} required value={executable} onChange={(event) => setExecutable(event.target.value)} /><small>Checked for presence only. Nothing is executed.</small></label>
          <label className="settings-field" htmlFor="provider-secret-ref"><span>Authentication secret reference</span><input id="provider-secret-ref" maxLength={260} value={secretRef} onChange={(event) => setSecretRef(event.target.value)} placeholder="credential-store:provider-name" /><small>Use credential-store:key or provider-managed:key. Never enter a token or password here.</small></label>
          <div className="provider-inline-fields">
            <label className="settings-field" htmlFor="model-id"><span>Model ID</span><input id="model-id" maxLength={160} required value={modelId} onChange={(event) => setModelId(event.target.value)} placeholder="provider model identifier" /></label>
            <label className="settings-field" htmlFor="model-name"><span>Model display name</span><input id="model-name" maxLength={100} required value={modelName} onChange={(event) => setModelName(event.target.value)} /></label>
          </div>
          <label className="settings-field" htmlFor="model-efforts"><span>Supported reasoning effort labels</span><input id="model-efforts" value={efforts} onChange={(event) => setEfforts(event.target.value)} /><small>Comma-separated manual labels, for example low, medium, high.</small></label>
          <div className="settings-actions"><p className={`settings-feedback ${saveState}`}>{saveState === "saving" ? "Saving…" : saveState === "error" ? message : "Codex runs are available from the Runs view; other provider operations remain unsupported."}</p><button className="button button-primary" type="submit" disabled={saveState === "saving"}>Add provider</button></div>
        </form>
      </section>
    </div>
  );
}
