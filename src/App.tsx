import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import { SettingsPanel } from "./SettingsPanel";
import { ProvidersPanel } from "./ProvidersPanel";
import {
  cancelProjectScan,
  getCachedGitHubActivity,
  GitHubActivity,
  isDesktopApp,
  loadSettings,
  onProjectScanFinished,
  onProjectScanInvalidated,
  onProjectScanProgress,
  onProjectWatchError,
  ProjectEntry,
  ProjectScanFinished,
  ProjectScanProgress,
  refreshGitHubActivity,
  startProjectScan,
} from "./settings";

type Section = "projects" | "runs" | "providers" | "settings";
type ScanStatus = "idle" | "scanning" | "cancelling" | "complete" | "cancelled" | "error";

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
  const [scanId, setScanId] = useState("");
  const [progress, setProgress] = useState<ProjectScanProgress | null>(null);
  const [projects, setProjects] = useState<ProjectEntry[]>([]);
  const [selectedProjectId, setSelectedProjectId] = useState<string | null>(null);
  const [scanResult, setScanResult] = useState<ProjectScanFinished | null>(null);
  const [scanError, setScanError] = useState("");
  const [watchError, setWatchError] = useState("");
  const [projectRoot, setProjectRoot] = useState("C:\\Projects");
  const activeScanId = useRef("");
  const cancelRequested = useRef(false);
  const refreshAfterScan = useRef(false);
  const beginScanRef = useRef<(forceRefresh: boolean) => void>(() => undefined);
  const desktop = isDesktopApp();

  useEffect(() => {
    const handleNavigation = () => {
      setSection(sectionFromHash());
    };
    window.addEventListener("hashchange", handleNavigation);
    return () => window.removeEventListener("hashchange", handleNavigation);
  }, []);

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

    onProjectScanProgress((next) => {
      if (active && next.scanId === activeScanId.current) setProgress(next);
    }).then((stop) => {
      if (active) unlistenProgress = stop;
      else stop();
    }).catch((error: unknown) => {
      if (active) {
        setScanStatus("error");
        setScanError(error instanceof Error ? error.message : String(error));
      }
    });
    onProjectScanFinished((result) => {
      if (!active || result.scanId !== activeScanId.current) return;
      activeScanId.current = "";
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
      if (!result.cancelled && refreshAfterScan.current) {
        refreshAfterScan.current = false;
        window.setTimeout(() => beginScanRef.current(false), 0);
      }
    }).then((stop) => {
      if (active) unlistenFinished = stop;
      else stop();
    }).catch((error: unknown) => {
      if (active) {
        setScanStatus("error");
        setScanError(error instanceof Error ? error.message : String(error));
      }
    });

    onProjectScanInvalidated(() => {
      if (activeScanId.current) refreshAfterScan.current = true;
      else beginScanRef.current(false);
    }).then((stop) => {
      if (active) unlistenInvalidated = stop;
      else stop();
    }).catch((error: unknown) => {
      if (active) setWatchError(error instanceof Error ? error.message : String(error));
    });
    onProjectWatchError((message) => {
      if (active) setWatchError(message);
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
    setSelectedProjectId((selected) => projects.some((project) => project.id === selected) ? selected : projects[0]?.id ?? null);
  }, [projects]);

  const current = sections.find((item) => item.id === section) ?? sections[0];
  const collectionTitle = section === "projects" ? "Your projects" : section === "runs" ? "Recent runs" : "Connected providers";
  const scanning = scanStatus === "scanning" || scanStatus === "cancelling";

  async function beginScan(forceRefresh = true) {
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
    setScanResult(null);
    setProgress(null);
    setScanError("");
    try {
      const settings = await loadSettings();
      setProjectRoot(settings.projectRoot);
      await startProjectScan(id, forceRefresh);
      if (cancelRequested.current) await cancelProjectScan(id);
    } catch (error) {
      activeScanId.current = "";
      setScanStatus("error");
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

  beginScanRef.current = (forceRefresh) => { void beginScan(forceRefresh); };

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
          ) : (
            <section className="panel project-panel" aria-labelledby="project-list-title">
              <div className="panel-heading panel-heading-row">
                <div><h2 id="project-list-title">{collectionTitle}</h2><p>{section === "projects" ? `Scanning ${projectRoot} and its configured folders.` : "This workspace section is part of the foundation shell."}</p></div>
                <span className="project-count" aria-live="polite">{section === "projects" ? projects.length : ""}</span>
              </div>
              <div className="panel-rule" />

              {watchError && section === "projects" && <div className="scan-notice">Automatic project refresh is unavailable: {watchError} <span>Use Refresh projects to update the list.</span></div>}

              {section !== "projects" ? (
                <div className="empty-state"><h3>A little room to grow</h3><p>This section is ready for a future phase.</p></div>
              ) : projects.length > 0 ? (
                <>
                  {scanning && <div className="scan-progress" role="status"><span className="spinner" aria-hidden="true" /><span>{scanStatus === "cancelling" ? "Stopping the scan" : progress ? `Refreshing · scanned ${progress.directoriesScanned} folders · found ${progress.projectsFound} projects` : "Refreshing projects…"}</span></div>}
                  {scanStatus === "error" && <div className="scan-notice error-notice" role="alert"><strong>Refresh could not complete.</strong> {scanError} Showing the last available results. <button className="text-button" type="button" onClick={() => { void beginScan(); }}>Try again</button></div>}
                  {scanResult?.errors.length ? <details className="scan-notice"><summary>Some folders could not be inspected ({scanResult.errors.length})</summary><ul>{scanResult.errors.map((error, index) => <li key={`${index}-${error}`}>{error}</li>)}</ul><p>Showing available results.</p></details> : null}
                  {scanStatus === "cancelled" && <div className="scan-notice" role="status">Scan cancelled. Showing available results.</div>}
                  <ProjectsBrowser projects={projects} selectedProjectId={selectedProjectId} onSelect={setSelectedProjectId} />
                </>
              ) : scanning ? (
                <div className="state-box loading-state" role="status"><span className="spinner" aria-hidden="true" /><div><strong>{scanStatus === "cancelling" ? "Stopping the scan" : "Finding your projects"}</strong><p>{progress ? `Scanned ${progress.directoriesScanned} folders · found ${progress.projectsFound} projects` : "Preparing your workspace scan…"}</p></div></div>
              ) : scanStatus === "error" ? (
                <div className="state-box error-state" role="alert"><span className="state-symbol" aria-hidden="true">!</span><div><strong>We couldn’t scan this folder</strong><p>{scanError}</p><button className="text-button" type="button" onClick={() => { void beginScan(); }}>Try again</button></div></div>
              ) : (
                <div className="empty-state">
                  <div className="empty-illustration" aria-hidden="true"><span className="orbit orbit-one" /><span className="orbit orbit-two" /><span className="orbit orbit-three" /><span className="empty-spark spark-one">✦</span><span className="empty-spark spark-two">·</span><span className="folder-shape"><span /></span></div>
                  <h3>{scanStatus === "complete" ? scanResult?.errors.length ? "Scan partly complete" : "No projects found" : scanStatus === "cancelled" ? "Scan cancelled" : "Ready when you are"}</h3>
                  <p>{scanStatus === "complete" ? scanResult?.errors.length ? `Some folders under ${projectRoot} could not be inspected.` : `No coding projects were found under ${projectRoot}.` : scanStatus === "cancelled" ? `The scan stopped after checking ${scanResult?.directoriesScanned ?? 0} folders.` : "Scan your configured workspace to find coding projects and repositories."}</p>
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

const PROJECT_ROW_HEIGHT = 68;
const PROJECT_OVERSCAN = 5;

function ProjectsBrowser({ projects, selectedProjectId, onSelect }: {
  projects: ProjectEntry[];
  selectedProjectId: string | null;
  onSelect: (id: string) => void;
}) {
  const [query, setQuery] = useState("");
  const [stackFilter, setStackFilter] = useState("all");
  const [scrollTop, setScrollTop] = useState(0);
  const viewportRef = useRef<HTMLDivElement>(null);
  const lastViewRef = useRef({ projects, query, stackFilter, selectedProjectId });
  const stacks = useMemo(() => [...new Set(projects.flatMap((project) => project.detectedStacks))].sort(), [projects]);
  const visible = useMemo(() => {
    const normalized = query.trim().toLocaleLowerCase();
    return projects.filter((project) =>
      (stackFilter === "all" || project.detectedStacks.includes(stackFilter)) &&
      (!normalized || `${project.name} ${project.path} ${project.detectedStacks.join(" ")}`.toLocaleLowerCase().includes(normalized)),
    );
  }, [projects, query, stackFilter]);
  const selected = projects.find((project) => project.id === selectedProjectId) ?? null;
  const selectedIndex = visible.findIndex((project) => project.id === selectedProjectId);
  const start = Math.max(0, Math.floor(scrollTop / PROJECT_ROW_HEIGHT) - PROJECT_OVERSCAN);
  const end = Math.min(visible.length, Math.ceil((scrollTop + 360) / PROJECT_ROW_HEIGHT) + PROJECT_OVERSCAN);

  useLayoutEffect(() => {
    const previous = lastViewRef.current;
    lastViewRef.current = { projects, query, stackFilter, selectedProjectId };
    const viewport = viewportRef.current;
    if (!viewport) return;

    if (previous.query !== query || previous.stackFilter !== stackFilter) {
      viewport.scrollTop = 0;
      setScrollTop(0);
      return;
    }
    if (previous.projects === projects && previous.selectedProjectId === selectedProjectId) return;

    const index = visible.findIndex((project) => project.id === selectedProjectId);
    if (index < 0) return;
    const rowTop = index * PROJECT_ROW_HEIGHT;
    const rowBottom = rowTop + PROJECT_ROW_HEIGHT;
    let nextScrollTop = viewport.scrollTop;
    if (rowTop < nextScrollTop) nextScrollTop = rowTop;
    else if (rowBottom > nextScrollTop + viewport.clientHeight) nextScrollTop = rowBottom - viewport.clientHeight;
    if (nextScrollTop !== viewport.scrollTop) {
      viewport.scrollTop = nextScrollTop;
      setScrollTop(nextScrollTop);
    }
  }, [projects, query, stackFilter, selectedProjectId, visible]);

  function choose(index: number) {
    const bounded = Math.max(0, Math.min(visible.length - 1, index));
    const project = visible[bounded];
    if (!project) return;
    onSelect(project.id);
    const node = viewportRef.current;
    if (node) {
      const top = bounded * PROJECT_ROW_HEIGHT;
      if (top < node.scrollTop) node.scrollTop = top;
      else if (top + PROJECT_ROW_HEIGHT > node.scrollTop + node.clientHeight) node.scrollTop = top + PROJECT_ROW_HEIGHT - node.clientHeight;
    }
  }

  function handleListKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) {
      event.preventDefault();
      const current = visible.findIndex((project) => project.id === selectedProjectId);
      choose(event.key === "Home" ? 0 : event.key === "End" ? visible.length - 1 : current + (event.key === "ArrowDown" ? 1 : -1));
    }
  }

  return <div className="project-browser">
    <div className="project-browser-controls">
      <label className="project-search"><span className="sr-only">Search projects</span><span aria-hidden="true">⌕</span><input type="search" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search projects" /></label>
      <label className="project-filter"><span className="sr-only">Filter by stack</span><select value={stackFilter} onChange={(event) => setStackFilter(event.target.value)}><option value="all">All stacks</option>{stacks.map((stack) => <option key={stack} value={stack}>{stack}</option>)}</select></label>
      <span className="project-count">{visible.length} of {projects.length}</span>
    </div>
    <div className="project-browser-layout">
      <div ref={viewportRef} className="project-viewport" role="listbox" aria-label="Projects" aria-activedescendant={selected && selectedIndex >= start && selectedIndex < end ? `project-option-${cssSafeId(selected.id)}` : undefined} tabIndex={0} onKeyDown={handleListKeyDown} onScroll={(event) => setScrollTop(event.currentTarget.scrollTop)}>
        {visible.length ? <div className="project-virtual-space" style={{ height: visible.length * PROJECT_ROW_HEIGHT }}>
          <div className="project-virtual-window" style={{ transform: `translateY(${start * PROJECT_ROW_HEIGHT}px)` }}>
            {visible.slice(start, end).map((project, offset) => {
              const index = start + offset;
              return <div id={`project-option-${cssSafeId(project.id)}`} key={project.id} className={`project-option${selectedProjectId === project.id ? " selected" : ""}`} role="option" aria-selected={selectedProjectId === project.id} aria-posinset={index + 1} aria-setsize={visible.length} onClick={() => onSelect(project.id)}>
                <span className="project-symbol" aria-hidden="true">{project.isGitRepository ? "⌘" : "▣"}</span>
                <span className="project-option-copy"><strong>{project.name}</strong><span>{project.path}</span></span>
                <span className="project-option-tags">{project.detectedStacks.slice(0, 2).map((stack) => <span className="project-tag" key={stack}>{stack}</span>)}</span>
              </div>;
            })}
          </div>
        </div> : <div className="project-filter-empty">No projects match these filters.</div>}
      </div>
      <ProjectDetails project={selected} />
    </div>
  </div>;
}

function ProjectDetails({ project }: { project: ProjectEntry | null }) {
  const [activity, setActivity] = useState<GitHubActivity | null>(null);
  const [activityLoading, setActivityLoading] = useState(false);
  const [activityError, setActivityError] = useState("");
  const remote = project?.git.remotes.find((item) => item.github)?.github;
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
    setActivityLoading(true);
    setActivityError("");
    try {
      setActivity(await refreshGitHubActivity(remote.owner, remote.repository));
    } catch (error) {
      setActivityError(error instanceof Error ? error.message : String(error));
    } finally {
      setActivityLoading(false);
    }
  }

  if (!project) return <aside className="project-detail empty-detail"><h3>Select a project</h3><p>Choose a project to see its local details.</p></aside>;
  const git = project.git;
  return <aside className="project-detail" aria-labelledby="project-detail-title">
    <p className="eyebrow">PROJECT DETAILS</p><h3 id="project-detail-title">{project.name}</h3><p className="detail-path">{project.path}</p>
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
      {activityError && <span role="status">{activityError}</span>}
      {isDesktopApp() && <button className="text-button" type="button" onClick={() => { void refreshActivity(); }} disabled={activityLoading}>{activityLoading ? "Refreshing GitHub…" : "Refresh GitHub activity"}</button>}
    </div>}
    {project.workflowPrerequisites.length > 0 && <div className="detail-group"><strong>Workflow prerequisites</strong>{project.workflowPrerequisites.map((tool) => <span key={tool.name}>{tool.name} · {tool.available ? "available" : "missing"}</span>)}</div>}
  </aside>;
}

function cssSafeId(value: string): string { return encodeURIComponent(value); }
