import { useEffect, useRef, useState } from "react";
import { cancelWorkflowRun, startScriptBuildRun, type ProjectEntry, type WorkflowRun } from "./settings";
import { latestBuildStep } from "./buildProgress";
import { useLiveWorkflowRun } from "./useLiveWorkflowRun";

const steps = ["Prepare version", "Build artifacts", "Verify artifact versions", "Check startup", "Save build receipt", "Complete"];
async function beginBuild(projectId: string, profile: "release" | "debug") {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(crypto.randomUUID()));
  const operationKey = Array.from(new Uint8Array(digest), byte => byte.toString(16).padStart(2, "0")).join("");
  return startScriptBuildRun(projectId, { action: "dev-build", operationKey, intent: `Build ${profile} artifacts`, buildProfile: profile,
    issueUrl: null, scopePaths: [], requestedEffects: [], sideEffects: null });
}

export function BuildProgressDialog({ project, profile, onClose, onStarted, initialRun }: {
  project: ProjectEntry; profile: "release" | "debug"; onClose: () => void; onStarted?: (run: WorkflowRun) => void; initialRun?: WorkflowRun;
}) {
  const { run, setRun, running } = useLiveWorkflowRun();
  const [starting, setStarting] = useState(true);
  const [stopping, setStopping] = useState(false);
  const [error, setError] = useState("");
  const dialogRef = useRef<HTMLDialogElement>(null);
  const outputRef = useRef<HTMLPreElement>(null);
  const pending = useRef<Promise<WorkflowRun> | null>(null);
  const startedCallback = useRef(onStarted);
  startedCallback.current = onStarted;
  useEffect(() => {
    dialogRef.current?.showModal();
    let active = true;
    // Reuse the same launch if React replays mount effects.
    pending.current ??= initialRun ? Promise.resolve(initialRun) : beginBuild(project.id, profile);
    pending.current.then(value => { if (active) { setRun(value); startedCallback.current?.(value); } })
      .catch(reason => { if (active) setError(reason instanceof Error ? reason.message : String(reason)); })
      .finally(() => { if (active) setStarting(false); });
    return () => { active = false; };
  }, [project.id, profile, setRun, initialRun?.id]);
  useEffect(() => { const output = outputRef.current; if (output) output.scrollTop = output.scrollHeight; }, [run?.nextSequence]);
  function close() { dialogRef.current?.close(); onClose(); }
  async function stop() {
    if (!run || stopping) return;
    setStopping(true); setError("");
    try { await cancelWorkflowRun(run.id); }
    catch (reason) { setError(reason instanceof Error ? reason.message : String(reason)); }
    finally { setStopping(false); }
  }
  const phase = run ? latestBuildStep(run) : null;
  const step = phase ? Number(/^Step ([1-6])/.exec(phase)?.[1] ?? 0) : 0;
  const completed = run?.state === "completed";
  const status = starting ? "Starting…" : completed ? "Completed" : running ? run?.state === "cancelling" ? "Stopping…" : "Building…" : run ? `Stopped · ${run.state}` : "Could not start";
  return <dialog ref={dialogRef} className="commit-review-dialog commit-review-workflow build-progress-dialog" aria-labelledby="build-progress-title" onCancel={event => { event.preventDefault(); if (!starting) close(); }}>
    <div className="commit-review-heading"><div><p className="eyebrow">{project.name} · {profile === "release" ? "Release" : "Debug"}</p><h2 id="build-progress-title">Build project</h2></div><button className="text-button" type="button" aria-label="Close build progress" disabled={starting} onClick={close}>×</button></div>
    <div className="commit-review-summary" role="status"><strong>{phase ?? status}</strong>{(starting || running) && <span className="commit-review-loading"><span className="commit-review-spinner" />{phase ? status : ""}</span>}</div>
    <ol className="build-progress-steps" aria-label="Build steps">{steps.map((label, index) => <li key={label} aria-current={!completed && step === index + 1 ? "step" : undefined} className={completed || step > index + 1 ? "is-complete" : step === index + 1 ? "is-current" : ""}><span aria-hidden="true">{completed || step > index + 1 ? "✓" : index + 1}</span>{label}</li>)}</ol>
    <section className="commit-review-progress" aria-label="Build output"><div className="commit-review-progress-heading"><h3>Messages</h3><span role="status">{status}</span></div><pre ref={outputRef} tabIndex={0} aria-label="Build messages">{run?.events.filter(event => !event.text.startsWith("NAVIS_BUILD_ARTIFACT:")).map(event => event.text.replace(/^BUILD_STEP:[1-6]\/6:/, "")).join("\n") || "Waiting for build output…"}</pre></section>
    {run?.workflow?.buildArtifact && <p className="build-progress-artifact"><strong>{run.workflow.buildArtifact.verified ? "Verified artifact" : "Artifact"}</strong><code>{run.workflow.buildArtifact.path}</code></p>}
    {error && <p className="project-card-refresh-error" role="alert">{error}</p>}
    <div className="dialog-actions">{running && <button className="button button-secondary" type="button" disabled={stopping || run?.state === "cancelling"} onClick={() => void stop()}>{stopping ? "Stopping…" : "Stop build"}</button>}<button className="button button-secondary" type="button" disabled={starting} onClick={close}>Close</button></div>
  </dialog>;
}
