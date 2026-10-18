import { ActivityEntry, ActivityOverview } from "./Activity";
import { ProjectCollection } from "./ProjectCollection";
import { useEffect, useRef, useState } from "react";
import { SettingsPanel } from "./SettingsPanel";
import { ProvidersPanel } from "./ProvidersPanel";
import { RunsPanel } from "./RunsPanel";
import { LocalIssuesPanel } from "./LocalIssuesPanel";
import { BatchRunnerPanel } from "./BatchRunnerPanel";
import {
  cancelProjectScan,
  getProjectInventory,
  isDesktopApp,
  loadSettings,
  onProjectScanFinished,
  onProjectScanInvalidated,
  onProjectDiscoverySettingsChanged,
  onProjectScanProgress,
  onProjectWatchError,
  ProjectEntry,
  ProjectScanFinished,
  ProjectScanProgress,
  startProjectScan,
  startAffectedProjectRefresh,
} from "./settings";

type Section = "projects" | "local-issues" | "batches" | "runs" | "providers" | "settings";
type ScanStatus = "idle" | "scanning" | "cancelling" | "complete" | "cancelled" | "error";
type InventoryFreshness = "loading" | "saved" | "partial" | "refreshing" | "current" | "stale";

const sections: { id: Section; label: string; icon: string }[] = [
  { id: "projects", label: "My Projects", icon: "◫" },
  { id: "local-issues", label: "Local issues", icon: "☷" },
  { id: "batches", label: "Runner batches", icon: "▷" },
  { id: "runs", label: "Activity", icon: "↗" },
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
      if (!active) return;
      if (activeScanId.current) {
        refreshAfterScan.current = [...new Set([...refreshAfterScan.current, ...paths])].slice(0, 512);
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
      setInventoryFreshness("stale");
      loadSettings().then(settings => { setProjectRoot(settings.projectRoot); return getProjectInventory(); }).then(setProjects).catch(error => setScanError(String(error)));
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

  // Catalog discovery is explicit. On startup only re-inspect saved collection entries.
  useEffect(() => {
    if (!desktop || !scanListenersReady) return;
    let active = true;
    Promise.all([loadSettings(), getProjectInventory()]).then(([settings, inventory]) => {
      if (active) beginAffectedRefreshRef.current(inventory.filter(p => settings.preferences.projectDisplay.favoriteProjectIds.includes(p.id)).map(p => p.path));
    }).catch(() => undefined);
    return () => { active = false; };
  }, [desktop, scanListenersReady]);

  useEffect(() => {
    const refreshCollection = () => {
      if (!desktop) return;
      Promise.all([loadSettings(), getProjectInventory()]).then(([settings, inventory]) => {
        beginAffectedRefreshRef.current(inventory.filter(p => settings.preferences.projectDisplay.favoriteProjectIds.includes(p.id)).map(p => p.path));
      }).catch(() => undefined);
    };
    window.addEventListener("fabrica-collection-saved", refreshCollection);
    return () => window.removeEventListener("fabrica-collection-saved", refreshCollection);
  }, [desktop]);

  const current = sections.find((item) => item.id === section) ?? sections[0];
  const selectedProject = projects.find((project) => project.id === selectedProjectId) ?? null;
  const scanning = scanStatus === "scanning" || scanStatus === "cancelling";
  const freshnessLabel = inventoryFreshness === "loading" ? "Loading saved projects…"
    : inventoryFreshness === "saved" ? "Saved project collection"
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
      refreshAfterScan.current = [...new Set([...refreshAfterScan.current, ...paths])].slice(0, 512);
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
          <img className="brand-mark" src={new URL("../src-tauri/icons/icon.png", import.meta.url).href} alt="" width={34} height={34} />
          <span className="brand-copy"><strong>navis</strong><span>FABRICA</span></span>
        </a>

        <div className="workspace-label">WORKSPACE</div>
        <nav className="primary-nav" aria-label="Main navigation">
          {sections.filter(item => ["projects", "runs", "settings"].includes(item.id)).map((item) => (
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
        </div>
      </aside>

      <div className="main-column">
        <header className="toolbar">
          <div className="breadcrumb"><span>Workspace</span><span className="breadcrumb-slash">/</span><strong>{current.label}</strong></div>
          <div className="toolbar-actions"><ActivityEntry /></div>
        </header>

        <main id="main-content" className="content" tabIndex={-1}>
          <div className="page-heading">
            <div>
              <p className="eyebrow">YOUR WORKSPACE</p>
              <h1>{section === "projects" ? "My Projects" : current.label}</h1>
              <p className="page-subtitle">{section === "projects" ? "A clear view of the work taking shape." : section === "settings" ? "Make this workspace feel like yours." : section === "providers" ? "Configure manual choices and inspect known provider capabilities." : section === "local-issues" ? "Read and edit local Markdown issues." : section === "batches" ? "Preview and manage bounded batches from local issues." : "Follow your work and open project actions."}</p>
            </div>
            {section === "settings" && <button className="button button-primary" type="button" onClick={scanning ? stopScan : () => { void beginScan(); }} disabled={scanStatus === "cancelling"}>
              <span aria-hidden="true">{scanning ? "×" : "↻"}</span> {scanning ? "Cancel scan" : projects.length ? "Rediscover project folders" : "Discover project folders"}
            </button>}
            {section === "settings" && <p role="status">{scanning ? "Discovering folders…" : scanError || freshnessLabel}</p>}{section === "settings" && Boolean(scanResult?.errors.length) && <details><summary>Some folders could not be inspected</summary><ul>{scanResult?.errors.map((error,index) => <li key={index}>{error}</li>)}</ul></details>}
          </div>

          <div hidden={section !== "projects"}><p className="scan-measurement" role="status">{freshnessLabel}</p>{watchError && <p role="status">Automatic content updates are unavailable. Reopen the app or use Settings to refresh.</p>}{!desktop && <p className="scan-notice">Browser preview: native discovery, Git details and local issue reads require the desktop app.</p>}<ProjectCollection projects={projects} onSelect={setSelectedProjectId} /></div>
          {section === "runs" && <ActivityOverview projects={projects} onSelect={setSelectedProjectId} />}
          <div hidden={section !== "local-issues"}><LocalIssuesPanel defaultPath={selectedProject?.path ?? projectRoot} /></div>
          <div hidden={section !== "batches"}><BatchRunnerPanel /></div>
          {section === "local-issues" || section === "batches" ? null : section === "settings" ? (
            <><ProjectCollection projects={projects} manage onSelect={setSelectedProjectId} /><details className="panel advanced-settings"><summary>Provider & model settings</summary><ProvidersPanel /></details><details className="panel advanced-settings"><summary>Discovery & workspace settings</summary><SettingsPanel /></details></>
          ) : section === "providers" ? (
            <ProvidersPanel />
          ) : section === "runs" ? selectedProject ? isDesktopApp() ? (
            <><div className="project-actions"><a href="#batches">Local issue batches</a><a href="#local-issues">Local issue workspace</a></div><RunsPanel defaultCwd={selectedProject.path} project={selectedProject} issue={null} /></>
          ) : (
            <div className="panel empty-state"><h3>Workflow execution needs the desktop app</h3><p>The browser preview can inspect project context, but it cannot launch local processes.</p></div>
          ) : (
            <section className="panel empty-state"><h2>Activity</h2><p>Choose a project from My Projects to open its actions.</p><div className="project-actions"><a href="#projects">My Projects</a><a href="#batches">Local issue batches</a></div></section>
          ) : null}
        </main>
      </div>
    </div>
  );
}

export default App;
