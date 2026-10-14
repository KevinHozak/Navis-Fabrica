import { useEffect, useRef, useState } from "react";
import { SettingsPanel } from "./SettingsPanel";
import { ProvidersPanel } from "./ProvidersPanel";
import {
  cancelProjectScan,
  isDesktopApp,
  loadSettings,
  onProjectScanFinished,
  onProjectScanProgress,
  ProjectEntry,
  ProjectScanFinished,
  ProjectScanProgress,
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
  const [scanResult, setScanResult] = useState<ProjectScanFinished | null>(null);
  const [scanError, setScanError] = useState("");
  const [projectRoot, setProjectRoot] = useState("C:\\Projects");
  const activeScanId = useRef("");
  const cancelRequested = useRef(false);
  const desktop = isDesktopApp();

  useEffect(() => {
    const handleNavigation = () => setSection(sectionFromHash());
    window.addEventListener("hashchange", handleNavigation);
    return () => window.removeEventListener("hashchange", handleNavigation);
  }, []);

  useEffect(() => {
    document.title = `Navis-Fabrica · v${__APP_VERSION__}`;
    loadSettings().then((settings) => setProjectRoot(settings.projectRoot)).catch(() => undefined);
  }, []);

  useEffect(() => {
    if (!desktop) return;
    let active = true;
    let unlistenProgress: (() => void) | undefined;
    let unlistenFinished: (() => void) | undefined;

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
      setScanResult(result);
      setProjects(result.projects);
      setScanError(result.fatalError ?? "");
      setScanStatus(result.fatalError ? "error" : result.cancelled ? "cancelled" : "complete");
    }).then((stop) => {
      if (active) unlistenFinished = stop;
      else stop();
    }).catch((error: unknown) => {
      if (active) {
        setScanStatus("error");
        setScanError(error instanceof Error ? error.message : String(error));
      }
    });

    return () => {
      active = false;
      unlistenProgress?.();
      unlistenFinished?.();
    };
  }, [desktop]);

  const current = sections.find((item) => item.id === section) ?? sections[0];
  const collectionTitle = section === "projects" ? "Your projects" : section === "runs" ? "Recent runs" : "Connected providers";
  const scanning = scanStatus === "scanning" || scanStatus === "cancelling";

  async function beginScan() {
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
    setProjects([]);
    setScanResult(null);
    setProgress(null);
    setScanError("");
    try {
      const settings = await loadSettings();
      setProjectRoot(settings.projectRoot);
      await startProjectScan(id);
      if (cancelRequested.current) await cancelProjectScan(id);
    } catch (error) {
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
            {section === "projects" && <button className="button button-primary" type="button" onClick={scanning ? stopScan : beginScan} disabled={scanStatus === "cancelling"}>
              <span aria-hidden="true">{scanning ? "×" : "↻"}</span> {scanning ? "Cancel scan" : "Scan projects"}
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

              {section !== "projects" ? (
                <div className="empty-state"><h3>A little room to grow</h3><p>This section is ready for a future phase.</p></div>
              ) : scanning ? (
              <div className="state-box loading-state" role="status"><span className="spinner" aria-hidden="true" /><div><strong>{scanStatus === "cancelling" ? "Stopping the scan" : "Finding your projects"}</strong><p>{scanError || (progress ? `Scanned ${progress.directoriesScanned} folders · found ${progress.projectsFound} projects` : "Preparing your workspace scan…")}</p></div></div>
              ) : scanStatus === "error" ? (
                <div className="state-box error-state" role="alert"><span className="state-symbol" aria-hidden="true">!</span><div><strong>We couldn’t scan this folder</strong><p>{scanError}</p><button className="text-button" type="button" onClick={beginScan}>Try again</button></div></div>
              ) : projects.length > 0 ? (
                <>
                  {scanResult?.errors.length ? <details className="scan-notice"><summary>Some folders could not be inspected ({scanResult.errors.length})</summary><ul>{scanResult.errors.map((error, index) => <li key={`${index}-${error}`}>{error}</li>)}</ul><p>Showing available results.</p></details> : null}
                  {scanStatus === "cancelled" && <div className="scan-notice" role="status">Scan cancelled. Showing projects found so far.</div>}
                  <ul className="project-list">
                    {projects.map((project) => <li className="project-row" key={project.id}>
                      <span className="project-symbol" aria-hidden="true">{project.isGitRepository ? "⌘" : "▣"}</span>
                      <div className="project-details"><strong>{project.name}</strong><span>{project.path}</span>
                        {project.git.status === "available" ? <span className="project-facts">
                          {project.git.branch ? `Branch ${project.git.branch}` : "Branch unavailable"}
                          {project.git.dirty === true ? " · Uncommitted changes" : project.git.dirty === false ? " · Clean" : ""}
                          {project.git.latestCommit ? ` · ${project.git.latestCommit.shortHash} ${project.git.latestCommit.subject}` : ""}
                          {project.git.upstream.status === "tracking" ? ` · ↑${project.git.upstream.ahead ?? 0} ↓${project.git.upstream.behind ?? 0}` : project.git.upstream.status === "noUpstream" ? " · No upstream" : " · Upstream status unavailable"}
                        </span> : project.isGitRepository ? <span className="project-facts">{gitStatusLabel(project.git.status)}</span> : null}
                        {project.git.remotes.length > 0 && <span className="project-facts">Remotes: {project.git.remotes.map((remote) => remote.url ? `${remote.name} ${remote.url}` : remote.name).join(" · ")}</span>}
                        {project.workflowPrerequisites.length > 0 && <span className="project-facts">Tools: {project.workflowPrerequisites.map((tool) => `${tool.name} ${tool.available ? "available" : "missing"}`).join(" · ")}</span>}
                      </div>
                      <div className="project-tags">
                        {project.isGitRepository && <span className="project-tag">Git</span>}
                        {project.isWorktree && <span className="project-tag">Worktree</span>}
                        {project.detectedStacks.map((stack) => <span className="project-tag" key={stack}>{stack}</span>)}
                      </div>
                    </li>)}
                  </ul>
                </>
              ) : (
                <div className="empty-state">
                  <div className="empty-illustration" aria-hidden="true"><span className="orbit orbit-one" /><span className="orbit orbit-two" /><span className="orbit orbit-three" /><span className="empty-spark spark-one">✦</span><span className="empty-spark spark-two">·</span><span className="folder-shape"><span /></span></div>
                  <h3>{scanStatus === "complete" ? scanResult?.errors.length ? "Scan partly complete" : "No projects found" : scanStatus === "cancelled" ? "Scan cancelled" : "Ready when you are"}</h3>
                  <p>{scanStatus === "complete" ? scanResult?.errors.length ? `Some folders under ${projectRoot} could not be inspected.` : `No coding projects were found under ${projectRoot}.` : scanStatus === "cancelled" ? `The scan stopped after checking ${scanResult?.directoriesScanned ?? 0} folders.` : "Scan your configured workspace to find coding projects and repositories."}</p>
                  {scanResult?.errors.length ? <details className="scan-notice"><summary>Show scan issues ({scanResult.errors.length})</summary><ul>{scanResult.errors.map((error, index) => <li key={`${index}-${error}`}>{error}</li>)}</ul></details> : null}
                  {scanStatus === "idle" && <button className="button button-secondary" type="button" onClick={beginScan}>↻ Scan workspace</button>}
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
