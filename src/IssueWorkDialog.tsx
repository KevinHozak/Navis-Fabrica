import { invoke } from "@tauri-apps/api/core";
import { useEffect, useRef, useState } from "react";
import { batchCommand, runnerContext, refreshProviderObservations, type BatchPreview, type BatchRequest, type BatchStatus, type BatchEvent, type CheckSpec } from "./batchClient";
import { publishBatchActivity } from "./Activity";
import type { ListedIssue } from "./runnerClient";
import { previewBlocked, submissionDisposition } from "./issueWorkState";

export function issueAcceptance(body: string): string {
  return body.split(/\r?\n/).filter(line => /^\s*- \[[ xX]\] /.test(line)).map(line => line.replace(/^\s*- \[[ xX]\] /, "")).join("\n");
}
type Connection = { dataRoot: string; batchId: string; admitted?: boolean };
export function IssueWorkDialog({ projectPath, configPath, issue, onClose }: { projectPath: string; configPath: string; issue: ListedIssue; onClose: () => void }) {
  const id = String(issue.record.frontmatter.id);
  const storageKey = `fabrica.issue-work.v1:${projectPath}:${id}`;
  const dialog = useRef<HTMLDialogElement>(null);
  const [connection, setConnection] = useState<Connection | null>(() => { try { const saved = JSON.parse(localStorage.getItem(storageKey) ?? "null"); return saved && typeof saved.batchId === "string" && typeof saved.dataRoot === "string" ? saved : null; } catch { return null; } });
  const [automatic, setAutomatic] = useState(true);
  const [files, setFiles] = useState("");
  const [acceptance, setAcceptance] = useState(() => issueAcceptance(issue.record.body));
  const [checkProgram, setCheckProgram] = useState("");
  const [checkArgs, setCheckArgs] = useState("");
  const [checkCwd, setCheckCwd] = useState(".");
  const [extraChecks, setExtraChecks] = useState<CheckSpec[]>([]);
  const [preview, setPreview] = useState<BatchPreview | null>(null);
  const [previewRoot, setPreviewRoot] = useState("");
  const [status, setStatus] = useState<BatchStatus | null>(null);
  const [events, setEvents] = useState<BatchEvent[]>([]);
  const [busy, setBusy] = useState(!connection);
  const [error, setError] = useState("");
  const [pollError, setPollError] = useState("");
  const launchLock = useRef(false);
  useEffect(() => { dialog.current?.showModal(); if (!connection) void review(); }, []);
  useEffect(() => {
    if (!connection) return;
    const fields = { dataRoot: connection.dataRoot, batchId: connection.batchId };
    let alive = true, inFlight = false, cursor = 0;
    async function refresh() {
      if (inFlight) return;
      inFlight = true;
      try {
        const current = await batchCommand("status", fields);
        if (current.kind !== "status") throw new Error("Unexpected runner status.");
        if (!alive) return;
        setStatus(current.value); setPollError("");
        publishBatchActivity({ ...connection!, projectPath, state: current.value.snapshot.state, needsAttention: current.value.jobs.some(job => ["needs_input", "reconciliation_required", "failed", "blocked"].includes(job.state)) });
        const page = await batchCommand("events", { ...fields, cursor, limit: 100 });
        if (!alive) return;
        if (page.kind !== "events") throw new Error("Unexpected runner events.");
        cursor = page.value.nextCursor;
        setEvents(previous => [...new Map([...previous, ...page.value.events].map(event => [event.sequence, event])).values()].slice(-500));
      } catch (reason) { if (alive) setPollError(String(reason)); }
      finally { inFlight = false; }
    }
    void refresh(); const timer = window.setInterval(() => void refresh(), 3000);
    return () => { alive = false; window.clearInterval(timer); };
  }, [connection, projectPath]);
  function remember(value: Connection) {
    // Persist before attaching controls; closing the dialog never cancels the worker.
    setConnection(value);
    try { localStorage.setItem(storageKey, JSON.stringify(value)); localStorage.setItem("fabrica.runner.connection.v1", JSON.stringify({ ...value, projectPath })); } catch { setError("Runner admitted this work. Keep this view open; connection could not be saved."); }
    if (value.admitted === true) window.dispatchEvent(new CustomEvent("fabrica-runner-connect", { detail: { dataRoot: value.dataRoot, batchId: value.batchId, projectPath } }));
  }
  async function review(allowLiveProbe = false) {
    if (launchLock.current) return;
    launchLock.current = true; setBusy(true); setError(""); setPreview(null);
    try {
      const context = await runnerContext();
      if (!context.workerAvailable) throw new Error("The bundled runner worker is unavailable.");
      const args = checkArgs.split(/\r?\n/).map(arg => arg.trim()).filter(Boolean);
      if (!automatic && !checkProgram.trim()) throw new Error("Provide a verification check program.");
      const parsedChecks = [{ id: "project-check", program: checkProgram.trim(), args, cwd: checkCwd, timeout_ms: 300000, output_limit_bytes: 65536 }, ...extraChecks];
      const request = await invoke<BatchRequest>("prepare_issue_work", { input: { projectPath, configPath, issueId: id, expectedRevision: issue.revision, autoDetect: automatic, files: files.split(/\r?\n/).map(value => value.trim()).filter(Boolean), acceptance: acceptance.split(/\r?\n/).map(value => value.trim()).filter(Boolean), checks: parsedChecks } });
      // Refresh local runtime/auth facts before preview; no model inference or
      // remote catalog probe is needed for the selected Codex route.
      await refreshProviderObservations(context.dataRoot, request.settingsPath, allowLiveProbe);
      const result = await batchCommand("batch-preview", { dataRoot: context.dataRoot, batchRequest: request });
      if (result.kind !== "preview") throw new Error("Unexpected runner preview.");
      setPreview(result.value);
      setPreviewRoot(context.dataRoot);
    } catch (reason) { setError(String(reason)); }
    finally { launchLock.current = false; setBusy(false); }
  }
  async function start() {
    if (!preview || launchLock.current) return;
    launchLock.current = true; setBusy(true); setError("");
    let target: Connection | null = null;
    let attempted = false;
    try {
      target = { dataRoot: previewRoot, batchId: preview.request.batchId };
      // Save the reviewed identity before submit so ambiguous responses can be inspected after reopen.
      localStorage.setItem(storageKey, JSON.stringify({ ...target, admitted: false }));
      attempted = true;
      const result = await batchCommand("submit", { dataRoot: target.dataRoot, batchRequest: preview.request, previewFingerprint: preview.fingerprint });
      if (result.kind !== "submitted") throw new Error("Unexpected runner submission.");
      remember({ ...target, batchId: result.value.batchId, admitted: true });
    } catch (reason) {
      setError(String(reason));
      // WORKER_LAUNCH_FAILED proves admission; other failures never attach another batch's controls.
      if (!attempted) { /* No submit occurred; keep the reviewed plan available. */ }
      else if (target && submissionDisposition(reason) === "admitted") remember({ ...target, admitted: true });
      else if (submissionDisposition(reason) === "refused") { localStorage.removeItem(storageKey); setPreview(null); }
      else if (target) remember({ ...target, admitted: false });
    } finally { launchLock.current = false; setBusy(false); }
  }
  async function cancel() {
    if (!connection || launchLock.current) return;
    launchLock.current = true; setBusy(true); setError("");
    try { await batchCommand("cancel", { dataRoot: connection.dataRoot, batchId: connection.batchId }); }
    catch (reason) { setError(String(reason)); }
    finally { launchLock.current = false; setBusy(false); }
  }
  async function refreshProviders() {
    await review(true);
  }
  const blocked = !preview || previewBlocked(preview.plan);
  const terminal = status && ["completed", "failed", "cancelled", "window_expired"].includes(status.snapshot.state);
  return <dialog ref={dialog} className="commit-review-dialog commit-review-workflow build-progress-dialog issue-work-dialog" aria-labelledby="issue-work-title" onCancel={event => { event.preventDefault(); if (!busy) onClose(); }}>
    <div className="commit-review-heading"><div><p className="eyebrow">{id} · {String(issue.record.frontmatter.title)}</p><h2 id="issue-work-title">Work on issue</h2></div><button className="text-button" disabled={busy} onClick={onClose} aria-label="Close issue progress">×</button></div>
    {!connection ? <div className="run-launch-form">
      <div className="commit-review-summary" role="status"><strong>{busy ? "Preparing issue plan…" : preview ? blocked ? "Plan needs attention" : "Ready to work" : "Plan needs attention"}</strong></div>
      {preview && <><p>{preview.request.jobs[0]?.plan.tasks[0]?.model_id} · implementation and independent review</p><details><summary>Proposed scope and checks · {preview.request.jobs[0]?.plan.tasks[0]?.files.length ?? 0} paths · {preview.request.verification[0]?.checks.length ?? 0} checks</summary><ul>{preview.request.jobs[0]?.plan.tasks[0]?.files.map(file => <li key={file}><code>{file}</code></li>)}</ul><ul>{preview.request.verification.flatMap(policy => policy.checks).map(check => <li key={check.id}><code>{check.program} {check.args.join(" ")}</code> · {check.cwd}</li>)}</ul></details><details><summary>Acceptance criteria</summary><ul>{preview.request.verification[0]?.acceptanceCriteria.map((criterion, index) => <li key={index}>{criterion}</li>)}</ul></details>{preview.plan.warnings.map((warning, index) => <p key={index}>{warning}</p>)}{preview.plan.issues?.flatMap(item => item.tasks?.flatMap(task => task.reasons) ?? []).map((reason, index) => <p key={index}>{reason}</p>)}</>}
      <p className="muted">Isolated worktree · 400,000 tokens · 60 minutes · no paid API budget</p>
      <details><summary>Advanced</summary><label className="run-checkbox"><input type="checkbox" checked={!automatic} disabled={busy} onChange={event => { if (event.target.checked && preview) { const task = preview.request.jobs[0]?.plan.tasks[0]; const policy = preview.request.verification[0]; setFiles(task?.files.join("\n") ?? ""); setAcceptance(policy?.acceptanceCriteria.join("\n") ?? ""); const check = policy?.checks[0]; setCheckProgram(check?.program ?? ""); setCheckArgs(check?.args.join("\n") ?? ""); setCheckCwd(check?.cwd ?? "."); setExtraChecks(policy?.checks.slice(1) ?? []); } setAutomatic(!event.target.checked); setPreview(null); }} />Override automatic plan</label>
        {!automatic && <><label>Files or folders<textarea rows={2} value={files} disabled={busy} onChange={event => { setFiles(event.target.value); setPreview(null); }} placeholder="One repository-relative path per line" /></label><label>Acceptance criteria<textarea rows={4} value={acceptance} disabled={busy} onChange={event => { setAcceptance(event.target.value); setPreview(null); }} /></label><fieldset disabled={busy}><legend>Verification check</legend><label>Program<input value={checkProgram} onChange={event => { setCheckProgram(event.target.value); setPreview(null); }} placeholder="node" /></label><label>Arguments<textarea rows={2} value={checkArgs} onChange={event => { setCheckArgs(event.target.value); setPreview(null); }} placeholder="One argument per line" /></label><label>Working folder<input value={checkCwd} onChange={event => { setCheckCwd(event.target.value); setPreview(null); }} /></label></fieldset>{extraChecks.map(check => <div key={check.id}><code>{check.program} {check.args.join(" ")}</code><button type="button" disabled={busy} onClick={() => { setExtraChecks(previous => previous.filter(item => item.id !== check.id)); setPreview(null); }}>Remove check</button></div>)}</>}
        {preview && <details><summary>Execution plan</summary><pre>{JSON.stringify(preview.request, null, 2)}</pre></details>}
      </details>
    </div>
      : <><div className="commit-review-summary" role="status"><strong>{pollError ? "Connection unavailable" : status?.snapshot.state.replaceAll("_", " ") ?? "Connecting…"}</strong></div><ol className="build-progress-steps" aria-label="Issue work progress">{status?.jobs.map(job => <li key={job.id}><strong>{job.state.replaceAll("_", " ")}</strong><span>{job.completionGate}</span>{job.lastAttemptSummary && <p>{job.lastAttemptSummary}</p>}</li>)}</ol><section className="commit-review-progress" aria-label="Issue work output"><h3>Messages</h3><pre tabIndex={0}>{events.map(event => `${event.kind}: ${JSON.stringify(event.payload)}`).join("\n") || "Waiting for runner events…"}</pre></section>{status && <details><summary>Checks and usage</summary><p>{status.budget.tokensUsed.toLocaleString()} tokens reported · {status.budget.unresolved} unresolved usage records</p><pre>{JSON.stringify(status.jobs.map(job => job.checkpoints), null, 2)}</pre></details>}{connection.admitted !== true && <p role="alert">Submission outcome is unknown. Inspect runner details before retrying.</p>}<a href="#batches" onClick={() => { window.dispatchEvent(new CustomEvent("fabrica-runner-connect", { detail: { dataRoot: connection.dataRoot, batchId: connection.batchId, projectPath } })); onClose(); }}>Runner details and recovery</a></>}
    {(error || pollError) && <p className="project-card-refresh-error" role="alert">{error || pollError}</p>}
    <div className="dialog-actions">{!connection && <>{!preview && <button className="button button-primary" disabled={busy || (!automatic && (!files.trim() || !acceptance.trim()))} onClick={() => void review()}>{busy ? "Preparing…" : "Review plan"}</button>}{preview && <button className="button button-primary" disabled={busy || blocked} onClick={() => void start()}>Start work</button>}{preview && <button className="button button-secondary" disabled={busy} onClick={() => void refreshProviders()}>Refresh provider status</button>}</>}{connection && connection.admitted === true && !terminal && <button className="button button-secondary" disabled={busy || !status} onClick={() => void cancel()}>Stop work</button>}<button className="button button-secondary" disabled={busy} onClick={onClose}>Close</button></div>
  </dialog>;
}
