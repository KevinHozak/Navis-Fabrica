import { useEffect, useMemo, useRef, useState } from "react";
import { SettingsPanel } from "./SettingsPanel";
import { ProvidersPanel } from "./ProvidersPanel";
import { GitHubIssuePicker } from "./GitHubIssuePicker";
import { RunsPanel } from "./RunsPanel";
import { displayDirectoryPath } from "./pathDisplay";
import { actionDisabledReason, workflowActions, WorkflowEffect, WorkflowSkillStatus } from "./actions";
import {
  cancelProjectScan,
  discoverWorkflowSkills,
  getCachedGitHubActivity,
  getProjectInventory,
  GitHubActivity,
  isDesktopApp,
  loadSettings,
  onProjectScanFinished,
  onProjectScanInvalidated,
  onProjectDiscoverySettingsChanged,
  onProjectScanProgress,
  onProjectWatchError,
  ProjectEntry,
  GitHubIssue,
  ProjectScanFinished,
  ProjectScanProgress,
  refreshGitHubActivity,
  startProjectScan,
  startAffectedProjectRefresh,
} from "./settings";

type Section = "projects" | "runs" | "providers" | "settings";
type ScanStatus = "idle" | "scanning" | "cancelling" | "complete" | "cancelled" | "error";
type InventoryFreshness = "loading" | "saved" | "partial" | "refreshing" | "current" | "stale";

const sections: { id: Section; label: string; icon: string }[] = [
  { id: "projects", label: "Projects", icon: "◫" },
  { id: "runs", label: "Runs", icon: "↗" },
  { id: "providers", label: "Providers", icon: "◇" },
  { id: "settings", label: "Settings", icon: "⚙" },
];

function sectionFromHash(): Section {
  const candidate = window.location.hash.slice(1);
  return sections.some((section) => section.id === candidate) ? (candidate as Section) : "projects";
}

function App() {
  const [section, setSection] = useState<Section>(sectionFromHash);
  const [scanStatus, setScanStatus] = useState<ScanStatus>("idle");
  const [scanListenersReady, setScanListenersReady] = useState(false);
  const [scanId, setScanId] = useState("");
  const [progress, setProgress] = useState<ProjectScanProgress | null>(null);
  const [projects, setProjects] = useState<ProjectEntry[]>([]);
  const [selectedProjectId, setSelectedProjectId] = useState<string | null>(null);
  const [scanResult, setScanResult] = useState<ProjectScanFinished | null>(null);
  const [scanDurationMs, setScanDurationMs] = useState<number | null>(null);
  const [firstProjectMs, setFirstProjectMs] = useState<number | null>(null);
  const [scanError, setScanError] = useState("");
  const [watchError, setWatchError] = useState("");
  const [inventoryFreshness, setInventoryFreshness] = useState<InventoryFreshness>("loading");
  const [projectRoot, setProjectRoot] = useState("C:\\Projects");
  const [workContext, setWorkContext] = useState<{ projectId: string; issue: GitHubIssue } | null>(null);
  const [workflowSkills, setWorkflowSkills] = useState<WorkflowSkillStatus[]>([]);
  const activeScanId = useRef("");
  const scanStartedAt = useRef<number | null>(null);
  const firstProjectMsRef = useRef<number | null>(null);
  const cancelRequested = useRef(false);
  const refreshAfterScan = useRef<string[]>([]);
  const fullRefreshAfterScan = useRef(false);
  const partialScanIds = useRef(new Set<string>());
  const beginScanRef = useRef<(forceRefresh: boolean, maxDirectories?: number) => void>(() => undefined);
  const beginAffectedRefreshRef = useRef<(paths: string[]) => void>(() => undefined);
  const desktop = isDesktopApp();

  useEffect(() => {
    const handleNavigation = () => {
      setSection(sectionFromHash());
    };
    window.addEventListener("hashchange", handleNavigation);
    return () => window.removeEventListener("hashchange", handleNavigation);
  }, []);

  useEffect(() => {
    if (!desktop) return;
    let active = true;
    discoverWorkflowSkills().then((skills) => {
      if (active) setWorkflowSkills(skills);
    }).catch(() => {
      if (active) setWorkflowSkills(["dev-work", "dev-push", "dev-build"].map((id) => ({ id: id as WorkflowSkillStatus["id"], status: "unavailable", compatibleAgents: [] })));
    });
    return () => { active = false; };
  }, [desktop]);

  useEffect(() => {
    document.title = `Navis-Fabrica · v${__APP_VERSION__}`;
    loadSettings().then((settings) => {
      setProjectRoot(settings.projectRoot);
    }).catch(() => undefined);
  }, []);

  useEffect(() => {
    if (!desktop) return;
    let active = true;
    let unlistenProgress: (() => void) | undefined;
    let unlistenFinished: (() => void) | undefined;
    let unlistenInvalidated: (() => void) | undefined;
    let unlistenWatchError: (() => void) | undefined;
    let readyListeners = 0;
    const markScanListenerReady = () => {
      readyListeners += 1;
      if (active && readyListeners === 2) setScanListenersReady(true);
    };

    onProjectScanProgress((next) => {
      if (!active || next.scanId !== activeScanId.current) return;
      setProgress(next);
      if (next.projectsFound > 0 && firstProjectMsRef.current === null && scanStartedAt.current !== null) {
        const elapsed = Math.round(performance.now() - scanStartedAt.current);
        firstProjectMsRef.current = elapsed;
        setFirstProjectMs(elapsed);
      }
    }).then((stop) => {
      if (active) unlistenProgress = stop;
      else stop();
      markScanListenerReady();
    }).catch((error: unknown) => {
      if (active) {
        setScanStatus("error");
        setScanError(error instanceof Error ? error.message : String(error));
      }
    });
    onProjectScanFinished((result) => {
      if (!active || result.scanId !== activeScanId.current) return;
      activeScanId.current = "";
      if (scanStartedAt.current !== null) {
        setScanDurationMs(Math.round(performance.now() - scanStartedAt.current));
        scanStartedAt.current = null;
      }
      setScanResult(result);
      if (!result.fatalError || result.projects.length > 0) {
        setProjects((previous) => {
          if (!result.errors.length && !result.cancelled) return result.projects;
          const merged = new Map(previous.map((project) => [project.id, project]));
          result.projects.forEach((project) => merged.set(project.id, project));
          return [...merged.values()];
        });
      }
      setScanError(result.fatalError ?? "");
      setScanStatus(result.fatalError ? "error" : result.cancelled ? "cancelled" : "complete");
      const partial = partialScanIds.current.delete(result.scanId);
      setInventoryFreshness(result.fatalError || result.errors.length ? "stale" : result.cancelled ? "saved" : partial ? "partial" : "current");
      if (!result.cancelled && fullRefreshAfterScan.current) {
        fullRefreshAfterScan.current = false;
        refreshAfterScan.current = [];
        window.setTimeout(() => beginScanRef.current(true), 0);
      } else if (!result.cancelled && refreshAfterScan.current.length) {
        const pending = [...new Set(refreshAfterScan.current)];
        refreshAfterScan.current = [];
        window.setTimeout(() => beginAffectedRefreshRef.current(pending), 0);
      }
    }).then((stop) => {
      if (active) unlistenFinished = stop;
      else stop();
      markScanListenerReady();
    }).catch((error: unknown) => {
      if (active) {
        setScanStatus("error");
        setScanError(error instanceof Error ? error.message : String(error));
      }
    });

    onProjectScanInvalidated((paths) => {
      if (activeScanId.current) {
        if (partialScanIds.current.has(activeScanId.current)) {
          refreshAfterScan.current.push(...paths.slice(0, 512));
        } else {
          fullRefreshAfterScan.current = true;
        }
      } else beginAffectedRefreshRef.current(paths);
    }).then((stop) => {
      if (active) unlistenInvalidated = stop;
      else stop();
    }).catch((error: unknown) => {
      if (active) setWatchError(error instanceof Error ? error.message : String(error));
    });
    onProjectWatchError((message) => {
      if (active) {
        setWatchError(message);
        if (message) setInventoryFreshness("stale");
      }
    }).then((stop) => {
      if (active) unlistenWatchError = stop;
      else stop();
    }).catch((error: unknown) => {
      if (active) setWatchError(error instanceof Error ? error.message : String(error));
    });

    return () => {
      active = false;
      unlistenProgress?.();
      unlistenFinished?.();
      unlistenInvalidated?.();
      unlistenWatchError?.();
    };
  }, [desktop]);

  useEffect(() => {
    if (!desktop) return;
    const unlisten = onProjectDiscoverySettingsChanged(() => {
      if (activeScanId.current) fullRefreshAfterScan.current = true;
      else beginScanRef.current(true);
    });
    return () => { void unlisten.then((stop) => stop()); };
  }, [desktop]);

  useEffect(() => {
    if (!desktop || !scanListenersReady) return;
    let active = true;
    getProjectInventory()
      .catch((error: unknown) => {
        if (active) {
          setInventoryFreshness("stale");
          setScanError(error instanceof Error ? error.message : String(error));
        }
        return null;
      })
      .then((savedProjects) => {
        if (!active || !savedProjects) return;
        if (activeScanId.current) return;
        setProjects(savedProjects);
        setInventoryFreshness("saved");
      });
    return () => { active = false; };
  }, [desktop, scanListenersReady]);

  useEffect(() => {
    if (!desktop || !scanListenersReady) return;
    const startupRefresh = window.setTimeout(() => beginScanRef.current(false, 2_000), 1_500);
    const fallbackRefresh = window.setInterval(() => beginScanRef.current(true, 20_000), 15 * 60 * 1_000);
    return () => {
      window.clearTimeout(startupRefresh);
      window.clearInterval(fallbackRefresh);
    };
  }, [desktop, scanListenersReady]);

  useEffect(() => {
    setSelectedProjectId((selected) => projects.some((project) => project.id === selected) ? selected : projects[0]?.id ?? null);
  }, [projects]);

  const current = sections.find((item) => item.id === section) ?? sections[0];
  const selectedProject = projects.find((project) => project.id === selectedProjectId) ?? null;
  const workIssue = workContext && selectedProject?.id === workContext.projectId ? workContext.issue : null;
  const collectionTitle = section === "projects" ? "Your projects" : section === "runs" ? "Work context" : "Connected providers";
  const scanning = scanStatus === "scanning" || scanStatus === "cancelling";
  const freshnessLabel = inventoryFreshness === "loading" ? "Loading saved projects…"
    : inventoryFreshness === "saved" ? "Saved project list · background refresh is scheduled"
      : inventoryFreshness === "partial" ? "Saved project list · changed repositories refreshed"
      : inventoryFreshness === "refreshing" ? "Refreshing project details in the background…"
        : inventoryFreshness === "stale" ? "Showing saved project details · refresh needed"
          : "Project list is up to date";

  async function beginScan(forceRefresh = true, maxDirectories?: number) {
    if (activeScanId.current) {
      if (forceRefresh) fullRefreshAfterScan.current = true;
      return;
    }
    if (!desktop) {
      setScanStatus("error");
      setScanError("Project scanning is available in the Navis-Fabrica desktop app.");
      return;
    }
    const id = crypto.randomUUID();
    activeScanId.current = id;
    cancelRequested.current = false;
    setScanId(id);
    setScanStatus("scanning");
    setInventoryFreshness("refreshing");
    setScanResult(null);
    setProgress(null);
    setScanDurationMs(null);
    setFirstProjectMs(null);
    firstProjectMsRef.current = null;
    setScanError("");
    try {
      const settings = await loadSettings();
      setProjectRoot(settings.projectRoot);
      scanStartedAt.current = performance.now();
      await startProjectScan(id, forceRefresh, maxDirectories);
      if (cancelRequested.current) await cancelProjectScan(id);
    } catch (error) {
      activeScanId.current = "";
      setScanStatus("error");
      setInventoryFreshness("stale");
      setScanError(error instanceof Error ? error.message : String(error));
    }
  }

  async function beginAffectedRefresh(paths: string[]) {
    if (!desktop || !paths.length) return;
    if (activeScanId.current) {
      refreshAfterScan.current.push(...paths.slice(0, 512));
      return;
    }
    const id = crypto.randomUUID();
    partialScanIds.current.add(id);
    activeScanId.current = id;
    cancelRequested.current = false;
    setScanId(id);
    setScanStatus("scanning");
    setInventoryFreshness("refreshing");
    setScanResult(null);
    setProgress(null);
    setScanError("");
    try {
      await startAffectedProjectRefresh(id, paths.slice(0, 512));
    } catch (error) {
      activeScanId.current = "";
      partialScanIds.current.delete(id);
      setScanStatus("error");
      setInventoryFreshness("stale");
      setScanError(error instanceof Error ? error.message : String(error));
    }
  }

  async function stopScan() {
    if (!scanId) return;
    setScanStatus("cancelling");
    try {
      const accepted = await cancelProjectScan(scanId);
      if (!accepted) cancelRequested.current = true;
    } catch (error) {
      setScanStatus("scanning");
      setScanError(error instanceof Error ? error.message : String(error));
    }
  }

  beginScanRef.current = (forceRefresh, maxDirectories) => { void beginScan(forceRefresh, maxDirectories); };
  beginAffectedRefreshRef.current = (paths) => { void beginAffectedRefresh(paths); };

  return (
    <div className="app-shell">
      <a className="skip-link" href="#main-content" onClick={(event) => { event.preventDefault(); document.getElementById("main-content")?.focus(); }}>Skip to content</a>
      <aside className="sidebar" aria-label="Application navigation">
        <a className="brand" href="#projects" aria-label="Navis-Fabrica home">
          <span className="brand-mark" aria-hidden="true">N</span>
          <span className="brand-copy"><strong>navis</strong><span>FABRICA</span></span>
        </a>

        <div className="workspace-label">WORKSPACE</div>
        <nav className="primary-nav" aria-label="Main navigation">
          {sections.map((item) => (
            <a key={item.id} className={`nav-link${section === item.id ? " active" : ""}`} href={`#${item.id}`} aria-current={section === item.id ? "page" : undefined}>
              <span className="nav-icon" aria-hidden="true">{item.icon}</span>{item.label}
              {section === item.id && <span className="nav-indicator" aria-hidden="true" />}
            </a>
          ))}
        </nav>

        <div className="sidebar-bottom">
          <div className="local-card">
            <span className="local-dot" aria-hidden="true" />
            <div><strong>Local workspace</strong><span>On this device</span></div>
          </div>
          <div className="sidebar-footer"><span className="version-mark">NF</span><span>Navis-Fabrica</span><span className="version-number">v{__APP_VERSION__}</span></div>
        </div>
      </aside>

      <div className="main-column">
        <header className="toolbar">
          <div className="breadcrumb"><span>Workspace</span><span className="breadcrumb-slash">/</span><strong>{current.label}</strong></div>
          <div className="toolbar-actions"><span className="toolbar-divider" /><button className="avatar" type="button" aria-label="Local profile">K</button></div>
        </header>

        <main id="main-content" className="content" tabIndex={-1}>
          <div className="page-heading">
            <div>
              <p className="eyebrow">YOUR WORKSPACE</p>
              <h1>{section === "projects" ? "Projects" : current.label}</h1>
              <p className="page-subtitle">{section === "projects" ? "A clear view of the work taking shape." : section === "settings" ? "Make this workspace feel like yours." : section === "providers" ? "Configure manual choices and inspect known provider capabilities." : `${current.label} will live here as the foundation grows.`}</p>
            </div>
            {section === "projects" && <button className="button button-primary" type="button" onClick={scanning ? stopScan : () => { void beginScan(); }} disabled={scanStatus === "cancelling"}>
              <span aria-hidden="true">{scanning ? "×" : "↻"}</span> {scanning ? "Cancel scan" : projects.length ? "Refresh projects" : "Scan projects"}
            </button>}
          </div>

          {section === "settings" ? (
            <SettingsPanel />
          ) : section === "providers" ? (
            <ProvidersPanel />
          ) : section === "runs" ? selectedProject ? isDesktopApp() ? (
            <RunsPanel defaultCwd={selectedProject.path} project={selectedProject} issue={workIssue} />
          ) : (
            <div className="panel empty-state"><h3>Workflow execution needs the desktop app</h3><p>The browser preview can inspect project context, but it cannot launch local processes.</p></div>
          ) : (
            <section className="panel project-panel" aria-labelledby="project-list-title">
              <div className="panel-heading"><div><h2 id="project-list-title">Work context</h2><p>Issue details selected from a repository. Selection never edits GitHub.</p></div></div><div className="panel-rule" />
              {workIssue ? <div className="work-context"><p className="eyebrow">READY FOR WORK</p><h3><a href={workIssue.url} target="_blank" rel="noreferrer">#{workIssue.number} · {workIssue.title} ↗</a></h3><p>{workIssue.body}</p>{workIssue.acceptanceCriteria.length > 0 && <div><strong>Acceptance criteria</strong><ul>{workIssue.acceptanceCriteria.map((criterion, index) => <li key={`${index}-${criterion}`}>{criterion}</li>)}</ul></div>}{workIssue.blockedBy.length > 0 && <div><strong>Blockers</strong>{workIssue.blockedBy.map((blocker) => <a key={blocker.number} href={blocker.url} target="_blank" rel="noreferrer">#{blocker.number} · {blocker.title} ({blocker.state})</a>)}</div>}{workIssue.parentEpic && <div><strong>Parent epic</strong><a href={workIssue.parentEpic.url} target="_blank" rel="noreferrer">#{workIssue.parentEpic.number} · {workIssue.parentEpic.title}</a></div>}{workIssue.pullRequestUrls.map((url) => <a key={url} href={url} target="_blank" rel="noreferrer">PR context: {url}</a>)}<small>Bounded read-only context · {workIssue.acceptanceCriteria.length} acceptance criteria · {workIssue.blockedBy.length} blockers</small></div> : <div className="empty-state"><h3>No issue context yet</h3><p>Choose an issue in a project’s GitHub activity panel to prepare its work context.</p><a href="#projects">Browse projects</a></div>}
              {!workIssue && workContext && <p className="registry-context-note">The selected issue belongs to another project. Choose an issue from the selected project to prepare work.</p>}
              <WorkflowRegistry project={selectedProject} issue={workIssue} skills={workflowSkills} />
            </section>
          ) : (
            <section className="panel project-panel" aria-labelledby="project-list-title">
              <div className="panel-heading panel-heading-row">
                <div><h2 id="project-list-title">{collectionTitle}</h2><p>{section === "projects" ? `Workspace: ${displayDirectoryPath(projectRoot)}` : "This workspace section is part of the foundation shell."}</p></div>
                <span className="project-count" aria-live="polite">{section === "projects" ? projects.length : ""}</span>
              </div>
              <div className="panel-rule" />

              {section === "projects" && <p className="scan-measurement" role="status" aria-live="polite">{freshnessLabel}</p>}

              {section === "projects" && scanResult && scanDurationMs !== null && <p className="scan-measurement" role="status" aria-live="polite">Last scan: {scanDurationMs} ms · {scanResult.directoriesScanned} folders · {scanResult.projects.length} projects{firstProjectMs === null ? "." : ` · first project in ${firstProjectMs} ms.`}</p>}

              {watchError && section === "projects" && <div className="scan-notice">Automatic project refresh is unavailable: {watchError} <span>Use Refresh projects to update the list.</span></div>}

              {section !== "projects" ? (
                <div className="empty-state"><h3>A little room to grow</h3><p>This section is ready for a future phase.</p></div>
              ) : projects.length > 0 ? (
                <>
                  {scanning && <div className="scan-progress" role="status"><span className="spinner" aria-hidden="true" /><span>{scanStatus === "cancelling" ? "Stopping the scan" : progress ? `Refreshing · scanned ${progress.directoriesScanned} folders · found ${progress.projectsFound} projects` : "Refreshing projects…"}</span></div>}
                  {scanStatus === "error" && <div className="scan-notice error-notice" role="alert"><strong>Refresh could not complete.</strong> {scanError} Showing the last available results. <button className="text-button" type="button" onClick={() => { void beginScan(); }}>Try again</button></div>}
                  {scanResult?.errors.length ? <details className="scan-notice"><summary>Some folders could not be inspected ({scanResult.errors.length})</summary><ul>{scanResult.errors.map((error, index) => <li key={`${index}-${error}`}>{error}</li>)}</ul><p>Showing available results.</p></details> : null}
                  {scanStatus === "cancelled" && <div className="scan-notice" role="status">Scan cancelled. Showing available results.</div>}
                  <ProjectsBrowser projects={projects} selectedProjectId={selectedProjectId} onSelect={setSelectedProjectId} onWork={(issue, projectId) => { setWorkContext({ projectId, issue }); window.location.hash = "runs"; }} />
                </>
              ) : scanning ? (
                <div className="state-box loading-state" role="status"><span className="spinner" aria-hidden="true" /><div><strong>{scanStatus === "cancelling" ? "Stopping the scan" : "Finding your projects"}</strong><p>{progress ? `Scanned ${progress.directoriesScanned} folders · found ${progress.projectsFound} projects` : "Preparing your workspace scan…"}</p></div></div>
              ) : scanStatus === "error" ? (
                <div className="state-box error-state" role="alert"><span className="state-symbol" aria-hidden="true">!</span><div><strong>We couldn’t scan this folder</strong><p>{scanError}</p><button className="text-button" type="button" onClick={() => { void beginScan(); }}>Try again</button></div></div>
              ) : (
                <div className="empty-state">
                  <div className="empty-illustration" aria-hidden="true"><span className="orbit orbit-one" /><span className="orbit orbit-two" /><span className="orbit orbit-three" /><span className="empty-spark spark-one">✦</span><span className="empty-spark spark-two">·</span><span className="folder-shape"><span /></span></div>
                  <h3>{scanStatus === "complete" ? scanResult?.errors.length ? "Scan partly complete" : "No projects found" : scanStatus === "cancelled" ? "Scan cancelled" : "Ready when you are"}</h3>
                  <p>{scanStatus === "complete" ? scanResult?.errors.length ? `Some folders under ${displayDirectoryPath(projectRoot)} could not be inspected.` : `No coding projects were found under ${displayDirectoryPath(projectRoot)}.` : scanStatus === "cancelled" ? `The scan stopped after checking ${scanResult?.directoriesScanned ?? 0} folders.` : "Scan your configured workspace to find coding projects and repositories."}</p>
                  {scanResult?.errors.length ? <details className="scan-notice"><summary>Show scan issues ({scanResult.errors.length})</summary><ul>{scanResult.errors.map((error, index) => <li key={`${index}-${error}`}>{error}</li>)}</ul></details> : null}
                  {scanStatus === "idle" && <button className="button button-secondary" type="button" onClick={() => { void beginScan(); }}>↻ Scan workspace</button>}
                </div>
              )}
            </section>
          )}

          <footer className="content-footer"><span className="footer-orbit" aria-hidden="true">✳</span><span>Built for steady progress.</span><span className="footer-version">v{__APP_VERSION__}</span></footer>
        </main>
      </div>
    </div>
  );
}

export default App;

function WorkflowRegistry({ project, issue, skills }: {
  project: ProjectEntry | null;
  issue: GitHubIssue | null;
  skills: WorkflowSkillStatus[];
}) {
  return <section className="workflow-registry" aria-labelledby="workflow-registry-title">
    <div className="workflow-registry-heading">
      <div><p className="eyebrow">PROJECT ACTIONS</p><h3 id="workflow-registry-title">Workflow registry</h3></div>
      <span className="registry-phase">Registry only</span>
    </div>
    <p className="workflow-registry-intro">Actions describe their inputs, checks, and outputs. Skill files are discovered by reference and never executed as Markdown.</p>
    {!project && <p className="registry-context-note">Select a project in Projects to check stack and tool compatibility.</p>}
    {workflowActions.map((action) => {
      const disabledReason = actionDisabledReason(action, project, issue, skills);
      const skill = skills.find((entry) => entry.id === action.id);
      return <article className="workflow-action" key={action.id}>
        <div className="workflow-action-top">
          <div><h4>{action.title}</h4><p>{action.summary}</p></div>
          <button className="button button-secondary workflow-run-button" type="button" disabled aria-describedby={`${action.id}-disabled`}>Prepare</button>
        </div>
        <p className="workflow-disabled-reason" id={`${action.id}-disabled`}>{disabledReason}</p>
        <div className="workflow-meta-grid">
          <div><strong>Registry entry</strong><span>v{action.version} · {action.skillReference} · {skill?.status ?? "checking"}{skill?.status === "available" && skill.compatibleAgents.length ? ` · ${skill.compatibleAgents.join(", ")}` : ""}</span></div>
          <div><strong>Supported stacks</strong><span>{action.supportedStacks === "all" ? "Any detected stack" : action.supportedStacks.join(", ")}</span></div>
          {action.defaultBuildProfile && <div><strong>Build default</strong><span>{action.defaultBuildProfile}</span></div>}
        </div>
        <details className="workflow-details"><summary>Inputs, checks, effects, and outputs</summary>
          <div className="workflow-detail-columns">
            <div><strong>Inputs</strong><ul>{action.inputs.map((input) => <li key={input.id}>{input.label}{input.required ? " · required" : " · optional"}{input.defaultValue ? ` · default ${input.defaultValue}` : ""}{input.options ? ` · ${input.options.join(" / ")}` : ""}</li>)}</ul></div>
            <div><strong>Checks</strong><ul>{action.checks.map((check) => <li key={check}>{check}</li>)}</ul></div>
            <div><strong>Effect scope</strong><ul>
              {(["commit", "push", "createPullRequest"] as WorkflowEffect[]).map((effect) => <li key={effect}>{effect === "createPullRequest" ? "Create PR" : effect} · {action.effectScope.permitted.includes(effect) ? "requires explicit opt-in" : "not permitted"}</li>)}
              <li>Requested by default · {action.effectScope.defaultRequested.length ? action.effectScope.defaultRequested.join(", ") : "none"}</li>
            </ul></div>
            <div><strong>Expected artifacts</strong><ul>{action.outputArtifacts.map((artifact) => <li key={artifact}>{artifact}</li>)}</ul></div>
          </div>
        </details>
      </article>;
    })}
  </section>;
}

function gitStatusLabel(status: string): string {
  switch (status) {
    case "detached": return "Git · Detached HEAD";
    case "unborn": return "Git · No commits yet";
    case "unsafeOwnership": return "Git · Ownership needs attention";
    case "gitUnavailable": return "Git · Git executable unavailable";
    case "notRepository": return "Git · Repository metadata unavailable";
    default: return "Git · Inspection unavailable";
  }
}

function ProjectsBrowser({ projects, selectedProjectId, onSelect, onWork }: {
  projects: ProjectEntry[];
  selectedProjectId: string | null;
  onSelect: (id: string) => void;
  onWork: (issue: GitHubIssue, projectId: string) => void;
}) {
  const [query, setQuery] = useState("");
  const [stackFilter, setStackFilter] = useState("all");
  const stacks = useMemo(() => [...new Set(projects.flatMap((project) => project.detectedStacks))].sort(), [projects]);
  const visible = useMemo(() => {
    const normalized = query.trim().toLocaleLowerCase();
    return projects.filter((project) =>
      (stackFilter === "all" || project.detectedStacks.includes(stackFilter)) &&
      (!normalized || `${project.name} ${project.path} ${project.detectedStacks.join(" ")}`.toLocaleLowerCase().includes(normalized)),
    );
  }, [projects, query, stackFilter]);
  const selected = projects.find((project) => project.id === selectedProjectId) ?? null;

  return <div className="project-browser">
    <div className="project-browser-controls">
      <label className="project-search"><span className="sr-only">Search projects</span><span aria-hidden="true">⌕</span><input type="search" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search projects" /></label>
      <label className="project-filter"><span className="sr-only">Filter by stack</span><select value={stackFilter} onChange={(event) => setStackFilter(event.target.value)}><option value="all">All stacks</option>{stacks.map((stack) => <option key={stack} value={stack}>{stack}</option>)}</select></label>
      <span className="project-count" aria-live="polite">{visible.length} of {projects.length}</span>
    </div>
    <div className="project-browser-results">
      {visible.length ? <div className="project-card-grid" aria-label="Projects" role="list">
        {visible.map((project) => <article className={`project-card${selectedProjectId === project.id ? " selected" : ""}`} role="listitem" key={project.id}>
          <button className="project-card-select" type="button" aria-pressed={selectedProjectId === project.id} onClick={() => onSelect(project.id)}>
            <span className="project-card-heading">
              <span className="project-symbol" aria-hidden="true">{project.isGitRepository ? "⌘" : "▣"}</span>
              <span className="project-card-title"><strong>{project.name}</strong><span className="project-card-path">{displayDirectoryPath(project.path)}</span></span>
              <span className="project-card-selection" aria-hidden="true">{selectedProjectId === project.id ? "Selected" : "Select"}</span>
            </span>
            <span className="project-card-facts">
              <span className="project-card-fact"><span>Branch</span><strong>{project.isGitRepository ? project.git.status === "available" ? project.git.branch || "Unavailable" : gitStatusLabel(project.git.status) : "Not a Git repository"}</strong></span>
              <span className={`project-card-tree${project.git.dirty === false ? " clean" : project.git.dirty === true ? " dirty" : " unknown"}`}>
                <span className="project-card-status-dot" aria-hidden="true" />
                {project.isGitRepository ? project.git.dirty === false ? "Working tree clean" : project.git.dirty === true ? "Uncommitted changes" : "Working tree status unavailable" : "Not a Git repository"}
              </span>
              <span className="project-card-fact"><span>Latest commit</span><strong>{project.git.latestCommit ? <><code>{project.git.latestCommit.shortHash}</code> · {project.git.latestCommit.subject}</> : "Unavailable"}</strong></span>
              <span className="project-card-stacks"><span className="project-card-fact-label">Detected stacks</span>{project.detectedStacks.length ? project.detectedStacks.map((stack) => <span className="project-tag" key={stack}>{stack}</span>) : <span className="project-card-unavailable">None detected</span>}</span>
            </span>
          </button>
        </article>)}
      </div> : <div className="project-filter-empty">No projects match these filters.</div>}
      <ProjectDetails project={selected} onWork={(issue) => { if (selected) onWork(issue, selected.id); }} />
    </div>
  </div>;
}
function ProjectDetails({ project, onWork }: { project: ProjectEntry | null; onWork: (issue: GitHubIssue) => void }) {
  const [activity, setActivity] = useState<GitHubActivity | null>(null);
  const [activityLoading, setActivityLoading] = useState(false);
  const [activityError, setActivityError] = useState("");
  const remote = project?.git.remotes.find((item) => item.github)?.github;
  const selectedRepository = useRef<string | null>(null);
  selectedRepository.current = project && remote
    ? `${project.id}:${remote.owner.toLowerCase()}/${remote.repository.toLowerCase()}`
    : null;
  useEffect(() => {
    let active = true;
    setActivity(null);
    setActivityError("");
    if (remote && isDesktopApp()) {
      getCachedGitHubActivity(remote.owner, remote.repository).then((value) => {
        if (active) setActivity(value);
      }).catch((error: unknown) => {
        if (active) setActivityError(error instanceof Error ? error.message : String(error));
      });
    }
    return () => { active = false; };
  }, [project?.id, remote?.owner, remote?.repository]);

  async function refreshActivity() {
    if (!remote) return;
    const requestRepository = selectedRepository.current;
    setActivityLoading(true);
    setActivityError("");
    try {
      const refreshedActivity = await refreshGitHubActivity(remote.owner, remote.repository);
      if (selectedRepository.current === requestRepository) setActivity(refreshedActivity);
    } catch (error) {
      if (selectedRepository.current === requestRepository) {
        setActivityError(error instanceof Error ? error.message : String(error));
      }
    } finally {
      if (selectedRepository.current === requestRepository) setActivityLoading(false);
    }
  }

  if (!project) return <aside className="project-detail empty-detail"><h3>Select a project</h3><p>Choose a project to see its local details.</p></aside>;
  const git = project.git;
  return <aside className="project-detail" aria-labelledby="project-detail-title">
    <p className="eyebrow">PROJECT DETAILS</p><h3 id="project-detail-title">{project.name}</h3><p className="detail-path">{displayDirectoryPath(project.path)}</p>
    <div className="detail-group"><strong>Repository</strong><span>{project.isGitRepository ? git.status === "available" ? git.branch ? `Branch ${git.branch}` : "Branch unavailable" : gitStatusLabel(git.status) : "Not a Git repository"}</span>
      {project.isGitRepository && git.dirty !== null && <span>{git.dirty ? "Uncommitted changes" : "Working tree clean"}</span>}
      {git.latestCommit && <span>{git.latestCommit.shortHash} · {git.latestCommit.subject}</span>}
      {git.upstream.status === "tracking" && <span>Upstream · ↑{git.upstream.ahead ?? 0} ↓{git.upstream.behind ?? 0}</span>}
      {git.upstream.status === "noUpstream" && <span>No upstream configured</span>}
      {git.upstream.status === "unavailable" && git.status === "available" && <span>Upstream status unavailable</span>}
    </div>
    <div className="detail-group"><strong>Detected stacks</strong><div className="project-detail-tags">{project.detectedStacks.length ? project.detectedStacks.map((stack) => <span className="project-tag" key={stack}>{stack}</span>) : <span>None detected</span>}</div></div>
    {git.remotes.length > 0 && <div className="detail-group"><strong>Remotes</strong>{git.remotes.map((remote) => <span className="detail-break" key={remote.name}>{remote.name}{remote.url ? ` · ${remote.url}` : " · URL unavailable"}</span>)}</div>}
    {remote && <div className="detail-group github-activity">
      <strong>GitHub activity</strong>
      <a href={remote.canonicalUrl} target="_blank" rel="noreferrer">{remote.owner}/{remote.repository} ↗</a>
      <span>Remote access · {activity?.authentication === "available" ? "authenticated" : activity?.authentication === "expired" ? "authentication expired" : "public GitHub API"}</span>
      <div className="github-activity-counts">
        <a href={activity?.issuesUrl ?? `${remote.canonicalUrl}/issues`} target="_blank" rel="noreferrer">{activity?.issueCount ?? "—"} open issues</a>
        <a href={activity?.pullsUrl ?? `${remote.canonicalUrl}/pulls`} target="_blank" rel="noreferrer">{activity?.pullRequestCount ?? "—"} open pull requests</a>
      </div>
      <span className="github-freshness">Remote · {activity?.remoteStatus === "fresh" ? "updated" : activity?.remoteStatus === "rateLimited" ? "rate limited" : activity?.remoteStatus === "unauthorized" ? "access unavailable" : activity?.remoteStatus === "authExpired" ? "authentication expired" : activity?.remoteStatus === "unavailable" ? "unavailable" : "not refreshed"}{activity?.checkedAt ? ` · ${new Date(Number(activity.checkedAt) * 1000).toLocaleString()}` : ""}</span>
      {activity?.authentication === "expired" && <span role="status">GitHub rejected authentication. Public repository links and local project details are still available.</span>}
      {activity?.remoteStatus === "rateLimited" && <span role="status">GitHub rate limit reached. Try again later.</span>}
      {activity?.remoteStatus === "unauthorized" && <span role="status">GitHub did not provide repository activity. Private data stays hidden.</span>}
      {activity?.remoteStatus === "unavailable" && <span role="status">Remote activity is unavailable. Local project details are still available.</span>}
      {activity?.recentCommits.map((commit) => <a className="github-commit" key={commit.hash} href={commit.url} target="_blank" rel="noreferrer"><code>{commit.hash}</code> · {commit.subject}</a>)}
      {isDesktopApp() && <GitHubIssuePicker key={`${remote.owner.toLowerCase()}/${remote.repository.toLowerCase()}`} owner={remote.owner} repository={remote.repository} onWork={onWork} />}
      {activityError && <span role="status">{activityError}</span>}
      {isDesktopApp() && <button className="text-button" type="button" onClick={() => { void refreshActivity(); }} disabled={activityLoading}>{activityLoading ? "Refreshing GitHub…" : "Refresh GitHub activity"}</button>}
    </div>}
    {project.workflowPrerequisites.length > 0 && <div className="detail-group"><strong>Workflow prerequisites</strong>{project.workflowPrerequisites.map((tool) => <span key={tool.name}>{tool.name} · {tool.available ? "available" : "missing"}</span>)}</div>}
  </aside>;
}
