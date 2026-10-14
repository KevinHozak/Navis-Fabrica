import { FormEvent, useEffect, useMemo, useState } from "react";
import {
  cancelWorkflowRun, discoverWorkflowSkills, listWorkflowRuns, markWorkflowRunNeedsInput, onWorkflowRunEvent, onWorkflowRunUpdated,
  loadSettings, respondCodexApproval, respondCodexUserInput, startAntigravityWorkflowRun, startCodexWorkflowRun, startWorkflowRun, WorkflowRun, WorkflowSkillStatus, writeWorkflowRunInput,
} from "./settings";

const activeStates = new Set(["queued", "running", "needsInput", "cancelling"]);

export function RunsPanel({ defaultCwd }: { defaultCwd: string }) {
  const [runs, setRuns] = useState<WorkflowRun[]>([]);
  const [executable, setExecutable] = useState("");
  const [cwd, setCwd] = useState(defaultCwd);
  const [argumentsText, setArgumentsText] = useState("[]");
  const [timeout, setTimeout] = useState(3600);
  const [mutating, setMutating] = useState(false);
  const [trustedExecutable, setTrustedExecutable] = useState(false);
  const [selectedId, setSelectedId] = useState("");
  const [input, setInput] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [codexPrompt, setCodexPrompt] = useState("");
  const [codexSkill, setCodexSkill] = useState("");
  const [codexTimeout, setCodexTimeout] = useState(3600);
  const [skills, setSkills] = useState<WorkflowSkillStatus[]>([]);
  const [respondedRequests, setRespondedRequests] = useState<Set<string>>(() => new Set());
  const [codexAnswers, setCodexAnswers] = useState<Record<string, Record<string, string>>>({});
  const [selectedProviderKind, setSelectedProviderKind] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    listWorkflowRuns().then((items) => { if (active) setRuns(items); }).catch((reason: unknown) => {
      if (active) setError(reason instanceof Error ? reason.message : String(reason));
    });
    const update = onWorkflowRunUpdated((next) => {
      if (active) setRuns((items) => [next, ...items.filter((item) => item.id !== next.id)].slice(0, 100));
    });
    const event = onWorkflowRunEvent(([runId, next]) => {
      if (active) setRuns((items) => items.map((item) => item.id === runId
        ? { ...item, nextSequence: Math.max(item.nextSequence, next.sequence + 1), events: [...item.events, next].slice(-2000) }
        : item));
    });
    return () => {
      active = false;
      void update.then((stop) => stop());
      void event.then((stop) => stop());
    };
  }, []);

  useEffect(() => setCwd(defaultCwd), [defaultCwd]);
  useEffect(() => { loadSettings().then((settings) => setSelectedProviderKind(settings.providers.find((provider) => provider.id === settings.providerSelection.providerId)?.kind ?? null)).catch(() => setSelectedProviderKind(null)); }, []);
  useEffect(() => { discoverWorkflowSkills().then(setSkills).catch(() => setSkills([])); }, []);
  const selected = useMemo(() => runs.find((run) => run.id === selectedId) ?? runs[0] ?? null, [runs, selectedId]);

  async function startAgent(event: FormEvent) {
    event.preventDefault();
    setError("");
    setBusy(true);
    try {
      const run = selectedProviderKind === "antigravity"
        ? await startAntigravityWorkflowRun({ cwd: defaultCwd, prompt: codexPrompt, timeoutSeconds: codexTimeout })
        : await startCodexWorkflowRun({ cwd: defaultCwd, prompt: codexPrompt, skillId: codexSkill || null, timeoutSeconds: codexTimeout });
      setRuns((items) => [run, ...items.filter((item) => item.id !== run.id)].slice(0, 100));
      setSelectedId(run.id);
      setCodexPrompt("");
    } catch (reason) { setError(reason instanceof Error ? reason.message : String(reason)); }
    finally { setBusy(false); }
  }

  async function start(event: FormEvent) {
    event.preventDefault();
    setError("");
    let args: unknown;
    try { args = JSON.parse(argumentsText); } catch { setError("Arguments must be a valid JSON array of strings."); return; }
    if (!Array.isArray(args) || !args.every((item) => typeof item === "string")) {
      setError("Arguments must be a valid JSON array of strings."); return;
    }
    setBusy(true);
    try {
      const run = await startWorkflowRun({ executable, arguments: args, cwd, timeoutSeconds: timeout, mutating });
      setRuns((items) => {
        const latest = items.find((item) => item.id === run.id);
        const chosen = latest && (latest.nextSequence > run.nextSequence || latest.state !== "queued") ? latest : run;
        return [chosen, ...items.filter((item) => item.id !== run.id)].slice(0, 100);
      });
      setSelectedId(run.id);
    } catch (reason) { setError(reason instanceof Error ? reason.message : String(reason)); }
    finally { setBusy(false); }
  }

  async function cancel(runId: string) {
    try { await cancelWorkflowRun(runId); }
    catch (reason) { setError(reason instanceof Error ? reason.message : String(reason)); }
  }

  async function sendInput() {
    if (!selected) return;
    try {
      const sent = await writeWorkflowRunInput(selected.id, `${input}\n`);
      if (!sent) setError("This process is not accepting input.");
      else setInput("");
    } catch (reason) { setError(reason instanceof Error ? reason.message : String(reason)); }
  }

  async function markNeedsInput() {
    if (!selected) return;
    try { await markWorkflowRunNeedsInput(selected.id); }
    catch (reason) { setError(reason instanceof Error ? reason.message : String(reason)); }
  }

  return <div className="runs-workspace">
    <section className="panel run-launch-panel" aria-labelledby="codex-run-title">
      <div className="panel-heading"><div><h2 id="codex-run-title">Start {selectedProviderKind === "antigravity" ? "Antigravity" : "Codex"} in this project</h2><p>Uses the selected provider and model, plus settings supported by that runtime.</p></div></div>
      <div className="panel-rule" />
      <form className="run-launch-form" onSubmit={(event) => { void startAgent(event); }}>
        <label>Project scope<input value={defaultCwd} readOnly aria-readonly="true" /></label>
        {selectedProviderKind !== "antigravity" && <label>Codex skill (optional)<select value={codexSkill} onChange={(event) => setCodexSkill(event.target.value)}>
          <option value="">No skill</option>{skills.filter((skill) => skill.status === "available" && skill.compatibleAgents.includes("Codex")).map((skill) => <option key={skill.id} value={skill.id}>{skill.id}</option>)}
        </select></label>}
        {selectedProviderKind === "antigravity" && <p className="registry-context-note">Antigravity CLI discovers workspace skills from .agents/skills and global CLI skills. Headless runs cannot pause for approvals or input; terminal tools run with the CLI sandbox. Structured progress and reasoning-effort options are unavailable in the installed CLI version.</p>}
        <label>Prompt<textarea rows={4} required maxLength={60000} value={codexPrompt} onChange={(event) => setCodexPrompt(event.target.value)} placeholder={`Describe the work for ${selectedProviderKind === "antigravity" ? "Antigravity" : "Codex"}…`} /></label>
        <div className="run-launch-options">
          <label>Timeout (seconds)<input type="number" min={1} max={86400} value={codexTimeout} onChange={(event) => setCodexTimeout(Number(event.target.value))} /></label>
          <p className="registry-context-note">{selectedProviderKind === "antigravity" ? "Git projects must have a clean worktree. The headless CLI cannot pause for approvals; tool permissions follow Antigravity CLI settings and sandbox policy." : "Git projects must have a clean worktree before Codex starts. Approval requests pause for your decision."}</p>
          <button className="button button-primary" type="submit" disabled={busy || !codexPrompt.trim() || !["codex", "antigravity"].includes(selectedProviderKind ?? "")}>{busy ? "Starting…" : `Start ${selectedProviderKind === "antigravity" ? "Antigravity" : "Codex"}`}</button>
        </div>
      </form>
      {error && <p className="scan-notice error-notice" role="alert">{error}</p>}
    </section>
    <section className="panel run-launch-panel" aria-labelledby="run-launch-title">
      <div className="panel-heading"><div><h2 id="run-launch-title">Start a configured executable</h2><p>Arguments are passed as separate values to the native process runner.</p></div></div>
      <div className="panel-rule" />
      <form className="run-launch-form" onSubmit={(event) => { void start(event); }}>
        <label>Executable path or command<input required value={executable} onChange={(event) => setExecutable(event.target.value)} placeholder="C:\\Tools\\codex.exe or npm" /></label>
        <label>Working directory<input required value={cwd} onChange={(event) => setCwd(event.target.value)} placeholder="C:\\Projects\\my-project" /></label>
        <label>Arguments (JSON string array)<textarea rows={3} value={argumentsText} onChange={(event) => setArgumentsText(event.target.value)} spellCheck={false} /></label>
        <div className="run-launch-options">
          <label>Timeout (seconds)<input type="number" min={1} max={86400} value={timeout} onChange={(event) => setTimeout(Number(event.target.value))} /></label>
          <label className="run-checkbox"><input type="checkbox" checked={mutating} onChange={(event) => setMutating(event.target.checked)} /> This run may modify the repository</label>
          <label className="run-checkbox"><input type="checkbox" checked={trustedExecutable} onChange={(event) => setTrustedExecutable(event.target.checked)} /> I trust this executable</label>
          <button className="button button-primary" type="submit" disabled={busy || !trustedExecutable}>{busy ? "Starting…" : "Start run"}</button>
        </div>
      </form>
      {error && <p className="scan-notice error-notice" role="alert">{error}</p>}
    </section>
    <div className="runs-layout">
      <section className="panel run-list-panel" aria-labelledby="run-history-title">
        <div className="panel-heading"><div><h2 id="run-history-title">Recent runs</h2><p>Persisted local history · output is bounded and token patterns are redacted.</p></div></div><div className="panel-rule" />
        {runs.length ? runs.map((run) => <button key={run.id} type="button" className={`run-list-item${selected?.id === run.id ? " selected" : ""}`} onClick={() => setSelectedId(run.id)}>
          <strong>{run.executable.split(/[\\/]/).pop()}</strong><span>{run.state}{run.mutating ? " · mutating" : " · read only"}</span><small>{run.cwd}</small>
        </button>) : <p className="registry-context-note">No runs yet.</p>}
      </section>
      <section className="panel run-detail-panel" aria-labelledby="run-detail-title">
        <div className="panel-heading"><div><h2 id="run-detail-title">{selected ? selected.executable : "Run output"}</h2><p>{selected ? `${selected.state} · ${selected.cwd}` : "Select a run or start a configured executable."}</p></div>
          {selected && activeStates.has(selected.state) && <button className="button button-secondary" type="button" onClick={() => { void cancel(selected.id); }}>Cancel run</button>}
        </div><div className="panel-rule" />
        {selected ? <>
          <pre className="run-output" aria-live="polite">{selected.events.map((item) => `[${item.sequence}] ${item.kind}: ${item.text}`).join("\n") || "Waiting for process output…"}</pre>
          {selected.state === "needsInput" && selected.events.filter((item) => item.kind === "approvalRequest" || item.kind === "userInputRequest").map((item) => <CodexRequestCard key={`${item.sequence}-${item.kind}`} runId={selected.id} event={item} onResolved={(requestId) => setRespondedRequests((prior) => new Set(prior).add(requestId))} isResolved={(requestId) => respondedRequests.has(requestId)} />)}
          {selected.state === "needsInput" && selected.provider !== "codex" && selected.provider !== "antigravity" && <form className="run-input-form" onSubmit={(event) => { event.preventDefault(); void sendInput(); }}>
            <label>Send process input (one line)<input value={input} onChange={(event) => setInput(event.target.value)} /></label>
            <button className="button button-secondary" type="submit" disabled={!input}>Send input</button>
          </form>}
          {activeStates.has(selected.state) && selected.state !== "needsInput" && selected.provider !== "codex" && selected.provider !== "antigravity" && <button className="text-button" type="button" onClick={() => { void markNeedsInput(); }}>Mark as waiting for input</button>}
          {selected.exitCode !== null && <p className="registry-context-note">Exit code {selected.exitCode}</p>}
        </> : <div className="empty-state"><h3>Nothing running yet</h3><p>Start a configured executable to see sequence-numbered output here.</p></div>}
      </section>
    </div>
  </div>;
}

type CodexQuestion = { id: string; header: string; question: string; isOther: boolean; isSecret: boolean; options: { label: string; description: string }[] | null };
type CodexRequest = { requestId: string | number; method?: string; params?: { kind?: string; reason?: string | null; command?: string | null; permissions?: Record<string, unknown> | null; availableDecisions?: (string | Record<string, unknown>)[] | null }; questions?: CodexQuestion[] };

const OTHER_OPTION_VALUE = "__navis_codex_other_option__";

function CodexRequestCard({ runId, event, onResolved, isResolved }: {
  runId: string;
  event: { kind: string; text: string };
  onResolved: (requestId: string) => void;
  isResolved: (requestId: string) => boolean;
}) {
  const [answers, setAnswers] = useState<Record<string, string>>({});
  const [otherSelected, setOtherSelected] = useState<Record<string, boolean>>({});
  const [otherAnswers, setOtherAnswers] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  let request: CodexRequest;
  try { request = JSON.parse(event.text) as CodexRequest; } catch { return null; }
  const requestId = String(request.requestId);
  if (isResolved(requestId)) return null;

  async function approve(decision: string) {
    setBusy(true);
    try {
      await respondCodexApproval(runId, requestId, decision);
      onResolved(requestId);
    } catch (reason) { setError(reason instanceof Error ? reason.message : String(reason)); setBusy(false); }
  }

  async function answer(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    try {
      const values: Record<string, string[]> = {};
      (request.questions ?? []).forEach((question) => {
        const selectedOther = question.isOther && otherSelected[question.id];
        const value = selectedOther ? otherAnswers[question.id] ?? "" : answers[question.id] ?? "";
        const trimmed = value.trim();
        values[question.id] = trimmed ? [selectedOther ? `user_note: ${trimmed}` : trimmed] : [];
      });
      await respondCodexUserInput(runId, requestId, values);
      onResolved(requestId);
    } catch (reason) { setError(reason instanceof Error ? reason.message : String(reason)); setBusy(false); }
  }

  if (event.kind === "approvalRequest") {
    const choices = (request.params?.availableDecisions ?? []).filter((value): value is string => typeof value === "string");
    const permissionRequest = request.method === "item/permissions/requestApproval";
    const safeChoices = permissionRequest ? ["accept", "decline"] : choices.length ? choices : ["decline"];
    return <section className="run-input-form" aria-label="Codex approval request">
      <strong>{permissionRequest ? "Codex requests additional permissions" : "Codex is waiting for approval"}</strong>
      {request.params?.reason && <p>{request.params.reason}</p>}
      {request.params?.command && <pre>{request.params.command}</pre>}
      {permissionRequest && request.params?.permissions && <pre aria-label="Requested permissions">{JSON.stringify(request.params.permissions, null, 2)}</pre>}
      {error && <p role="alert">{error}</p>}
      <div className="settings-actions">{safeChoices.map((choice) => <button key={choice} className={choice === "accept" ? "button button-primary" : "button button-secondary"} type="button" disabled={busy} onClick={() => { void approve(choice); }}>{permissionRequest && choice === "accept" ? "Grant requested permissions" : choice === "accept" ? "Approve once" : choice === "decline" ? "Decline" : choice === "acceptForSession" ? "Approve for session" : choice}</button>)}</div>
    </section>;
  }
  if (event.kind !== "userInputRequest") return null;
  return <form className="run-input-form" aria-label="Codex input request" onSubmit={(event) => { void answer(event); }}>
    <strong>Codex needs your input</strong>
    {(request.questions ?? []).map((question) => <label key={question.id}>{question.header}: {question.question}
      {question.options?.length ? <>
        <select value={otherSelected[question.id] ? OTHER_OPTION_VALUE : answers[question.id] ?? ""} onChange={(event) => {
          const isOther = event.target.value === OTHER_OPTION_VALUE;
          setOtherSelected((prior) => ({ ...prior, [question.id]: isOther }));
          if (!isOther) setAnswers((prior) => ({ ...prior, [question.id]: event.target.value }));
        }}>
          <option value="">Choose an answer</option>{question.options.map((option) => <option key={option.label} value={option.label}>{option.label}</option>)}
          {question.isOther && <option value={OTHER_OPTION_VALUE}>Other</option>}
        </select>
        {question.isOther && otherSelected[question.id] && <input type={question.isSecret ? "password" : "text"} aria-label={`${question.header} other answer`} placeholder="Enter another answer" value={otherAnswers[question.id] ?? ""} onChange={(event) => setOtherAnswers((prior) => ({ ...prior, [question.id]: event.target.value }))} />}
      </> : <input type={question.isSecret ? "password" : "text"} value={answers[question.id] ?? ""} onChange={(event) => setAnswers((prior) => ({ ...prior, [question.id]: event.target.value }))} />}
    </label>)}
    {error && <p role="alert">{error}</p>}
    <button className="button button-secondary" type="submit" disabled={busy}>Send answers</button>
  </form>;
}
