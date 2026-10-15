import { FormEvent, useEffect, useMemo, useState } from "react";
import {
  isDesktopApp,
  loadSettings,
  ModelProfile,
  ProviderAvailability,
  AntigravityProbe,
  OllamaProbe,
  OpenRouterProbe,
  ProviderKind,
  ProviderProfile,
  ProviderSelection,
  saveSettings,
  Settings,
  inspectProviders,
  probeAntigravityCli,
  probeOllama,
  probeOpenRouter,
  saveOpenRouterCredential,
  listUsageSignals,
  recordManualUsageEstimate,
  UsageCategory,
  UsageSignal,
  evaluateRouting,
  listWorkflowRuns,
  WorkflowRun,
  RoutingCandidate,
  RoutingDecision,
  RoutingComplexity,
  RoutingPace,
} from "./settings";

type SaveState = "idle" | "saving" | "saved" | "error";

const executableDefaults: Record<ProviderKind, string> = {
  codex: "codex",
  antigravity: "agy",
  ollama: "aider",
  openRouter: "aider",
  custom: "",
};

const capabilityLabels: [keyof ProviderAvailability["capabilities"], string][] = [
  ["discovery", "Executable discovery"], ["health", "Health"], ["modelListing", "Model listing"],
  ["skillSupport", "Skills"],
  ["runStart", "Start runs"], ["runCancel", "Cancel runs"], ["eventStream", "Events"],
  ["approvalRequests", "Approval requests"], ["interactiveInput", "User input"], ["checkpoints", "Checkpoints"],
];

export function ProvidersPanel() {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [availability, setAvailability] = useState<ProviderAvailability[]>([]);
  const [probes, setProbes] = useState<Record<string, AntigravityProbe>>({});
  const [ollamaProbes, setOllamaProbes] = useState<Record<string, OllamaProbe>>({});
  const [openRouterProbes, setOpenRouterProbes] = useState<Record<string, OpenRouterProbe>>({});
  const [openRouterKeys, setOpenRouterKeys] = useState<Record<string, string>>({});
  const [probingId, setProbingId] = useState<string | null>(null);
  const [kind, setKind] = useState<ProviderKind>("codex");
  const [name, setName] = useState("Codex");
  const [executable, setExecutable] = useState("codex");
  const [endpoint, setEndpoint] = useState("http://127.0.0.1:11434");
  const [secretRef, setSecretRef] = useState("");
  const [modelId, setModelId] = useState("");
  const [modelName, setModelName] = useState("");
  const [efforts, setEfforts] = useState("low, medium, high");
  const [runtimeId, setRuntimeId] = useState("");
  const [saveState, setSaveState] = useState<SaveState>("idle");
  const [message, setMessage] = useState("");
  const [usageSignals, setUsageSignals] = useState<UsageSignal[]>([]);
  const [estimateCategory, setEstimateCategory] = useState<UsageCategory>("apiSpend");
  const [estimateValue, setEstimateValue] = useState("");
  const [estimateUnit, setEstimateUnit] = useState("USD");
  const [estimateWindowMinutes, setEstimateWindowMinutes] = useState("");
  const [estimateResetAt, setEstimateResetAt] = useState("");
  const [usageMessage, setUsageMessage] = useState("");
  const [savingEstimate, setSavingEstimate] = useState(false);
  const [workflowRuns, setWorkflowRuns] = useState<WorkflowRun[]>([]);
  const [routingComplexity, setRoutingComplexity] = useState<RoutingComplexity>("standard");
  const [routingPace, setRoutingPace] = useState<RoutingPace>("balanced");
  const [routingRequireTools, setRoutingRequireTools] = useState(true);
  const [routingLocalOnly, setRoutingLocalOnly] = useState(false);
  const [desiredMinutes, setDesiredMinutes] = useState("");
  const [manualRoute, setManualRoute] = useState("");
  const [routingDecision, setRoutingDecision] = useState<RoutingDecision | null>(null);
  const [routingBusy, setRoutingBusy] = useState(false);
  const [routingError, setRoutingError] = useState("");
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
      if (desktop) {
        try { setUsageSignals(await listUsageSignals()); }
        catch (error) { if (active) setUsageMessage(error instanceof Error ? error.message : String(error)); }
      }
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

  useEffect(() => {
    if (!desktop) return;
    let active = true;
    listWorkflowRuns().then((runs) => { if (active) setWorkflowRuns(runs); }).catch(() => undefined);
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

  async function probeAntigravity(providerId: string) {
    setProbingId(providerId);
    try {
      const result = await probeAntigravityCli(providerId);
      setProbes((current) => ({ ...current, [providerId]: result }));
    } catch (error) {
      setProbes((current) => ({ ...current, [providerId]: { version: null, state: "unavailable", models: [], note: error instanceof Error ? error.message : String(error) } }));
    } finally { setProbingId(null); }
  }

  async function probeLocalOllama(providerId: string) {
    setProbingId(providerId);
    try {
      const result = await probeOllama(providerId);
      setOllamaProbes((current) => ({ ...current, [providerId]: result }));
      if (result.state === "ready" && settings) {
        const profile = settings.providers.find((item) => item.id === providerId);
        if (profile?.kind === "ollama") {
          const eligible = result.models.filter((item) => item.eligible).map((item) => {
            const previous = profile.models.find((model) => model.id === item.id);
            return { ...previous, id: item.id, name: previous?.name ?? item.id, reasoningEfforts: previous?.reasoningEfforts ?? [], contextWindowTokens: item.contextTokens, supportsTools: item.toolsSupported, qualityRating: previous?.qualityRating ?? null };
          });
          const providers = settings.providers.map((item) => item.id === providerId ? { ...item, models: eligible } : item);
          const selection = settings.providerSelection.providerId === providerId && !eligible.some((item) => item.id === settings.providerSelection.modelId)
            ? { ...settings.providerSelection, modelId: eligible[0]?.id ?? null, reasoningEffort: null }
            : settings.providerSelection;
          await persist({ ...settings, providers, providerSelection: selection });
        }
      }
    } catch (error) {
      setOllamaProbes((current) => ({ ...current, [providerId]: { endpoint: "", state: "unavailable", models: [], availableMemoryBytes: null, note: error instanceof Error ? error.message : String(error) } }));
    } finally { setProbingId(null); }
  }

  async function saveOpenRouterKey(providerId: string) {
    const apiKey = openRouterKeys[providerId] ?? "";
    setProbingId(providerId);
    try {
      const saved = await saveOpenRouterCredential(providerId, apiKey);
      setSettings(saved);
      setOpenRouterKeys((current) => ({ ...current, [providerId]: "" }));
      setMessage("OpenRouter key saved to Windows Credential Manager.");
      setSaveState("saved");
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error));
      setSaveState("error");
    } finally { setProbingId(null); }
  }

  async function discoverOpenRouterModels(providerId: string) {
    setProbingId(providerId);
    try {
      const result = await probeOpenRouter(providerId);
      setOpenRouterProbes((current) => ({ ...current, [providerId]: result }));
      if (result.state === "ready" && settings) {
        const previousModels = settings.providers.find((item) => item.id === providerId)?.models ?? [];
        const models: ModelProfile[] = result.models.map((item) => {
          const previous = previousModels.find((model) => model.id === item.id);
          const supportsTools = item.supportedParameters === null ? previous?.supportsTools ?? null : item.supportedParameters.includes("tools") || item.supportedParameters.includes("tool_choice");
          return { ...previous, id: item.id, name: item.name, reasoningEfforts: previous?.reasoningEfforts ?? [], contextWindowTokens: item.contextLength ?? previous?.contextWindowTokens ?? null, supportsTools, qualityRating: previous?.qualityRating ?? null };
        });
        const providers = settings.providers.map((item) => item.id === providerId ? { ...item, models } : item);
        const selection = settings.providerSelection.providerId === providerId && !models.some((item) => item.id === settings.providerSelection.modelId)
          ? { ...settings.providerSelection, modelId: null, reasoningEffort: null }
          : settings.providerSelection;
        await persist({ ...settings, providers, providerSelection: selection });
      }
    } catch (error) {
      setOpenRouterProbes((current) => ({ ...current, [providerId]: { state: "unavailable", models: [], note: error instanceof Error ? error.message : "OpenRouter discovery failed." } }));
    } finally { setProbingId(null); }
  }

  async function saveEstimate(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const value = Number(estimateValue);
    const minutes = estimateWindowMinutes.trim() ? Number(estimateWindowMinutes) : null;
    const resetAtMs = estimateResetAt ? new Date(estimateResetAt).getTime() : null;
    if (!Number.isFinite(value) || value < 0 || (minutes !== null && (!Number.isFinite(minutes) || minutes <= 0)) || (resetAtMs !== null && !Number.isFinite(resetAtMs))) {
      setUsageMessage("Enter a valid value, optional positive window, and reset time.");
      return;
    }
    setSavingEstimate(true);
    setUsageMessage("");
    try {
      setUsageSignals(await recordManualUsageEstimate({
        category: estimateCategory,
        value,
        unit: estimateUnit.trim(),
        windowDurationSeconds: minutes === null ? null : Math.round(minutes * 60),
        resetAtMs,
      }));
      setUsageMessage("Manual estimate saved and labelled as estimated.");
    } catch (error) { setUsageMessage(error instanceof Error ? error.message : String(error)); }
    finally { setSavingEstimate(false); }
  }

  function addProvider(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!settings || !name.trim() || !executable.trim() || (!['ollama', 'openRouter'].includes(kind) && (!modelId.trim() || !modelName.trim()))) return;
    const models: ModelProfile[] = kind !== "openRouter" && modelId.trim() && modelName.trim() ? [{
      id: modelId.trim(), name: modelName.trim(),
      reasoningEfforts: efforts.split(",").map((effort) => effort.trim()).filter(Boolean),
    }] : [];
    const provider: ProviderProfile = {
      id: `${kind}-${crypto.randomUUID().slice(0, 8)}`,
      kind,
      name: name.trim(),
      executable: executable.trim(),
      endpoint: kind === "ollama" ? endpoint.trim() : null,
      secretRef: kind === "antigravity" || kind === "ollama" || kind === "openRouter" ? null : secretRef.trim() || null,
      models,
    };
    const providers = [...settings.providers, provider];
    const selectedRuntime = kind === "ollama" ? "aider-ollama" : kind === "openRouter" ? "aider-openrouter" : kind === "antigravity" ? "agy-cli" : kind === "codex" ? "codex-app-server" : runtimeId.trim() || null;
    void persist({ ...settings, providers, providerSelection: { providerId: provider.id, modelId: models[0]?.id ?? null, reasoningEffort: null, runtimeId: selectedRuntime } });
  }

  function updateSelection(next: ProviderSelection) {
    if (!settings) return;
    void persist({ ...settings, providerSelection: { ...next, runtimeId: runtimeId.trim() || null } });
  }

  function updateSelectedModelRouting(patch: Partial<Pick<ModelProfile, "contextWindowTokens" | "supportsTools" | "qualityRating">>) {
    if (!settings || !selectedProvider || !selectedModel) return;
    const providers = settings.providers.map((provider) => provider.id !== selectedProvider.id ? provider : {
      ...provider,
      models: provider.models.map((model) => model.id === selectedModel.id ? { ...model, ...patch } : model),
    });
    void persist({ ...settings, providers });
  }

  async function recommendModel() {
    if (!settings) return;
    setRoutingBusy(true);
    setRoutingError("");
    try {
      let latestRuns = workflowRuns;
      let latestUsageSignals = usageSignals;
      if (desktop) {
        [latestRuns, latestUsageSignals] = await Promise.all([listWorkflowRuns(), listUsageSignals()]);
      }
      setWorkflowRuns(latestRuns);
      setUsageSignals(latestUsageSignals);
      const candidates: RoutingCandidate[] = settings.providers.flatMap((provider) => provider.models.map((model) => {
        const ollamaModel = provider.kind === "ollama" ? ollamaProbes[provider.id]?.models.find((item) => item.id === model.id) : undefined;
        const openRouterModel = provider.kind === "openRouter" ? openRouterProbes[provider.id]?.models.find((item) => item.id === model.id) : undefined;
        const completedRun = latestRuns.filter((run) => run.state === "completed" && run.provider === provider.kind && run.modelId === model.id && run.latencyMs !== null && run.finishedAtMs !== null)
          .sort((a, b) => (b.finishedAtMs ?? 0) - (a.finishedAtMs ?? 0))[0];
        const discoveredTools = ollamaModel ? ollamaModel.toolsSupported : openRouterModel?.supportedParameters
          ? openRouterModel.supportedParameters.includes("tools") || openRouterModel.supportedParameters.includes("tool_choice")
          : null;
        return {
          providerId: provider.id, providerName: provider.name, modelId: model.id, modelName: model.name,
          isLocal: provider.kind === "ollama",
          contextTokens: ollamaModel?.contextTokens ?? openRouterModel?.contextLength ?? model.contextWindowTokens ?? null,
          supportsTools: discoveredTools ?? model.supportsTools ?? null,
          reasoningEfforts: model.reasoningEfforts,
          qualityRating: model.qualityRating ?? null,
          latencyMs: completedRun?.latencyMs ?? null,
          latencyCollectedAtMs: completedRun?.finishedAtMs ?? null,
        };
      }));
      const [manualProviderId, manualModelId] = manualRoute ? JSON.parse(manualRoute) as [string, string] : [null, null];
      const decision = await evaluateRouting({
        complexity: routingComplexity, pace: routingPace, requireTools: routingRequireTools, localOnly: routingLocalOnly,
        desiredCompletionMinutes: desiredMinutes.trim() ? Number(desiredMinutes) : null,
        manualProviderId, manualModelId, candidates, usageSignals: latestUsageSignals, nowMs: Date.now(),
      });
      setRoutingDecision(decision);
    } catch (error) {
      setRoutingError(error instanceof Error ? error.message : String(error));
    } finally { setRoutingBusy(false); }
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
              <option value="">{selectedProvider?.kind === "ollama" ? "Discover and choose an eligible model" : "Choose a manual model profile"}</option>{selectedProvider?.models.map((model) => <option key={model.id} value={model.id}>{model.name}</option>)}
            </select>
          </label>
          <label className="settings-field" htmlFor="reasoning-effort"><span>Reasoning effort</span>
            <select id="reasoning-effort" disabled={!selectedModel} value={settings.providerSelection.reasoningEffort ?? ""} onChange={(event) => updateSelection({ ...settings.providerSelection, reasoningEffort: event.target.value || null })}>
              <option value="">Use provider default</option>{selectedModel?.reasoningEfforts.map((effort) => <option key={effort} value={effort}>{effort}</option>)}
            </select>
          </label>
          <label className="settings-field" htmlFor="runtime-id"><span>Runtime ID</span>
            <input id="runtime-id" value={runtimeId} maxLength={160} placeholder="e.g. codex-cli" onChange={(event) => setRuntimeId(event.target.value)} onBlur={() => settings && void persist({ ...settings, providerSelection: { ...settings.providerSelection, runtimeId: runtimeId.trim() || null } })} />
            <small>Stored separately from the provider and model. Runtime IDs include codex-app-server, agy-cli, and aider-ollama.</small>
          </label>
        </div>
        <div className="provider-feedback" role={saveState === "error" ? "alert" : "status"} aria-live="polite">{message || (desktop ? "Configuration is local to this app." : "Browser preview stores configuration in this browser.")}</div>
        {selectedModel && <details className="model-routing-settings">
          <summary>Routing details for {selectedModel.name}</summary>
          <p className="registry-context-note">Set known model capabilities and your own quality estimate. Estimates are labelled and never treated as measured provider data.</p>
          <div className="provider-inline-fields">
            <label className="settings-field"><span>Context window (tokens)</span><input type="number" min="1" max="10000000" value={selectedModel.contextWindowTokens ?? ""} onChange={(event) => updateSelectedModelRouting({ contextWindowTokens: event.target.value ? Number(event.target.value) : null })} /></label>
            <label className="settings-field"><span>Model reports tool support</span><select value={selectedModel.supportsTools === true ? "yes" : selectedModel.supportsTools === false ? "no" : "unknown"} onChange={(event) => updateSelectedModelRouting({ supportsTools: event.target.value === "unknown" ? null : event.target.value === "yes" })}><option value="unknown">Unknown</option><option value="yes">Yes</option><option value="no">No</option></select></label>
            <label className="settings-field"><span>Your quality estimate (1–5)</span><select value={selectedModel.qualityRating ?? ""} onChange={(event) => updateSelectedModelRouting({ qualityRating: event.target.value ? Number(event.target.value) : null })}><option value="">Not rated</option>{[1, 2, 3, 4, 5].map((rating) => <option key={rating} value={rating}>{rating}/5</option>)}</select></label>
          </div>
        </details>}
      </section>

      <section className="panel settings-panel routing-panel" aria-labelledby="routing-policy-title">
        <div className="panel-heading"><div><h2 id="routing-policy-title">Explainable model recommendation</h2><p>Eligibility is checked before pace ranking. Manual selection applies only to an eligible model.</p></div><span className="setting-glyph" aria-hidden="true">⌁</span></div>
        <div className="provider-inline-fields">
          <label className="settings-field"><span>Task complexity</span><select value={routingComplexity} onChange={(event) => setRoutingComplexity(event.target.value as RoutingComplexity)}><option value="simple">Simple · 8k context floor</option><option value="standard">Standard · 16k context floor</option><option value="complex">Complex · 32k context floor</option></select></label>
          <label className="settings-field"><span>Pace preference</span><select value={routingPace} onChange={(event) => setRoutingPace(event.target.value as RoutingPace)}><option value="fast">Fast · recent latency first</option><option value="balanced">Balanced · quality, quota, latency</option><option value="thorough">Thorough · quality and reasoning</option><option value="economy">Economy · fresh spend and quota</option></select></label>
          <label className="settings-field"><span>Manual override</span><select value={manualRoute} onChange={(event) => setManualRoute(event.target.value)}><option value="">Automatic recommendation</option>{settings.providers.flatMap((provider) => provider.models.map((model) => <option key={`${provider.id}/${model.id}`} value={JSON.stringify([provider.id, model.id])}>{provider.name} · {model.name}</option>))}</select></label>
          <label className="settings-field"><span>Desired completion time (minutes)</span><input type="number" min="1" max="1440" value={desiredMinutes} onChange={(event) => setDesiredMinutes(event.target.value)} placeholder="Optional" /><small>A preference only; task completion is not guaranteed.</small></label>
          <label className="routing-checkbox"><input type="checkbox" checked={routingRequireTools} onChange={(event) => setRoutingRequireTools(event.target.checked)} /> Require known tool support</label>
          <label className="routing-checkbox"><input type="checkbox" checked={routingLocalOnly} onChange={(event) => setRoutingLocalOnly(event.target.checked)} /> Local-only privacy</label>
        </div>
        <div className="settings-actions"><button className="button button-primary" type="button" disabled={!desktop || routingBusy || !settings.providers.some((provider) => provider.models.length)} onClick={() => { void recommendModel(); }}>{routingBusy ? "Evaluating…" : "Recommend a model"}</button><p className="settings-feedback" role="status">{routingError}</p></div>
        {routingDecision && <div className="routing-results" aria-live="polite">
          <h3>{routingDecision.selectedProviderId ? `Selected: ${routingDecision.candidates.find((item) => item.selected)?.providerName} · ${routingDecision.candidates.find((item) => item.selected)?.modelName}` : "No eligible model"}</h3>
          <p>{routingDecision.requiredContextTokens.toLocaleString()} tokens required for this complexity. {routingDecision.desiredCompletionNote}</p>
          <ul className="provider-list">{routingDecision.candidates.map((candidate) => <li className={`provider-card routing-candidate${candidate.selected ? " selected" : ""}`} key={`${candidate.providerId}/${candidate.modelId}`}>
            <strong>{candidate.providerName} · {candidate.modelName}</strong><span>{candidate.selected ? "Selected" : candidate.eligible ? "Eligible alternative" : "Rejected"}</span>
            <ul>{candidate.reasons.map((reason, index) => <li key={`${index}-${reason}`}>{reason}</li>)}</ul>
          </li>)}</ul>
        </div>}
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
            <p className="provider-models">{provider.kind === "ollama" ? "Last discovered eligible models" : provider.kind === "openRouter" ? "Last discovered OpenRouter models" : "Manual models"}: {provider.models.map((model) => `${model.name} (${model.id})`).join(", ") || "none yet"}</p>
            {provider.kind === "ollama" && <small>Local endpoint: {provider.endpoint ?? "http://127.0.0.1:11434"}</small>}
            <div className="provider-status-grid">
              <span className={status?.executableFound ? "status-supported" : "status-unsupported"}>Executable: {status ? status.executableFound ? status.executablePath : "missing" : desktop ? "not checked" : "desktop check unavailable"}</span>
              <span className="status-unsupported">Authentication: {provider.kind === "ollama" ? "not used (local-only)" : provider.kind === "antigravity" ? probes[provider.id]?.state ?? "CLI sign-in not probed" : provider.kind === "openRouter" ? provider.secretRef ? openRouterProbes[provider.id]?.state ?? "reference configured; validity unknown" : "key missing" : status ? status.authenticationReference === "referenceConfigured" ? "reference configured; validity unknown" : "reference missing" : provider.secretRef ? "reference configured; validity unknown" : "reference missing"}</span>
            </div>
            <div className="provider-capabilities" aria-label="Provider adapter capabilities">{capabilityLabels.map(([key, label]) => {
              const support = status ? status.capabilities[key] ? "supported" : "unsupported" : desktop ? "not checked" : "desktop check unavailable";
              return <span key={key} className={status ? status.capabilities[key] ? "status-supported" : "status-unsupported" : ""}>{label}: {support}</span>;
            })}</div>
            {provider.kind === "antigravity" && <div className="settings-actions">
              <button className="button button-secondary" type="button" disabled={!desktop || probingId === provider.id} onClick={() => { void probeAntigravity(provider.id); }}>{probingId === provider.id ? "Probing…" : "Probe agy CLI"}</button>
              {probes[provider.id] && <div className="provider-feedback" role="status"><strong>{probes[provider.id].version ?? "Version unavailable"}</strong> · {probes[provider.id].state}: {probes[provider.id].note}{probes[provider.id].models.length > 0 && <p>Available model IDs: {probes[provider.id].models.join(", ")}</p>}</div>}
            </div>}
            {provider.kind === "ollama" && <div className="settings-actions">
              <button className="button button-secondary" type="button" disabled={!desktop || probingId === provider.id} onClick={() => { void probeLocalOllama(provider.id); }}>{probingId === provider.id ? "Checking local Ollama…" : "Discover local models"}</button>
              {ollamaProbes[provider.id] && <div className="provider-feedback" role="status"><strong>{ollamaProbes[provider.id].state}</strong> · {ollamaProbes[provider.id].note}{ollamaProbes[provider.id].availableMemoryBytes !== null && <p>Available system memory: {Math.round(ollamaProbes[provider.id].availableMemoryBytes! / 1_073_741_824)} GiB</p>}<ul>{ollamaProbes[provider.id].models.map((model) => <li key={model.id}>{model.id}: {model.eligible ? "eligible" : "unavailable"} · context {model.contextTokens ?? "unknown"} · tools {model.toolsSupported ? "supported" : "unknown"} · {model.note}</li>)}</ul></div>}
            </div>}
            {provider.kind === "openRouter" && <div className="provider-form">
              <label className="settings-field"><span>OpenRouter API key</span><input type="password" autoComplete="new-password" maxLength={4096} value={openRouterKeys[provider.id] ?? ""} onChange={(event) => setOpenRouterKeys((current) => ({ ...current, [provider.id]: event.target.value }))} placeholder={provider.secretRef ? "Enter a replacement key or leave blank" : "Paste key; saved directly to Windows Credential Manager"} /></label>
              <div className="settings-actions"><button className="button button-secondary" type="button" disabled={!desktop || !openRouterKeys[provider.id]?.trim() || probingId === provider.id} onClick={() => { void saveOpenRouterKey(provider.id); }}>Save key securely</button><button className="button button-secondary" type="button" disabled={!desktop || !provider.secretRef || probingId === provider.id} onClick={() => { void discoverOpenRouterModels(provider.id); }}>{probingId === provider.id ? "Checking OpenRouter…" : "Discover models"}</button></div>
              {openRouterProbes[provider.id] && <div className="provider-feedback" role="status"><strong>{openRouterProbes[provider.id].state}</strong> · {openRouterProbes[provider.id].note}<ul>{openRouterProbes[provider.id].models.slice(0, 100).map((model) => <li key={model.id}>{model.name} ({model.id}) · context {model.contextLength ?? "unknown"} · prompt price {model.promptPrice ?? "unknown"} · completion price {model.completionPrice ?? "unknown"} · input {model.inputModalities?.join(", ") ?? "unknown"} · output {model.outputModalities?.join(", ") ?? "unknown"} · parameters {model.supportedParameters?.join(", ") ?? "unknown"}</li>)}</ul></div>}
            </div>}
          </li>;
        })}</ul> : <div className="provider-empty">No provider profiles yet. Add one below to configure a manual choice.</div>}
      </section>

      <section className="panel settings-panel" aria-labelledby="add-provider-title">
        <div className="panel-heading"><div><h2 id="add-provider-title">Add a provider profile</h2><p>Provider and model choices remain explicit. OpenRouter discovery adds only models returned by its catalog.</p></div></div>
        <form className="provider-form" onSubmit={addProvider}>
          <label className="settings-field" htmlFor="provider-kind"><span>Provider type</span><select id="provider-kind" value={kind} onChange={(event) => {
            const nextKind = event.target.value as ProviderKind;
            setKind(nextKind);
            setName(nextKind === "custom" ? "Custom runtime" : nextKind[0].toUpperCase() + nextKind.slice(1));
            setExecutable(executableDefaults[nextKind]);
            setEfforts(nextKind === "antigravity" ? "" : "low, medium, high");
          }}><option value="codex">Codex</option><option value="antigravity">Antigravity</option><option value="ollama">Ollama (local only)</option><option value="openRouter">OpenRouter</option><option value="custom">Custom runtime</option></select></label>
          <label className="settings-field" htmlFor="provider-name"><span>Display name</span><input id="provider-name" maxLength={100} required value={name} onChange={(event) => setName(event.target.value)} /></label>
          <label className="settings-field" htmlFor="provider-executable"><span>{kind === "ollama" || kind === "openRouter" ? "Aider coding harness executable" : "Executable name or path"}</span><input id="provider-executable" maxLength={260} required value={executable} onChange={(event) => setExecutable(event.target.value)} /><small>{kind === "ollama" ? "Aider performs code edits; Ollama supplies local inference." : kind === "openRouter" ? "Aider provides file edits and optional test execution; OpenRouter provides model inference only." : "Presence checks do not execute it. Use “Probe agy CLI” on a saved Antigravity profile to check version and model sign-in status."}</small></label>
          {kind === "ollama" && <label className="settings-field" htmlFor="provider-endpoint"><span>Local Ollama endpoint</span><input id="provider-endpoint" maxLength={260} value={endpoint} onChange={(event) => setEndpoint(event.target.value)} /><small>Only numeric loopback addresses such as 127.0.0.1 or [::1] are accepted. Remote endpoints and credentials are rejected.</small></label>}
          {kind !== "ollama" && kind !== "openRouter" && <label className="settings-field" htmlFor="provider-secret-ref"><span>Authentication secret reference</span><input id="provider-secret-ref" maxLength={260} value={secretRef} onChange={(event) => setSecretRef(event.target.value)} placeholder={kind === "antigravity" ? "Not used by the agy adapter" : "credential-store:provider-name"} /><small>{kind === "antigravity" ? "Sign-in is managed by the installed agy CLI; do not enter a token here." : "Use credential-store:key or provider-managed:key. Never enter a token or password here."}</small></label>}
          <div className="provider-inline-fields">
            <label className="settings-field" htmlFor="model-id"><span>Model ID</span><input id="model-id" maxLength={160} required={kind !== "ollama" && kind !== "openRouter"} value={modelId} onChange={(event) => setModelId(event.target.value)} placeholder={kind === "ollama" ? "Optional; discovered models are added by probe" : kind === "openRouter" ? "Added by catalog discovery" : "provider model identifier"} /></label>
            <label className="settings-field" htmlFor="model-name"><span>Model display name</span><input id="model-name" maxLength={100} required={kind !== "ollama" && kind !== "openRouter"} value={modelName} onChange={(event) => setModelName(event.target.value)} placeholder={kind === "openRouter" ? "Added by catalog discovery" : ""} /></label>
          </div>
          <label className="settings-field" htmlFor="model-efforts"><span>{kind === "antigravity" ? "Reasoning effort labels (unsupported by installed CLI)" : "Supported reasoning effort labels"}</span><input id="model-efforts" value={efforts} disabled={kind === "antigravity"} onChange={(event) => setEfforts(event.target.value)} /><small>{kind === "antigravity" ? "The installed agy CLI help does not expose an effort flag." : "Comma-separated manual labels, for example low, medium, high."}</small></label>
          <div className="settings-actions"><p className={`settings-feedback ${saveState}`}>{saveState === "saving" ? "Saving…" : saveState === "error" ? message : "Codex and Antigravity runs and local Ollama coding runs are available from Runs. Ollama requires a discovered tool-capable model."}</p><button className="button button-primary" type="submit" disabled={saveState === "saving"}>Add provider</button></div>
        </form>
      </section>

      <section className="panel settings-panel" aria-labelledby="usage-signals-title">
        <div className="panel-heading"><div><h2 id="usage-signals-title">Usage and latency signals</h2><p>Subscription windows, API spend, context limits, and local resources are tracked separately.</p></div>
          <button className="button button-secondary" type="button" disabled={!desktop} onClick={() => { void listUsageSignals().then(setUsageSignals).catch((error: unknown) => setUsageMessage(error instanceof Error ? error.message : String(error))); }}>Refresh readings</button>
        </div>
        <p className="registry-context-note">Codex subscription windows are read from its app-server when a run starts. Context usage is recorded only when reported. This adapter does not expose API spend or local resource telemetry; missing values stay unknown.</p>
        <ul className="provider-list usage-signal-list">{usageSignals.map((signal) => <li className="provider-card" key={signal.id}>
          <div className="provider-card-heading"><div><strong>{signal.label}</strong><span>{signal.category} · {signal.metric}</span></div><span className={signal.status === "available" ? "status-supported" : "status-unsupported"}>{signal.status}{signal.confidence === "estimated" ? " · estimate" : ""}</span></div>
          <p className="provider-models">{signal.value === null ? "Unknown" : `${signal.value.toLocaleString()} ${signal.unit}`}{signal.windowDurationSeconds ? ` · window ${Math.round(signal.windowDurationSeconds / 60)} min` : ""}{signal.resetAtMs ? ` · resets ${new Date(signal.resetAtMs).toLocaleString()}` : ""}</p>
          <small>{signal.source} · collected {new Date(signal.collectedAtMs).toLocaleString()}{signal.providerId ? ` · ${signal.providerId}` : ""}{signal.modelId ? ` · ${signal.modelId}` : ""}</small>
          {signal.note && <small>{signal.note}</small>}
        </li>)}</ul>
        <form className="provider-form" onSubmit={(event) => { void saveEstimate(event); }}>
          <h3>Record a manual estimate</h3>
          <p className="registry-context-note">Manual entries are labelled estimates and remain separate from provider measurements.</p>
          <div className="provider-inline-fields">
            <label className="settings-field"><span>Category</span><select value={estimateCategory} onChange={(event) => setEstimateCategory(event.target.value as UsageCategory)}><option value="subscriptionWindow">Subscription window</option><option value="apiSpend">API spend</option><option value="contextLimit">Context limit</option><option value="localResource">Local resource</option></select></label>
            <label className="settings-field"><span>Value</span><input type="number" min="0" step="any" required value={estimateValue} onChange={(event) => setEstimateValue(event.target.value)} /></label>
            <label className="settings-field"><span>Unit</span><input required maxLength={40} value={estimateUnit} onChange={(event) => setEstimateUnit(event.target.value)} placeholder="USD, tokens, % used" /></label>
          </div>
          <div className="provider-inline-fields">
            <label className="settings-field"><span>Window length (minutes, optional)</span><input type="number" min="1" step="1" value={estimateWindowMinutes} onChange={(event) => setEstimateWindowMinutes(event.target.value)} /></label>
            <label className="settings-field"><span>Reset time (optional)</span><input type="datetime-local" value={estimateResetAt} onChange={(event) => setEstimateResetAt(event.target.value)} /></label>
          </div>
          <div className="settings-actions"><p className="settings-feedback" role="status">{usageMessage}</p><button className="button button-primary" type="submit" disabled={!desktop || savingEstimate}>{savingEstimate ? "Saving…" : "Save estimate"}</button></div>
        </form>
      </section>
    </div>
  );
}
