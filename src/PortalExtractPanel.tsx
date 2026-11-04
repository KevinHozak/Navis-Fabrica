import { invoke } from "@tauri-apps/api/core";
import { useEffect, useRef, useState } from "react";

type ExtractResult = {
  status: "complete" | "partial" | "unknown" | "denied";
  text?: string | null;
  usage?: { state: "known" | "unknown" | "denied"; cost?: string | null; inputTokens?: number | null; outputTokens?: number | null } | null;
};
const pendingKey = "fabrica.portal-extract.pending-ids";
function readPending(): string[] {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(pendingKey) ?? "[]");
    return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string" && /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(item)) : [];
  } catch { return []; }
}
function savePending(values: string[]): boolean {
  try { localStorage.setItem(pendingKey, JSON.stringify(values)); return true; } catch { return false; }
}

export function PortalExtractPanel({ desktop }: { desktop: boolean }) {
  const [prompt, setPrompt] = useState("");
  const [pending, setPending] = useState<string[]>(readPending);
  const [activeId, setActiveId] = useState("");
  const activeIdRef = useRef("");
  const [status, setStatus] = useState("Not checked");
  const [result, setResult] = useState<ExtractResult | null>(null);

  useEffect(() => {
    const onClose = () => { const id = activeIdRef.current; if (id) void invoke("cancel_portal_extract", { operationId: id }).catch(() => undefined); };
    window.addEventListener("beforeunload", onClose);
    return () => window.removeEventListener("beforeunload", onClose);
  }, []);

  async function start() {
    if (!desktop || !prompt.trim() || activeIdRef.current) return;
    if (pending.length >= 128) { setStatus("Unknown usage · review unresolved operations"); return; }
    const operationId = crypto.randomUUID();
    const next = [...pending, operationId];
    if (!savePending(next)) { setStatus("Unavailable"); return; }
    activeIdRef.current = operationId;
    setPending(next); setActiveId(operationId); setResult(null); setStatus("Starting");
    try {
      const output = await invoke<ExtractResult>("start_portal_extract", { prompt, operationId });
      setResult(output); setStatus(output.status === "unknown" ? "Unknown usage" : output.status);
      if (["complete", "partial", "denied"].includes(output.status)) {
        const settled = next.filter(id => id !== operationId);
        if (savePending(settled)) setPending(settled); else setStatus("Unavailable");
      }
    } catch (error) {
      const code = typeof error === "string" ? error : "unknown";
      setStatus(code === "denied" ? "Denied" : code === "disabled" ? "Disabled" : code === "unavailable" ? "Unavailable" : "Unknown usage");
      if (code === "denied" || code === "disabled") {
        const settled = next.filter(id => id !== operationId);
        if (savePending(settled)) setPending(settled); else setStatus("Unavailable");
      }
    } finally { activeIdRef.current = ""; setActiveId(""); }
  }

  async function cancel() {
    const operationId = activeIdRef.current;
    if (!operationId) return;
    setStatus("Cancellation requested · usage may be unknown");
    try { await invoke("cancel_portal_extract", { operationId }); } catch { setStatus("Unknown usage"); }
  }

  return <section className="panel portal-extract-panel">
    <div className="portal-extract-heading"><div><p className="eyebrow">EXPLICIT FEATURE</p><h1>Portal extract</h1></div><span role="status">{desktop ? status : "Unavailable"}</span></div>
    {!desktop && <p>Portal extract requires the desktop app.</p>}
    {pending.length > 0 && <p role="status">{pending.length} unresolved previous operation{pending.length === 1 ? "" : "s"}. They will not be replayed.</p>}
    <label htmlFor="portal-extract-prompt">Prompt</label>
    <textarea id="portal-extract-prompt" value={prompt} maxLength={8000} onChange={event => setPrompt(event.target.value)} disabled={!desktop || Boolean(activeId)} rows={5} />
    <div className="portal-extract-actions">
      <button className="button button-primary" type="button" onClick={() => void start()} disabled={!desktop || !prompt.trim() || Boolean(activeId)}>{activeId ? "Starting…" : "Start extraction"}</button>
      {activeId && <button className="button button-secondary" type="button" onClick={() => void cancel()}>Cancel</button>}
    </div>
    {result?.usage && <p role="status">Usage: {result.usage.state}{result.usage.cost ? ` · $${result.usage.cost}` : ""}</p>}
    {result?.text && <pre className="portal-extract-output">{result.text}</pre>}
  </section>;
}
