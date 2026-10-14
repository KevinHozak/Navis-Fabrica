import { FormEvent, useEffect, useMemo, useState } from "react";
import {
  cancelWorkflowRun, listWorkflowRuns, markWorkflowRunNeedsInput, onWorkflowRunEvent, onWorkflowRunUpdated,
  startWorkflowRun, WorkflowRun, writeWorkflowRunInput,
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
  const selected = useMemo(() => runs.find((run) => run.id === selectedId) ?? runs[0] ?? null, [runs, selectedId]);

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
          {selected.state === "needsInput" && <form className="run-input-form" onSubmit={(event) => { event.preventDefault(); void sendInput(); }}>
            <label>Send process input (one line)<input value={input} onChange={(event) => setInput(event.target.value)} /></label>
            <button className="button button-secondary" type="submit" disabled={!input}>Send input</button>
          </form>}
          {activeStates.has(selected.state) && selected.state !== "needsInput" && <button className="text-button" type="button" onClick={() => { void markNeedsInput(); }}>Mark as waiting for input</button>}
          {selected.exitCode !== null && <p className="registry-context-note">Exit code {selected.exitCode}</p>}
        </> : <div className="empty-state"><h3>Nothing running yet</h3><p>Start a configured executable to see sequence-numbered output here.</p></div>}
      </section>
    </div>
  </div>;
}
