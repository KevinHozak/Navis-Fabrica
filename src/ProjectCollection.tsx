import { useWorkflowActivity, useBatchActivity, activityLabel, sameProjectPath } from "./Activity";
import { useEffect, useRef, useState } from "react";
import { draftProjectCommitMessage, prepareProjectCommitReview, getProjectIcon, getReleaseExecutableVersion, loadSettings, saveProjectDisplayPreferences, defaultProjectDisplayPreferences, isDesktopApp, runProjectGitAction, openProjectFolder, startReviewedGitRun, type CommitDraftContext, type CommitMessageDraft, type ProjectEntry, type ReleaseExecutableVersion, type WorkflowRun } from "./settings";
import { issueCommand, type IssuePage } from "./runnerClient";
import { LocalIssuesPanel } from "./LocalIssuesPanel";

import { BuildProgressDialog } from "./BuildProgressDialog";
import { useLiveWorkflowRun } from "./useLiveWorkflowRun";
import { latestGitStep, gitOutputText, pullRequestMergeStatus, localSyncStatus } from "./gitProgress";

type RepoQuickAction = "pull" | "push" | "build" | "main";
function quickActionReason(action: RepoQuickAction, project: ProjectEntry, options: {
  desktop: boolean; busy: boolean; gitBusy: boolean; hasHandler: boolean;
}): string | null {
  if (!options.desktop) return "Requires the desktop app";
  if ((action === "pull" || action === "push" || action === "main") && !project.isGitRepository) return "Requires a Git repository";
  if (options.busy || options.gitBusy) return "A repository operation is active";

  if (action === "build") {
    if (!project.detectedStacks.includes("Tauri")) return "Requires a Tauri project";
    const cargo = project.workflowPrerequisites.find(item => item.name === "cargo" && item.requiredFor.includes("Tauri"));
    if (!cargo?.available) return "Cargo is required for Tauri builds";
    const rustc = project.workflowPrerequisites.find(item => item.name === "rustc" && item.requiredFor.includes("Tauri"));
    if (!rustc?.available) return "Rust is required for Tauri builds";
    return options.hasHandler ? null : "Build workflow is unavailable";
  }

  if (project.git.status !== "available") {
    if (action === "push" && project.git.status === "unborn" && project.git.branch && project.git.dirty === true) {
      // A named unborn branch can still review and commit its initial files.
    } else return project.git.status === "detached" || !project.git.branch
      ? "Check out an attached branch first"
      : "Git status is unavailable; refresh Git details";
  }
  if (!project.git.branch) return "Check out an attached branch first";
  if (action === "push") {
    if (project.git.dirty === false) {
      if (project.git.upstream.status === "noUpstream") return "No changes to commit and no upstream push target";
      if (project.git.upstream.status !== "tracking" || project.git.upstream.ahead == null || project.git.upstream.behind == null) return "Upstream status is unavailable; refresh Git details";
      if (project.git.upstream.behind > 0 && project.git.upstream.ahead === 0) return "Pull upstream changes before pushing";
      if (project.git.upstream.ahead < 1) return "No committed changes to push";
      if (!project.git.remotes.some(remote => remote.name === project.git.upstream.remote && remote.github)) return "Push requires a supported GitHub upstream";
      return options.hasHandler ? null : "Push workflow is unavailable";
    }
    if (project.git.dirty !== true) return "Refresh Git to verify working tree status";
    if (project.git.changedFiles == null) return "Changed-file list is unavailable; refresh Git details";
    if (project.git.changedFiles.length === 0) return "No changed files to review and push";
    if (!project.git.remotes.some(remote => remote.github) && project.git.remotes.length > 0) return "Configured remote is not supported for pushing";
    return options.hasHandler ? null : "Push workflow is unavailable";
  }
  if (project.git.dirty !== false) return project.git.dirty ? "Commit or move uncommitted changes first" : "Refresh Git to verify working tree status";

  if (action === "main") return project.git.branch === "main" ? "Already on main; use Pull to update" : null;
  if (action === "pull") {
    if (project.git.upstream.status === "noUpstream") return "Configure an upstream for this branch, then refresh Git";
    if (project.git.upstream.status !== "tracking") return "Upstream status is unavailable; refresh Git details";
    if ((project.git.upstream.ahead ?? 0) > 0 && (project.git.upstream.behind ?? 0) > 0) return "Branch has diverged from its upstream; resolve divergence first";
    return null;
  }
  return null;
}

function unavailableTitle(action: string, reason: string | null): string {
  return reason ? `${action} unavailable: ${reason}` : action;
}

export type QuickActionEffects = ("commit" | "push")[];
export function ProjectCollection({ projects, manage = false, onSelect, onQuickAction, onRefreshProject, refreshingPaths = [], refreshErrorPaths = [], refreshVersions = {}, startupRefreshPending = false }: { projects: ProjectEntry[]; manage?: boolean; onSelect: (id: string) => void; onQuickAction?: (id: string, action: "dev-push" | "dev-build", effects?: QuickActionEffects) => void; onRefreshProject?: (path: string) => void; refreshingPaths?: string[]; refreshErrorPaths?: string[]; refreshVersions?: Record<string, number>; startupRefreshPending?: boolean }) {
  const [preferences, setPreferences] = useState(defaultProjectDisplayPreferences);
  const [ready, setReady] = useState(false);
  const [message, setMessage] = useState("");
  const runs = useWorkflowActivity();
  const [selected, setSelected] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [openedIds, setOpenedIds] = useState<string[]>([]);
  useEffect(() => { let alive = true; loadSettings().then(s => { if (alive) { setPreferences(s.preferences.projectDisplay); setReady(true); } }).catch(e => { if (alive) setMessage(`Could not load collection: ${String(e)}`); }); const changed = () => { loadSettings().then(s => { if (alive) setPreferences(s.preferences.projectDisplay); }).catch(e => setMessage(String(e))); };
    window.addEventListener("fabrica-collection-saved", changed);
    return () => { alive = false; window.removeEventListener("fabrica-collection-saved", changed); }; }, []);
  async function save() {
    setSaving(true);
    try {
      await saveProjectDisplayPreferences(preferences);
      const saved = (await loadSettings()).preferences.projectDisplay;
      if (JSON.stringify(saved.favoriteProjectIds) !== JSON.stringify(preferences.favoriteProjectIds)) throw new Error("Collection read-back did not match");
      setPreferences(saved); window.dispatchEvent(new Event("fabrica-collection-saved")); setMessage("My Projects saved and verified.");
    } catch (e) { setMessage(`Could not save: ${String(e)}`); }
    finally { setSaving(false); }
  }
  const unique = [...new Map(projects.map(p => [p.id, p])).values()];
  const visible = unique.filter(p => manage || preferences.favoriteProjectIds.includes(p.id));
  return <section className="my-projects">
    {message && <p role="status">{message}</p>}
    {!ready ? <p>Loading saved collection…</p> : <>
      {manage ? <><div className="collection-options">{visible.map(p => <label key={p.id}><input type="checkbox" disabled={saving} checked={preferences.favoriteProjectIds.includes(p.id)} onChange={e => setPreferences({ ...preferences, favoriteProjectIds: e.target.checked ? [...preferences.favoriteProjectIds, p.id] : preferences.favoriteProjectIds.filter(id => id !== p.id) })} /><span><strong>{p.name}</strong><small>{p.path}</small></span></label>)}</div>
      {preferences.favoriteProjectIds.filter(id => !unique.some(p => p.id === id)).map(id => <label className="collection-missing" key={id}><input type="checkbox" checked disabled={saving} onChange={() => setPreferences({ ...preferences, favoriteProjectIds: preferences.favoriteProjectIds.filter(item => item !== id) })} />Unavailable saved project: {id}</label>)}
      {!visible.length && <p>No discovered projects yet. Use Discover project folders in Settings.</p>}
      <button className="button button-primary" disabled={saving} onClick={() => void save()}>{saving ? "Saving…" : "Save My Projects"}</button></> : <>
      {!visible.length && <div className="empty-state"><h3>{preferences.favoriteProjectIds.length ? "No available projects match" : "Choose your projects"}</h3><p>Your collection only includes projects you select.</p><a href="#settings">Manage My Projects in Settings</a></div>}
      <div className="my-project-grid" role="list" aria-label={manage ? "Project candidates" : "My Projects"}>{visible.map(p => <div role="listitem" key={p.id}><ProjectCard project={p} runs={runs} selected={p.id === selected} onClick={() => { setSelected(p.id); setOpenedIds(ids => ids.includes(p.id) ? ids : [...ids, p.id]); onSelect(p.id); }} onQuickAction={onQuickAction} onRefreshProject={onRefreshProject} refreshing={startupRefreshPending || refreshingPaths.some(path => sameProjectPath(path, p.path))} refreshError={refreshErrorPaths.some(path => sameProjectPath(path, p.path))} refreshVersion={refreshVersions[p.id] ?? 0} /></div>)}</div>
      {unique.filter(project => openedIds.includes(project.id)).map(project => <section key={project.id} hidden={project.id !== selected} className="project-inspector panel" aria-label={`${project.name} details`}><div className="panel-heading"><h2>{project.name}</h2><button className="text-button" onClick={() => setSelected(null)}>Close details</button></div><p>{project.path}</p><h3>Changed files</h3>{project.git.changedFiles == null ? <p>Changed files unavailable. Reopen the app or rediscover folders in Settings to inspect current Git state.</p> : project.git.changedFiles.length ? <ul>{project.git.changedFiles.map(path => <li key={path}><code>{path}</code></li>)}</ul> : <p>Working tree clean.</p>}<div className="project-actions"><a className="button button-secondary" href="#runs">Project actions & activity</a><a className="button button-secondary" href="#batches">Run local issue batch</a></div><h3>Local issues</h3><LocalIssuesPanel key={project.id} defaultPath={project.path} refreshToken={project} /></section>)}
      </>}
    </>}
  </section>;
}

export function ProjectCard({ project, runs, selected, onClick, onQuickAction, onRefreshProject, refreshing = false, refreshError = false, refreshVersion = 0 }: { project: ProjectEntry; runs: WorkflowRun[] | null; selected: boolean; onClick: () => void; onQuickAction?: (id: string, action: "dev-push" | "dev-build", effects?: QuickActionEffects) => void; onRefreshProject?: (path: string) => void; refreshing?: boolean; refreshError?: boolean; refreshVersion?: number }) {
  const batch = useBatchActivity();
  const [icon, setIcon] = useState<string | null>(null);
  const [issues, setIssues] = useState({ epics: "0", stories: "0" });
  const [issueError, setIssueError] = useState("");
  const [gitBusy, setGitBusy] = useState("");
  const [gitMessage, setGitMessage] = useState("");
  const [releaseExecutable, setReleaseExecutable] = useState<ReleaseExecutableVersion | null>(null);
  const [releaseRefresh, setReleaseRefresh] = useState(0);
  const [current, setCurrent] = useState(project);
  const [commitReviewOpen, setCommitReviewOpen] = useState(false);
  const [buildDialog, setBuildDialog] = useState<{ run?: WorkflowRun } | null>(null);
  useEffect(() => setCurrent(project), [project]);
  const latestCompletedBuild = runs?.filter(run => sameProjectPath(run.cwd, current.path) && run.workflow?.action === "dev-build" && run.state === "completed" && run.finishedAtMs !== null)
    .sort((left, right) => (right.finishedAtMs ?? 0) - (left.finishedAtMs ?? 0))[0];
  const buildRefreshKey = latestCompletedBuild ? `${latestCompletedBuild.id}:${latestCompletedBuild.finishedAtMs}` : "";
  useEffect(() => {
    let alive = true;
    if (!isDesktopApp()) { setReleaseExecutable(null); return; }
    getReleaseExecutableVersion(current.id).then(value => { if (alive) setReleaseExecutable(value); })
      .catch(() => { if (alive) setReleaseExecutable(null); });
    return () => { alive = false; };
  }, [current.id, current.path, buildRefreshKey, releaseRefresh, refreshVersion]);
  async function gitAction(action: "pull" | "main") {
    if (gitBusy || !isDesktopApp()) return;
    setGitBusy(action); setGitMessage("");
    try { setCurrent(await runProjectGitAction(project.id, action)); setReleaseRefresh(value => value + 1); setGitMessage(action === "pull" ? "Pulled" : "Switched to main"); }
    catch (error) { setGitMessage(error instanceof Error ? error.message : String(error)); }
    finally { setGitBusy(""); }
  }
  async function openFolder() {
    try { await openProjectFolder(project.id); }
    catch (error) { setGitMessage(error instanceof Error ? error.message : String(error)); }
  }
  useEffect(() => {
    let alive = true;
    setIcon(null);
    if (!isDesktopApp()) return;
    getProjectIcon(project.id).then(value => { if (alive) setIcon(value); }).catch(() => { if (alive) setIcon(null); });
    return () => { alive = false; };
  }, [project.id, project.path]);
  useEffect(() => {
    let alive = true;
    setIssues({ epics: "0", stories: "0" }); setIssueError("");
    if (!isDesktopApp()) { setIssues({ epics: "0", stories: "0" }); return; }
    async function loadCounts() {
      let epics = 0;
      let stories = 0;
      let total = 1;
      for (let offset = 0; offset < total; offset += 100) {
        const page = await issueCommand<IssuePage>("issues-page", project.path, `${project.path}\\runner.json`, { limit: 100, offset });
        if (!alive) return;
        if (page.total > 5000 || page.items.length > 100 || page.offset !== offset || (offset < page.total && !page.items.length)) throw new Error("Invalid or oversized issue page");
        total = page.total;
        for (const issue of page.items) {
          if (issue.type === "epic") epics += 1;
          else stories += 1;
        }
      }
      if (alive) setIssues({ epics: String(epics), stories: String(stories) });
    }
    void loadCounts().catch(e => { if (alive) { setIssues({ epics: "0", stories: "0" }); setIssueError(String(e)); } });
    return () => { alive = false; };
  }, [project]);
  const relevant = runs?.filter(run => sameProjectPath(run.cwd, current.path)) ?? null;
  const projectBusy = relevant?.some(run => ["queued", "running", "needsInput", "cancelling"].includes(run.state)) ?? false;
  const matchingBatch = Boolean(batch?.projectPath && sameProjectPath(batch.projectPath, current.path));
  const batchBusy = matchingBatch && ["queued", "running", "cancelling", "unavailable"].includes(batch?.state ?? "");
  const mutationBusy = projectBusy || batchBusy;
  const actionOptions = { desktop: isDesktopApp(), busy: mutationBusy, gitBusy: Boolean(gitBusy), hasHandler: Boolean(onQuickAction) };
  const pullReason = quickActionReason("pull", current, actionOptions);
  const pushReason = quickActionReason("push", current, actionOptions);
  const supportsGithubPush = current.git.remotes.some(remote => Boolean(remote.github));
  const commitActionLabel = current.git.remotes.length > 0 ? "Commit & Push" : "Commit";
  const pushOnly = current.git.dirty === false && (current.git.upstream.ahead ?? 0) > 0;
  const pushActionLabel = pushOnly ? `Push ${current.git.upstream.ahead} ${current.git.upstream.ahead === 1 ? "commit" : "commits"}` : commitActionLabel;
  const activeBuild = relevant?.find(run => run.workflow?.action === "dev-build" && ["queued", "running", "cancelling"].includes(run.state));
  const buildReason = quickActionReason("build", current, { ...actionOptions, busy: activeBuild ? false : mutationBusy, hasHandler: true });
  const mainReason = quickActionReason("main", current, actionOptions);
  const refreshReason = !isDesktopApp() ? "Requires the desktop app" : !onRefreshProject ? "Refresh is unavailable" : null;
  const openFolderReason = isDesktopApp() ? null : "Requires the desktop app";
  const remoteUrl = current.git.remotes.find(remote => remote.github)?.github?.canonicalUrl;
  const branchLabel = current.git.branch ?? (current.git.status === "detached" ? "Detached HEAD" : current.git.status === "unborn" ? "Unborn branch" : current.git.status === "notRepository" ? "Not a Git repository" : refreshing ? "Checking branch…" : "Branch unavailable");
  const refreshTitle = refreshing ? `Refreshing ${current.name}…` : refreshError ? `Refresh incomplete for ${current.name}; showing last known details` : unavailableTitle("Refresh project", refreshReason);
  const latestCommit = current.git.latestCommit;
  const commitDate = latestCommit?.committedAt?.slice(0, 10) ?? (current.git.status === "unborn" ? "No commits" : "Date unavailable");
  const metadataTitle = [latestCommit ? `${latestCommit.subject} · ${latestCommit.shortHash}` : "Last commit date unavailable", latestCommit?.committedAt, releaseExecutable?.version ? `Release executable: ${releaseExecutable.path}` : null].filter(Boolean).join("\n");
  const versionLabel = releaseExecutable?.version ? `v${releaseExecutable.version.replace(/^v/i, "")}` : "";
  const statusMessage = refreshing ? "Refreshing project details…" : refreshError ? "Refresh incomplete · showing last known details" : gitBusy ? `${gitBusy === "pull" ? "Pulling" : "Switching branch"}…` : gitMessage;
  return <article className={`my-project-card${selected ? " selected" : ""}`} aria-label={`${current.name} project`}>
    <div className="project-card-heading-row"><button className="project-card-open" title={current.name} onClick={onClick} aria-pressed={selected}><ProjectCardHeading name={current.name} icon={icon} /></button><div className="project-card-meta" title={metadataTitle}><span>{commitDate}</span><span className={versionLabel ? "project-card-version" : "project-card-version is-empty"}>{versionLabel || " "}</span></div></div>
    <div className="project-card-branch" title={branchLabel}>{branchLabel}{current.isWorktree ? " · Worktree" : ""}</div>
    <div className="project-card-quick-actions" aria-label={`${current.name} quick actions`}>
        <button className="icon-button project-quick" type="button" title={unavailableTitle("Pull", pullReason)} aria-label={pullReason ? `Pull ${current.name} unavailable: ${pullReason}` : `Pull ${current.name}`} disabled={Boolean(pullReason)} onClick={() => void gitAction("pull")}><svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 1v9m-3-3 3 3 3-3M2 12v2h12v-2" /></svg></button>
        <button className="icon-button project-quick" type="button" title={unavailableTitle(pushActionLabel, pushReason)} aria-label={pushReason ? `${pushActionLabel} ${current.name} unavailable: ${pushReason}` : `${pushActionLabel} ${current.name}`} disabled={Boolean(pushReason)} onClick={() => setCommitReviewOpen(true)}><svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 15V6m-3 3 3-3 3 3M2 4l3-3m-3 3 3 3M14 4l-3-3m3 3-3 3" /></svg></button>
        <button className="icon-button project-quick" type="button" title={unavailableTitle("Build", buildReason)} aria-label={buildReason ? `Build ${current.name} unavailable: ${buildReason}` : `Build ${current.name}`} disabled={Boolean(buildReason)} onClick={() => setBuildDialog({ run: activeBuild })}><svg viewBox="0 0 16 16" aria-hidden="true"><path d="m3 2 10 6-10 6z" /></svg></button>
        <button className="icon-button project-quick project-main" type="button" title={unavailableTitle("Main", mainReason)} aria-label={mainReason ? `Switch ${current.name} to main unavailable: ${mainReason}` : `Switch ${current.name} to main`} disabled={Boolean(mainReason)} onClick={() => void gitAction("main")}>M</button>
        <details className="project-card-menu"><summary className="icon-button project-quick" title="More" aria-label={`More actions for ${current.name}`}>⋯</summary><div className="project-card-menu-pop"><button type="button" title={unavailableTitle("Open folder", openFolderReason)} aria-label={openFolderReason ? `Open ${current.name} folder unavailable: ${openFolderReason}` : `Open ${current.name} folder`} disabled={Boolean(openFolderReason)} onClick={() => void openFolder()}>Open folder</button>{remoteUrl && <a href={remoteUrl} target="_blank" rel="noreferrer">GitHub ↗</a>}</div></details>
        <button className="icon-button project-quick project-card-refresh" type="button" title={refreshTitle} aria-label={refreshing ? `Refreshing ${current.name}` : `Refresh ${current.name}`} aria-busy={refreshing} disabled={refreshing || Boolean(refreshReason)} onClick={() => onRefreshProject?.(current.path)}><svg className={refreshing ? "is-refreshing" : undefined} viewBox="0 0 16 16" aria-hidden="true"><path d="M13.7 6A5.9 5.9 0 0 0 3.3 4.1L2 5.5" /><path d="M2 2v3.5h3.5M2.3 10a5.9 5.9 0 0 0 10.4 1.9l1.3-1.4M14 14v-3.5h-3.5" /></svg></button>
    </div>
    {buildDialog && <BuildProgressDialog project={current} profile={buildDialog.run?.workflow?.buildProfile ?? "release"} initialRun={buildDialog.run} onClose={() => setBuildDialog(null)} />}
    {commitReviewOpen && <CommitReviewDialog project={current} onClose={() => setCommitReviewOpen(false)} />}
    <dl><div><dt>Changes</dt><dd title={String(current.git.changedFiles?.length ?? 0)} className={(current.git.changedFiles?.length ?? 0) > 0 ? "count-changes" : undefined}>{current.git.changedFiles?.length ?? 0}</dd></div><div title={issueError}><dt>Epics</dt><dd title={issues.epics} className={Number(issues.epics) > 0 ? "count-epics" : undefined}>{issues.epics}</dd></div><div title={issueError}><dt>Stories</dt><dd title={issues.stories} className={Number(issues.stories) > 0 ? "count-stories" : undefined}>{issues.stories}</dd></div></dl>
    <span className="project-card-activity" title={activityLabel(relevant)}>{activityLabel(relevant)}{matchingBatch ? ` · Batch ${batch?.needsAttention ? "needs attention" : batch?.state}` : ""}</span>
    <span className={refreshing || gitBusy ? "project-card-action-status" : refreshError ? "project-card-refresh-error" : "project-card-action-status"} title={statusMessage} role={statusMessage ? "status" : undefined}>{statusMessage || " "}</span>
  </article>;
}

export function CommitReviewDialog({ project, onClose }: { project: ProjectEntry; onClose: () => void }) {
  const [context, setContext] = useState<CommitDraftContext | null>(null);
  const [subject, setSubject] = useState("");
  const [body, setBody] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const { run, setRun, running } = useLiveWorkflowRun();
  const started = run !== null;
  const outputRef = useRef<HTMLPreElement>(null);
  const [runError, setRunError] = useState("");
  const [draftRevision, setDraftRevision] = useState(0);
  const dialogRef = useRef<HTMLDialogElement>(null);
  useEffect(() => { const dialog = dialogRef.current; if (dialog && !dialog.open) dialog.showModal(); }, []);
  function closeDialog() { if (dialogRef.current?.open) dialogRef.current.close(); onClose(); }
  useEffect(() => {
    const output = outputRef.current;
    if (output) output.scrollTop = output.scrollHeight;
  }, [run?.nextSequence]);
  useEffect(() => {
    let active = true;
    setLoading(true); setError(""); setContext(null); setSubject(""); setBody("");
    prepareProjectCommitReview(project.id).then(async value => {
      if (!active) return;
      setContext(value);
      if (value.mode === "pushOnly") return;
      try {
        const draft = await draftProjectCommitMessage(project.id, value.snapshotFingerprint);
        if (active) { setSubject(draft.subject); setBody(draft.body); }
      } catch (reason) { if (active) setError(reason instanceof Error ? reason.message : String(reason)); }
    }).catch(reason => { if (active) setError(reason instanceof Error ? reason.message : String(reason)); })
      .finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
  }, [project.id, draftRevision]);
  const actionLabel = context?.mode === "pushOnly" ? `Push ${context.ahead} ${context.ahead === 1 ? "commit" : "commits"}`
    : context?.mode === "commit" ? "Commit" : "Commit & Push";
  async function retryDraft() { setDraftRevision(value => value + 1); }
  async function confirm() {
    if (!context || (context.mode !== "pushOnly" && !subject.trim()) || busy || started) return;
    setBusy(true); setRunError("");
    try {
      const requestedEffects: ("commit" | "push" | "createPullRequest")[] = context.mode === "pushOnly" ? ["push", "createPullRequest"]
        : context.mode === "commit" ? ["commit"] : ["commit", "push", "createPullRequest"];
      const commit = requestedEffects.includes("commit");
      const operationData = JSON.stringify([project.id, context.snapshotFingerprint, context.head, context.mode, subject.trim(), body.trim(), crypto.randomUUID()]);
      const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(operationData));
      const operationKey = Array.from(new Uint8Array(digest), byte => byte.toString(16).padStart(2, "0")).join("");
      const intent = commit ? `${subject.trim()}${body.trim() ? `\n\n${body.trim()}` : ""}` : "Rebase existing commits onto upstream and push";
      const workflow = {
        action: "dev-push" as const, operationKey, intent, issueUrl: null,
        scopePaths: commit ? context.paths : [], requestedEffects, sideEffects: null,
        commitSubject: commit ? subject.trim() : null,
        commitBody: commit ? body.trim() : null,
        draftSnapshotFingerprint: context.snapshotFingerprint,
        autoMergePullRequest: context.mode !== "commit",
      };
      setRun(await startReviewedGitRun(project.id, workflow));
    } catch (reason) { setRunError(reason instanceof Error ? reason.message : String(reason)); }
    finally { setBusy(false); }
  }
  return <dialog ref={dialogRef} className="commit-review-dialog commit-review-workflow" aria-labelledby="commit-review-title" onCancel={event => { event.preventDefault(); if (!busy) closeDialog(); }}>
    <div className="commit-review-heading"><div><p className="eyebrow">{project.name} · {context?.branch ?? project.git.branch ?? "Git"}</p><h2 id="commit-review-title">Review changes</h2></div><button type="button" className="text-button" aria-label="Close review" disabled={busy} onClick={closeDialog}>×</button></div>
    <div className="commit-review-content" aria-busy={loading}>
      <div className="commit-review-summary">
        {context ? <span>{context.mode === "pushOnly" ? `${context.ahead} existing commits` : `${context.paths.length} changed ${context.paths.length === 1 ? "file" : "files"}`}</span>
          : <span className={loading ? "commit-review-skeleton" : ""}>{loading ? "Preparing changes…" : "Changes unavailable"}</span>}
        {loading && <span className="commit-review-loading" role="status"><span className="commit-review-spinner" />{context ? "Drafting…" : "Preparing version…"}</span>}
      </div>
      <div className="commit-review-paths">{context ? context.paths.map(path => <code key={path}>{path}</code>)
        : loading && <><span className="commit-review-skeleton" /><span className="commit-review-skeleton" /><span className="commit-review-skeleton" /></>}</div>
      <label>Message<input autoFocus maxLength={200} value={subject} disabled={loading || busy || started || !context || context.mode === "pushOnly"} placeholder={context?.mode === "pushOnly" ? "Existing commit messages are preserved" : loading ? "Drafting message…" : ""} className={loading ? "commit-review-pending" : ""} onChange={event => setSubject(event.target.value)} /></label>
      <label>Description<textarea rows={4} maxLength={4000} value={body} disabled={loading || busy || started || !context || context.mode === "pushOnly"} placeholder={loading ? "Drafting description…" : ""} className={loading ? "commit-review-pending" : ""} onChange={event => setBody(event.target.value)} /></label>
      {context && context.mode !== "commit" && <p className="commit-review-policy">Rebases before pushing. Opens and merges a PR if required, after checks and reviews. Syncs the local base after merge. Stops on conflicts.</p>}
      {error && <p className="project-card-refresh-error" role="alert">{error}</p>}
      {runError && <p className="project-card-refresh-error" role="alert">{runError}</p>}
    </div>
    <section className="commit-review-progress" aria-label="Commit and push progress">
      <div className="commit-review-progress-heading"><h3>{run ? latestGitStep(run) ?? "Progress" : "Progress"}</h3><span role="status">{busy || running ? <><span className="commit-review-spinner" />{busy ? "Starting…" : "Running…"}</> : run ? run.state === "completed" ? pullRequestMergeStatus(run) ?? "Completed" : `Stopped · ${run.state}` : "Ready when reviewed"}</span></div>
      <pre ref={outputRef} tabIndex={0} aria-label="Commit and push messages">{run ? gitOutputText(run) || "Waiting for output…" : "Commit and push messages will appear here."}</pre>
    </section>
    {run && run.state !== "completed" && pullRequestMergeStatus(run) && <p className="commit-review-policy" role="status">{pullRequestMergeStatus(run)}</p>}
    {run && localSyncStatus(run) && <p className="commit-review-policy" role="status">{localSyncStatus(run)}</p>}
    {run?.workflow?.sideEffects?.pullRequestUrl && <p className="commit-review-policy"><a href={run.workflow.sideEffects.pullRequestUrl} target="_blank" rel="noreferrer">Open pull request and checks ↗</a></p>}
    <div className="dialog-actions">
      {!started && <button className="button button-primary" type="button" disabled={!context || loading || busy || (context.mode !== "pushOnly" && !subject.trim())} onClick={() => void confirm()}>{busy ? "Starting…" : actionLabel}</button>}
      {!started && (error || runError) && !loading && <button className="button button-secondary" type="button" disabled={busy} onClick={() => { setRunError(""); void retryDraft(); }}>Refresh draft</button>}
      <button className="button button-secondary" type="button" disabled={busy} onClick={closeDialog}>{started ? "Close" : "Cancel"}</button>
    </div>
  </dialog>;
}

export function ProjectCardHeading({ name, icon }: { name: string; icon: string | null }) {
  return <div className="my-project-card-heading">{icon && <img className="my-project-icon" src={icon} alt="" />}<h3 title={name}>{name}</h3></div>;
}
