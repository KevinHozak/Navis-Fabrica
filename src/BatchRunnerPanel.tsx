import { publishBatchActivity } from "./Activity";
import { useEffect, useRef, useState } from "react";
import { isDesktopApp } from "./settings";
import { RunnerCommandError, batchCommand, refreshProviderObservations, runnerContext, runnerProviders, type BatchEvent, type BatchPreview, type BatchRequest, type BatchStatus, type ProviderChoices, type RecoveryRequest } from "./batchClient";

const connectionKey = "fabrica.runner.connection.v1";
function savedConnection(): { dataRoot: string; batchId: string; projectPath?: string } {
  try { const value = JSON.parse(localStorage.getItem(connectionKey) ?? "{}"); return { projectPath: typeof value.projectPath === "string" ? value.projectPath : undefined, dataRoot: typeof value.dataRoot === "string" ? value.dataRoot : "", batchId: typeof value.batchId === "string" ? value.batchId : "" }; } catch { return { dataRoot: "", batchId: "" }; }
}
const money = (micros: number) => `$${(micros / 1000000).toFixed(4)}`;
export function BatchRunnerPanel() {
  const [saved] = useState(savedConnection);
  const [dataRoot, setDataRoot] = useState(saved.dataRoot);
  const [batchId, setBatchId] = useState(saved.batchId);
  const [planPath, setPlanPath] = useState("");
  const [draft, setDraft] = useState<BatchRequest | null>(null);
  const [preview, setPreview] = useState<BatchPreview | null>(null);
  const [providers, setProviders] = useState<ProviderChoices["providers"]>([]);
  const [status, setStatus] = useState<BatchStatus | null>(null);
  const [events, setEvents] = useState<BatchEvent[]>([]);
  const [connected, setConnected] = useState(Boolean(saved.batchId && saved.dataRoot));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [lastRefreshed, setLastRefreshed] = useState<number | null>(null);
  const [pollError, setPollError] = useState("");
  const [recovery, setRecovery] = useState<RecoveryRequest | null>(null);
  const [allowLiveProbe, setAllowLiveProbe] = useState(false);
  const [dispatchRefreshKinds, setDispatchRefreshKinds] = useState<("codex" | "ollama" | "openRouter")[]>([]);
  const cursor = useRef(0);
  const scope = useRef({ dataRoot, batchId });
  scope.current = { dataRoot, batchId };
  const desktop = isDesktopApp();
  const invalidAssignments = preview?.plan.issues?.some(issue => issue.tasks?.some(task => !task.eligible)) ?? false;
  useEffect(() => {
    if (!desktop || saved.dataRoot) return;
    let active = true;
    runnerContext().then(context => { if (active) setDataRoot(previous => previous || context.dataRoot); }).catch(reason => { if (active) setError(String(reason)); });
    return () => { active = false; };
  }, [desktop, saved.dataRoot]);
  useEffect(() => {
    if (!desktop || !connected || !batchId || !dataRoot) return;
    try { localStorage.setItem(connectionKey, JSON.stringify({ dataRoot, batchId })); } catch { /* Connection remains usable without browser persistence. */ }
    let active = true, inFlight = false;
    async function refresh() {
      if (inFlight) return;
      inFlight = true;
      try {
        const current = await batchCommand("status", { dataRoot, batchId });
        if (!active || scope.current.dataRoot !== dataRoot || scope.current.batchId !== batchId) return;
        if (current.kind !== "status") throw new Error("Unexpected status response");
        setStatus(current.value); setLastRefreshed(Date.now()); setPollError("");
        const page = await batchCommand("events", { dataRoot, batchId, cursor: cursor.current, limit: 100 });
        if (!active || scope.current.dataRoot !== dataRoot || scope.current.batchId !== batchId) return;
        if (page.kind !== "events") throw new Error("Unexpected event response");
        cursor.current = page.value.nextCursor;
        const additions = page.value.events;
        setEvents(previous => [...new Map([...previous, ...additions].map(event => [event.sequence, event])).values()].slice(-1000));
      } catch (reason) { if (active) setPollError(String(reason)); } finally { inFlight = false; }
    }
    void refresh();
    const timer = window.setInterval(() => void refresh(), 3000);
    return () => { active = false; window.clearInterval(timer); };
  }, [desktop, connected, batchId, dataRoot]);
  useEffect(() => {
    if (!connected || !status) { publishBatchActivity(null); return; }
    const projectPath = draft?.batchId === batchId ? draft.projectPath : saved.batchId === batchId ? saved.projectPath : undefined;
    publishBatchActivity({ batchId, projectPath, state: pollError ? "unavailable" : status.snapshot.state,
      needsAttention: Boolean(pollError) || status.jobs.some(job => ["needs_input", "needsInput", "failed", "blocked"].includes(job.state)) });
    if (projectPath) {
      try { localStorage.setItem(connectionKey, JSON.stringify({ dataRoot, batchId, projectPath })); } catch { /* Optional browser persistence. */ }
    }
  }, [connected, status, pollError, batchId, dataRoot, draft, saved]);

  function changeScope(nextRoot: string, nextId: string) {
    if (nextRoot !== dataRoot) setPreview(null);
    setDataRoot(nextRoot); setBatchId(nextId); setConnected(false); setStatus(null); setEvents([]); setLastRefreshed(null); setPollError(""); setRecovery(null); cursor.current = 0;
  }
  async function reviewPlan(useDraft: boolean) {
    setBusy(true); setError(""); setNotice("");
    try {
      const request = useDraft && draft ? structuredClone(draft) : null;
      // These values are derived from the previous evidence snapshot. A fresh
      // preview must derive them again; keeping either value makes the core
      // correctly reject the stale snapshot binding.
      if (request) {
        delete request.providerObservationFingerprint;
        delete request.resolvedRoutes;
        delete request.routeCommitments;
      }
      const outcome = await batchCommand("batch-preview", { dataRoot, ...(request ? { batchRequest: request } : { batchRequestPath: planPath }) });
      if (outcome.kind !== "preview") throw new Error("Unexpected preview response");
      setPreview(outcome.value); setDraft(structuredClone(outcome.value.request));
      const consent = outcome.value.request.dispatchRefreshConsent;
      setDispatchRefreshKinds(consent?.mode === "providers" ? consent.kinds : []);
      const choices = await runnerProviders(outcome.value.request.settingsPath);
      setProviders(choices.providers);
      setNotice("Preview ready. No jobs or provider calls have started.");
    } catch (reason) { setPreview(null); setError(String(reason)); } finally { setBusy(false); }
  }
  async function refreshRoutingEvidence() {
    if (!draft) return;
    setBusy(true); setError(""); setNotice("");
    try {
      const snapshot = await refreshProviderObservations(dataRoot, draft.settingsPath, allowLiveProbe);
      const knownModels = snapshot.providers.reduce((count, provider) => count + provider.models.filter(model => model.modelAvailable.state === "known" && model.modelAvailable.value === true).length, 0);
      const refreshedDraft = structuredClone(draft);
      delete refreshedDraft.providerObservationFingerprint;
      delete refreshedDraft.resolvedRoutes;
      delete refreshedDraft.routeCommitments;
      setDraft(refreshedDraft);
      setPreview(null);
      setNotice(`Provider evidence refreshed (${snapshot.providers.length} configured providers, ${knownModels} confirmed available models). Review the plan again to use this snapshot. This did not run model inference.`);
    } catch (reason) { setError(String(reason)); } finally { setBusy(false); }
  }
  function setDispatchRefreshKind(kind: "codex" | "ollama" | "openRouter", enabled: boolean) {
    if (!draft) return;
    const kinds = enabled
      ? [...new Set([...dispatchRefreshKinds, kind])]
      : dispatchRefreshKinds.filter(candidate => candidate !== kind);
    setDispatchRefreshKinds(kinds);
    const updated = structuredClone(draft);
    updated.dispatchRefreshConsent = kinds.length ? { mode: "providers", kinds } : { mode: "disabled" };
    setDraft(updated);
    setPreview(null);
    setNotice("Dispatch refresh consent changed. Preview the plan again before starting.");
  }
  function assign(issueId: string, taskId: string, providerId: string, modelId: string) {
    if (!draft) return;
    const updated = structuredClone(draft);
    const task = updated.jobs.find(job => job.issue_id === issueId)?.plan.tasks.find(item => item.task_id === taskId);
    if (!task) return;
    task.provider_id = providerId || null; task.model_id = modelId || null;
    for (const policy of updated.verification) if (policy.issueId === issueId && policy.reviewer.task_id === taskId) { policy.reviewer.provider_id = providerId; policy.reviewer.model_id = modelId; const efforts = providers.find(provider => provider.id === providerId)?.models.find(model => model.id === modelId)?.reasoningEfforts ?? []; if (policy.reviewer.reasoning_effort && !efforts.includes(policy.reviewer.reasoning_effort)) policy.reviewer.reasoning_effort = null; }
    setDraft(updated); setPreview(null); setNotice("Assignments changed. Preview again before starting.");
  }
  function assignReviewerEffort(issueId: string, effort: string) {
    if (!draft) return;
    const updated = structuredClone(draft);
    const policy = updated.verification.find(item => item.issueId === issueId);
    if (!policy) return;
    policy.reviewer.reasoning_effort = effort || null;
    setDraft(updated); setPreview(null); setNotice("Reviewer effort changed. Preview again before starting.");
  }
  async function start() {
    if (!preview) return;
    setBusy(true); setError("");
    try {
      const outcome = await batchCommand("submit", { dataRoot, batchRequest: preview.request, previewFingerprint: preview.fingerprint });
      if (outcome.kind !== "submitted") throw new Error("Unexpected submit response");
      changeScope(dataRoot, outcome.value.batchId); setConnected(true); setPreview(null); setNotice("Batch submitted. Workers continue when you close this view.");
    } catch (reason) {
      setError(String(reason));
      // Only this typed error proves the reviewed request was admitted.
      // A reused batch-ID conflict must never attach another request's controls.
      if (!(reason instanceof RunnerCommandError) || reason.code !== "WORKER_LAUNCH_FAILED") return;
      try {
        const current = await batchCommand("status", { dataRoot, batchId: preview.request.batchId });
        if (current.kind === "status") {
          changeScope(dataRoot, preview.request.batchId);
          setStatus(current.value); setConnected(true); setPreview(null);
          setNotice("Submission did not finish. Persisted batch state is shown; inspect it before resuming.");
        }
      } catch { /* Keep the original submission error when nothing was persisted. */ }
    } finally { setBusy(false); }
  }
  async function control(operation: "pause" | "resume" | "stop" | "cancel" | "reconcile") {
    setBusy(true); setError("");
    try {
      const outcome = await batchCommand(operation, { dataRoot, batchId });
      setNotice(outcome.kind === "reconciled" ? "Reconciliation inspected owned processes. Ambiguous work requires explicit recovery; no job was retried." : `${operation} requested.`);
      const current = await batchCommand("status", { dataRoot, batchId });
      if (current.kind === "status") setStatus(current.value);
    } catch (reason) { setError(String(reason)); } finally { setBusy(false); }
  }
  async function resolveRecovery() {
    if (!recovery) return;
    setBusy(true); setError("");
    try {
      const outcome = await batchCommand("recover", { dataRoot, batchId, recovery });
      if (outcome.kind !== "recovery") throw new Error("Unexpected recovery response");
      setRecovery(null); setNotice("Recovery decision recorded. Resume / reconnect workers explicitly when ready; this decision does not certify completion.");
      const current = await batchCommand("status", { dataRoot, batchId });
      if (current.kind === "status") setStatus(current.value);
    } catch (reason) { setError(String(reason)); } finally { setBusy(false); }
  }
  function controlAllowed(operation: string) {
    const current = status?.snapshot.state ?? "";
    if (operation === "reconcile") return true;
    if (operation === "pause") return ["running", "starting"].includes(current);
    if (operation === "resume") return ["paused", "waiting", "starting", "running"].includes(current);
    if (operation === "stop") return ["running", "starting", "paused"].includes(current);
    return ["running", "starting", "paused", "stopping", "waiting"].includes(current);
  }
  if (!desktop) return <section className="panel"><h2>Runner batches</h2><p>Open Fabrica desktop to preview or control local runner batches.</p></section>;
  return <section className="panel runner-batches" aria-labelledby="runner-batches-title"><h2 id="runner-batches-title">Runner batches</h2><p>Review a local plan prepared by Codex, then start a bounded batch. Use the same state folder to reconnect from Codex or Fabrica.</p>
    <div className="local-issues-paths"><label>Runner state folder<input value={dataRoot} disabled={busy} onChange={event => changeScope(event.target.value, batchId)} /></label><label>Batch ID<input value={batchId} disabled={busy} onChange={event => changeScope(dataRoot, event.target.value)} /></label><button className="button button-secondary" disabled={busy || !dataRoot || !batchId} onClick={() => { setError(""); setConnected(false); window.setTimeout(() => setConnected(true), 0); }}>Reconnect</button></div>
    <div className="local-issues-paths"><label>Local plan file<input value={planPath} disabled={busy} onChange={event => { setPlanPath(event.target.value); setPreview(null); setDraft(null); }} /></label><button className="button button-secondary" disabled={busy || !dataRoot || !planPath} onClick={() => void reviewPlan(false)}>Open plan preview</button></div>
    {error && <p role="alert" className="scan-notice">{error}</p>}{notice && <p role="status">{notice}</p>}{pollError && <p role="alert" className="scan-notice">Live refresh failed; the displayed snapshot may be stale. {pollError}</p>}
    {draft && <div className="runner-plan"><h3>{draft.batchId}</h3><p>{draft.projectPath}</p><p>{draft.jobs.length} issues · up to {draft.policy.maxConcurrency} workers · {draft.policy.windowSeconds} second window · API ceiling {money(draft.policy.budget.maxCostMicros)} · {draft.policy.budget.maxTokens.toLocaleString()} token ceiling</p><p>Permitted effects: {draft.policy.permittedEffects.join(", ")}</p>
      <section aria-label="Provider routing evidence"><h4>Routing evidence</h4><p>Preview reads saved evidence only. The optional dispatch refresh below checks only the selected provider/model before implementation or review; it never runs inference or changes the saved snapshot. If disabled, any evidence change blocks dispatch.</p><label className="runner-recovery-confirm"><input type="checkbox" disabled={busy} checked={dispatchRefreshKinds.includes("codex")} onChange={event => setDispatchRefreshKind("codex", event.target.checked)} />Allow bounded Codex app-server account/model metadata refresh before dispatch (no API inference).</label><label className="runner-recovery-confirm"><input type="checkbox" disabled={busy} checked={dispatchRefreshKinds.includes("ollama")} onChange={event => setDispatchRefreshKind("ollama", event.target.checked)} />Allow bounded local Ollama metadata refresh before dispatch.</label><label className="runner-recovery-confirm"><input type="checkbox" disabled={busy} checked={dispatchRefreshKinds.includes("openRouter")} onChange={event => setDispatchRefreshKind("openRouter", event.target.checked)} />Allow OpenRouter authentication and user-catalog refresh before dispatch; this sends a network request using configured credentials, with no inference.</label><p>Saved-snapshot refresh consent is separate: the explicit refresh button may probe the OpenRouter catalog only when its network checkbox is selected.</p><label className="runner-recovery-confirm"><input type="checkbox" disabled={busy} checked={allowLiveProbe} onChange={event => setAllowLiveProbe(event.target.checked)} />Also allow OpenRouter network access for saved-evidence refresh.</label><div className="local-issues-actions"><button className="button button-secondary" disabled={busy || !dataRoot || !draft.settingsPath} onClick={() => void refreshRoutingEvidence()}>Refresh provider evidence</button><button className="button button-secondary" disabled={busy} onClick={() => void reviewPlan(true)}>Review plan with current evidence</button></div></section>
      {preview?.plan.issues?.map(issue => <div key={`routing-${issue.issue_id}`}><h4>{issue.issue_id} · {issue.eligibility ?? "eligibility unavailable"}</h4>{issue.routing_reasons?.map((reason, index) => <p key={index}>{reason}</p>)}{issue.tasks?.map(task => <div key={task.task_id}><strong>{task.task_id}: {task.eligible ? "eligible" : "blocked"}</strong><p>{task.provider_id ?? "unassigned"} / {task.model_id ?? "unassigned"} · {task.assignment_source}</p><ul>{task.reasons.map((reason, index) => <li key={index}>{reason}</li>)}</ul></div>)}</div>)}{draft.jobs.map(job => <details key={job.issue_id}><summary>{job.issue_id} · {job.plan.tasks.length} tasks · {job.dependencies.length} dependencies</summary><ul>{job.dependencies.map(dep => <li key={dep.issue_id}>{dep.issue_id}: {dep.resolution.kind}</li>)}</ul>{job.plan.tasks.map(task => { const selectedProvider = providers.find(provider => provider.id === task.provider_id); const reviewer = draft.verification.find(policy => policy.issueId === job.issue_id && policy.reviewer.task_id === task.task_id)?.reviewer; const efforts = selectedProvider?.models.find(model => model.id === task.model_id)?.reasoningEfforts ?? []; return <div className="runner-task" key={task.task_id}><strong>{task.task_id} · {task.kind}</strong><p>Files: {task.files.join(", ")}</p><ul>{task.acceptance.map((criterion, index) => <li key={index}>{criterion}</li>)}</ul><div className="local-issues-filters"><label>Provider for {task.task_id}<select disabled={busy} value={task.provider_id ?? ""} onChange={event => assign(job.issue_id, task.task_id, event.target.value, providers.find(provider => provider.id === event.target.value)?.models[0]?.id ?? "")}><option value="">Configured selection</option>{providers.map(provider => <option key={provider.id} value={provider.id}>{provider.name}</option>)}</select></label><label>Model for {task.task_id}<select disabled={busy} value={task.model_id ?? ""} onChange={event => assign(job.issue_id, task.task_id, task.provider_id ?? "", event.target.value)}><option value="">Configured selection</option>{selectedProvider?.models.map(model => <option key={model.id} value={model.id}>{model.name}</option>)}</select></label>{reviewer && <label>Reviewer effort for {task.task_id}<select disabled={busy} value={reviewer.reasoning_effort ?? ""} onChange={event => assignReviewerEffort(job.issue_id, event.target.value)}><option value="">Model default</option>{reviewer.reasoning_effort && !efforts.includes(reviewer.reasoning_effort) && <option value={reviewer.reasoning_effort}>{reviewer.reasoning_effort} (not advertised)</option>}{efforts.map(effort => <option key={effort} value={effort}>{effort}</option>)}</select></label>}</div><p>Task ceiling: {task.limits.max_tokens ?? "unknown"} tokens · {task.limits.max_seconds ?? "unknown"} seconds · {task.limits.max_attempts} attempts · API ceiling {task.limits.max_cost_micros == null ? "unspecified" : money(task.limits.max_cost_micros)}</p></div>; })}<p>Checks: {draft.verification.find(policy => policy.issueId === job.issue_id)?.checks.map(check => check.id).join(", ") ?? "none"}</p></details>)}
      {preview?.plan.warnings.map((warning, index) => <p key={index} className="scan-notice">{warning}</p>)}{invalidAssignments && <p role="status" className="scan-notice">Resolve blocked task assignments and preview again before starting.</p>}<div className="local-issues-actions"><button className="button button-secondary" disabled={busy} onClick={() => void reviewPlan(true)}>Preview assignments again</button><button className="button button-primary" disabled={busy || !preview || !dataRoot || invalidAssignments} onClick={() => void start()}>Start {draft.jobs.length} issues</button></div></div>}
    {status && <div><h3>{status.snapshot.batchId} · {status.snapshot.state}</h3>{lastRefreshed && <p>Snapshot read at {new Date(lastRefreshed).toLocaleTimeString()}.</p>}<p>API cost reported: {money(status.budget.costUsedMicros)} / {money(status.budget.maxCostMicros)} · {status.budget.unresolved} unresolved usage records (budget remains reserved)</p><p>Tokens reported: {status.budget.tokensUsed.toLocaleString()} / {status.budget.maxTokens.toLocaleString()}</p>{status.snapshot.controlNote && <p>{status.snapshot.controlNote}</p>}<div className="local-issues-actions">{(["pause", "resume", "stop", "cancel", "reconcile"] as const).map(operation => <button key={operation} className="button button-secondary" disabled={busy || !controlAllowed(operation)} onClick={() => void control(operation)}>{operation === "stop" ? "Stop after current work" : operation === "cancel" ? "Cancel active work" : operation === "resume" ? "Resume / reconnect workers" : operation[0].toUpperCase() + operation.slice(1)}</button>)}</div><table><thead><tr><th>Issue</th><th>State</th><th>Attempts</th><th>Completion gate</th><th>Recovery</th></tr></thead><tbody>{status.jobs.map(job => <tr key={job.id}><td>{job.issueId}</td><td>{job.state}</td><td>{job.attemptCount} / {job.maxAttempts}</td><td>{job.completionGate}{["needs_input", "reconciliation_required"].includes(job.state) && job.lastAttemptSummary && <p role="status" aria-label={`Reason this job needs attention: ${job.issueId}. ${job.lastAttemptSummary}`}>{job.lastAttemptSummary}</p>}{job.checkpoints && <details><summary>Checks and checkpoints</summary><pre>{JSON.stringify(job.checkpoints, null, 2)}</pre>{job.checkpointsTruncated && <p>Some checkpoint details are omitted from this bounded view. Full evidence remains in local runner state.</p>}</details>}</td><td>{["needs_input", "reconciliation_required"].includes(job.state) && <button className="button button-secondary" disabled={busy} onClick={() => setRecovery({jobId: job.id, decision: "keep_needs_input", evidence: "", processConfirmedStopped: false, effectsReconciled: false})}>Review recovery for {job.issueId}</button>}</td></tr>)}</tbody></table></div>}
    {recovery && <section aria-labelledby="recovery-title"><h3 id="recovery-title">Recovery for job {recovery.jobId}</h3><p>Inspect the recorded processes, workspace changes and usage first. Retry requires explicit evidence; the core also verifies ownership and process state.</p><label>Recovery decision<select disabled={busy} value={recovery.decision} onChange={event => setRecovery({...recovery, decision: event.target.value as RecoveryRequest["decision"]})}><option value="keep_needs_input">Keep blocked for input</option><option value="mark_failed">Mark failed</option><option value="safe_to_retry">Retry after reconciliation</option></select></label><label>Recovery evidence<textarea disabled={busy} rows={4} maxLength={8000} value={recovery.evidence} onChange={event => setRecovery({...recovery, evidence: event.target.value})} /></label>{recovery.decision === "safe_to_retry" && <><label className="runner-recovery-confirm"><input type="checkbox" disabled={busy} checked={recovery.processConfirmedStopped} onChange={event => setRecovery({...recovery, processConfirmedStopped: event.target.checked})} />I confirmed the owned processes have stopped.</label><label className="runner-recovery-confirm"><input type="checkbox" disabled={busy} checked={recovery.effectsReconciled} onChange={event => setRecovery({...recovery, effectsReconciled: event.target.checked})} />I inspected and reconciled the workspace effects.</label></>}<div className="local-issues-actions"><button className="button button-secondary" disabled={busy} onClick={() => setRecovery(null)}>Close recovery</button><button className="button button-primary" disabled={busy || !recovery.evidence.trim() || (recovery.decision === "safe_to_retry" && (!recovery.processConfirmedStopped || !recovery.effectsReconciled))} onClick={() => void resolveRecovery()}>Record recovery decision</button></div></section>}
    {events.length > 0 && <details><summary>Recent batch events ({events.length}, bounded)</summary><ol className="runner-events">{events.map(event => <li key={event.sequence}><strong>{new Date(event.timestampMs).toLocaleString()} · {event.kind}</strong><pre>{JSON.stringify(event.payload, null, 2)}</pre></li>)}</ol></details>}
  </section>;
}
