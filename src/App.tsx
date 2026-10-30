import { sameProjectPath } from "./Activity";
import { mergeRefreshPaths as mergePaths, settleRefreshPaths, withoutRefreshPaths as withoutPaths } from "./projectRefreshState";
import { ProjectCollection } from "./ProjectCollection";
import { UsageDashboard } from "./UsageDashboard";
import { useEffect, useRef, useState } from "react";
import { SettingsPanel } from "./SettingsPanel";
import { ProvidersPanel } from "./ProvidersPanel";
import { RunsPanel } from "./RunsPanel";
import { LocalIssuesPanel } from "./LocalIssuesPanel";
import { BatchRunnerPanel } from "./BatchRunnerPanel";
import { ProjectView } from "./ProjectView";
import { BrowserIcon } from "./BrowserIcon";
import { parseRoute, routeHash, routeKey, routeTitle, type AppRoute } from "./navigation";
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

type ScanStatus = "idle" | "scanning" | "cancelling" | "complete" | "cancelled" | "error";
type InventoryFreshness = "loading" | "saved" | "partial" | "refreshing" | "current" | "stale";

type AppHistoryState = { fabricaNavigation?: true; index?: number; maxIndex?: number; scrollY?: number; scrollTargets?: Record<string, number> };

function routeFromHash(hash: string): AppRoute {
  return parseRoute(hash) ?? { section: "projects" };
}

function isIssueRoute(route: AppRoute): boolean {
  return route.section === "epics-stories" || route.section === "local-issues";
}

function isProjectRoute(route: AppRoute): boolean {
  return route.section === "project" || route.section === "changes" || route.section === "build";
}

function App() {
  const [route, setRoute] = useState<AppRoute>(() => routeFromHash(window.location.hash));
  const routeRef = useRef(route);
  const [mountedIssueRoutes, setMountedIssueRoutes] = useState<AppRoute[]>(() => isIssueRoute(route) ? [route] : []);
  const [visitedProjectIds, setVisitedProjectIds] = useState<string[]>(() => isProjectRoute(route) && route.projectId ? [route.projectId] : []);
  const [canGoBack, setCanGoBack] = useState(false);
  const [canGoForward, setCanGoForward] = useState(false);
  const dirtyEditors = useRef(new Map<string, boolean>());
  const historyIndex = useRef(0);
  const maxHistoryIndex = useRef(0);
  const ignoreNextPop = useRef(false);
  const [scanStatus, setScanStatus] = useState<ScanStatus>("idle");
  const [scanListenersReady, setScanListenersReady] = useState(false);
  const [scanId, setScanId] = useState("");
  const [progress, setProgress] = useState<ProjectScanProgress | null>(null);
  const [projects, setProjects] = useState<ProjectEntry[]>([]);
  const [scanResult, setScanResult] = useState<ProjectScanFinished | null>(null);
  const [scanDurationMs, setScanDurationMs] = useState<number | null>(null);
  const [firstProjectMs, setFirstProjectMs] = useState<number | null>(null);
  const [scanError, setScanError] = useState("");
  const [watchError, setWatchError] = useState("");
  const [inventoryFreshness, setInventoryFreshness] = useState<InventoryFreshness>("loading");
  const [refreshingPaths, setRefreshingPaths] = useState<string[]>([]);
  const [refreshErrorPaths, setRefreshErrorPaths] = useState<string[]>([]);
  const [projectRefreshVersions, setProjectRefreshVersions] = useState<Record<string, number>>({});
  const [startupRefreshPending, setStartupRefreshPending] = useState(() => isDesktopApp());
  const [projectRoot, setProjectRoot] = useState("C:\\Projects");
  const activeScanId = useRef("");
  const scanStartedAt = useRef<number | null>(null);
  const firstProjectMsRef = useRef<number | null>(null);
  const cancelRequested = useRef(false);
  const refreshAfterScan = useRef<string[]>([]);
  const fullRefreshAfterScan = useRef(false);
  const partialScanIds = useRef(new Set<string>());
  const scanPaths = useRef(new Map<string, string[]>());
  const beginScanRef = useRef<(forceRefresh: boolean, maxDirectories?: number) => void>(() => undefined);
  const beginAffectedRefreshRef = useRef<(paths: string[]) => void>(() => undefined);
  const desktop = isDesktopApp();
  routeRef.current = route;

  function rememberRoute(next: AppRoute) {
    setRoute(next);
    routeRef.current = next;
    if (isIssueRoute(next)) setMountedIssueRoutes(current => current.some(item => routeKey(item) === routeKey(next)) ? current : [...current, next]);
    if (isProjectRoute(next) && next.projectId) setVisitedProjectIds(current => current.includes(next.projectId!) ? current : [...current, next.projectId!]);
  }

  function updateNavigationButtons() {
    setCanGoBack(historyIndex.current > 0);
    setCanGoForward(historyIndex.current < maxHistoryIndex.current);
  }

  function hasDirtyIssueEditors() {
    return [...dirtyEditors.current.values()].some(Boolean);
  }

  function allowNavigation(): boolean {
    return !hasDirtyIssueEditors() || window.confirm("You have unsaved issue edits. Continue? Your edits will be kept open.");
  }

  function navigateToHash(hash: string) {
    const next = parseRoute(hash);
    if (!next || hash === window.location.hash || !allowNavigation()) return;
    const currentIndex = historyIndex.current;
    const nextIndex = currentIndex + 1;
    const currentState = window.history.state as AppHistoryState | null;
    const currentEntry = { ...(currentState ?? {}), fabricaNavigation: true as const, index: currentIndex, maxIndex: nextIndex, scrollY: window.scrollY,
      scrollTargets: Object.fromEntries([...document.querySelectorAll<HTMLElement>("[data-fabrica-history-scroll]")].map(element => [element.dataset.fabricaHistoryScroll ?? "", element.scrollTop])) };
    window.history.replaceState(currentEntry, "", window.location.href);
    window.history.pushState({ fabricaNavigation: true, index: nextIndex, maxIndex: nextIndex, scrollY: 0, scrollTargets: {} }, "", hash);
    historyIndex.current = nextIndex;
    maxHistoryIndex.current = nextIndex;
    window.sessionStorage.setItem("fabrica-history-max-index", String(nextIndex));
    updateNavigationButtons();
    rememberRoute(next);
    window.scrollTo(0, 0);
  }

  function navigateToRoute(next: AppRoute) { navigateToHash(routeHash(next)); }

  function restoreHistoryScroll(state: AppHistoryState | null) {
    window.requestAnimationFrame(() => {
      window.scrollTo(0, state?.scrollY ?? 0);
      for (const element of document.querySelectorAll<HTMLElement>("[data-fabrica-history-scroll]")) {
        const key = element.dataset.fabricaHistoryScroll ?? "";
        const top = state?.scrollTargets?.[key];
        if (typeof top === "number") element.scrollTop = top;
      }
    });
  }

  function goBack() { if (historyIndex.current > 0) window.history.back(); }
  function goForward() { if (historyIndex.current < maxHistoryIndex.current) window.history.forward(); }

  function failCurrentScan(error: unknown) {
    const failedPaths = mergePaths([...scanPaths.current.values()].flat(), refreshAfterScan.current);
    scanPaths.current.clear();
    activeScanId.current = "";
    refreshAfterScan.current = [];
    fullRefreshAfterScan.current = false;
    partialScanIds.current.clear();
    setRefreshingPaths([]);
    setRefreshErrorPaths(current => mergePaths(current, failedPaths));
    setStartupRefreshPending(false);
    setScanStatus("error");
    setInventoryFreshness("stale");
    setScanError(error instanceof Error ? error.message : String(error));
  }

  useEffect(() => {
    const existing = window.history.state as AppHistoryState | null;
    if (existing?.fabricaNavigation && typeof existing.index === "number") {
      historyIndex.current = existing.index;
      maxHistoryIndex.current = Math.max(existing.index, Number(window.sessionStorage.getItem("fabrica-history-max-index")) || existing.index, existing.maxIndex ?? existing.index);
    } else {
      historyIndex.current = 0;
      maxHistoryIndex.current = 0;
      window.sessionStorage.setItem("fabrica-history-max-index", "0");
      const hash = window.location.hash || "#projects";
      window.history.replaceState({ fabricaNavigation: true, index: 0, maxIndex: 0, scrollY: window.scrollY, scrollTargets: {} }, "", hash);
    }
    updateNavigationButtons();
    const onPopState = (event: PopStateEvent) => {
      const state = event.state as AppHistoryState | null;
      const priorIndex = historyIndex.current;
      if (ignoreNextPop.current) {
        ignoreNextPop.current = false;
        historyIndex.current = state?.index ?? priorIndex;
        updateNavigationButtons();
        restoreHistoryScroll(state);
        return;
      }
      const nextIndex = state?.fabricaNavigation ? state.index ?? priorIndex : 0;
      if (!allowNavigation()) {
        ignoreNextPop.current = true;
        window.history.go(priorIndex - nextIndex);
        return;
      }
      historyIndex.current = nextIndex;
      updateNavigationButtons();
      rememberRoute(routeFromHash(window.location.hash));
      restoreHistoryScroll(state);
    };
    window.history.scrollRestoration = "manual";
    window.addEventListener("popstate", onPopState);
    return () => { window.removeEventListener("popstate", onPopState); window.history.scrollRestoration = "auto"; };
  }, []);

  useEffect(() => {
    let frame = 0;
    const captureScroll = () => {
      if (frame) return;
      frame = window.requestAnimationFrame(() => {
        frame = 0;
        const state = window.history.state as AppHistoryState | null;
        if (!state?.fabricaNavigation) return;
        window.history.replaceState({ ...state, scrollY: window.scrollY,
          scrollTargets: Object.fromEntries([...document.querySelectorAll<HTMLElement>("[data-fabrica-history-scroll]")].map(element => [element.dataset.fabricaHistoryScroll ?? "", element.scrollTop])) }, "", window.location.href);
      });
    };
    window.addEventListener("scroll", captureScroll, { passive: true, capture: true });
    return () => { window.removeEventListener("scroll", captureScroll, true); if (frame) window.cancelAnimationFrame(frame); };
  }, []);

  useEffect(() => {
    const onClick = (event: MouseEvent) => {
      if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
      const target = event.target;
      if (!(target instanceof Element)) return;
      const anchor = target.closest<HTMLAnchorElement>('a[href^="#"]');
      if (!anchor || anchor.target === "_blank" || anchor.hasAttribute("download")) return;
      if (!parseRoute(anchor.getAttribute("href") ?? "")) return;
      event.preventDefault();
      navigateToHash(anchor.getAttribute("href") ?? "");
    };
    document.addEventListener("click", onClick);
    return () => document.removeEventListener("click", onClick);
  }, [navigateToHash]);

  useEffect(() => {
    const onDirty = (event: Event) => {
      const detail = (event as CustomEvent<{ key: string; dirty: boolean }>).detail;
      if (!detail || typeof detail.key !== "string") return;
      dirtyEditors.current.set(detail.key, detail.dirty === true);
    };
    window.addEventListener("fabrica-issue-edit-state", onDirty);
    return () => window.removeEventListener("fabrica-issue-edit-state", onDirty);
  }, []);

  useEffect(() => {
    document.title = `Navis-Fabrica · v${__APP_VERSION__}`;
    loadSettings().then((settings) => {
      setProjectRoot(settings.projectRoot);
    }).catch(() => undefined);
  }, []);

  useEffect(() => {
    const updateProject = (event: Event) => {
      const project = (event as CustomEvent<ProjectEntry>).detail;
      setProjects((items) => items.map((item) => item.id === project.id ? project : item));
    };
    window.addEventListener("fabrica-project-updated", updateProject);
    return () => window.removeEventListener("fabrica-project-updated", updateProject);
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
      if (active) failCurrentScan(error);
    });
    onProjectScanFinished((result) => {
      if (!active || result.scanId !== activeScanId.current) return;
      const completedPaths = scanPaths.current.get(result.scanId) ?? [];
      scanPaths.current.delete(result.scanId);
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
      const queuedFullRefresh = fullRefreshAfterScan.current;
      const queuedPaths = refreshAfterScan.current;
      const continueRefresh = !result.cancelled && !result.fatalError;
      const settledPaths = continueRefresh ? completedPaths : mergePaths(completedPaths, queuedPaths);
      const failedPaths = result.fatalError
        ? settledPaths
        : result.errors.length
          ? completedPaths.filter(path => !result.projects.some(project => sameProjectPath(project.path, path)))
          : [];
      setRefreshingPaths(current => settleRefreshPaths(current, settledPaths, queuedPaths, continueRefresh));
      setRefreshErrorPaths(current => mergePaths(withoutPaths(current, settledPaths), failedPaths));
      if (continueRefresh) {
        setProjectRefreshVersions(current => {
          const next = { ...current };
          for (const project of result.projects) {
            if (completedPaths.some(path => sameProjectPath(path, project.path)) && !failedPaths.some(path => sameProjectPath(path, project.path))) {
              next[project.id] = (next[project.id] ?? 0) + 1;
            }
          }
          return next;
        });
      }
      if (!continueRefresh) {
        fullRefreshAfterScan.current = false;
        refreshAfterScan.current = [];
      } else if (queuedFullRefresh) {
        fullRefreshAfterScan.current = false;
        refreshAfterScan.current = [];
        window.setTimeout(() => beginScanRef.current(true), 0);
      } else if (queuedPaths.length) {
        const pending = [...new Set(refreshAfterScan.current)];
        refreshAfterScan.current = [];
        window.setTimeout(() => beginAffectedRefreshRef.current(pending), 0);
      }
    }).then((stop) => {
      if (active) unlistenFinished = stop;
      else stop();
      markScanListenerReady();
    }).catch((error: unknown) => {
      if (active) failCurrentScan(error);
    });

    onProjectScanInvalidated((paths) => {
      if (!active) return;
      if (activeScanId.current) {
        refreshAfterScan.current = mergePaths(refreshAfterScan.current, paths).slice(0, 512);
        setRefreshingPaths(current => mergePaths(current, paths.slice(0, 512)));
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
    if (!desktop) { setStartupRefreshPending(false); return; }
    if (!scanListenersReady) return;
    let active = true;
    Promise.all([loadSettings(), getProjectInventory()]).then(([settings, inventory]) => {
      if (active) {
        beginAffectedRefreshRef.current(inventory.filter(p => settings.preferences.projectDisplay.favoriteProjectIds.includes(p.id)).map(p => p.path));
        setStartupRefreshPending(false);
      }
    }).catch((error: unknown) => {
      if (active) {
        setStartupRefreshPending(false);
        setScanError(error instanceof Error ? error.message : String(error));
      }
    });
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

  const selectedProject = projects.find(project => project.id === route.projectId) ?? null;
  const scanning = scanStatus === "scanning" || scanStatus === "cancelling";
  const title = routeTitle(route, selectedProject?.name);
  const freshnessLabel = inventoryFreshness === "loading" ? "Loading saved projects…"
    : inventoryFreshness === "saved" ? "Saved project collection"
      : inventoryFreshness === "partial" ? "Saved project list · changed repositories refreshed"
      : inventoryFreshness === "refreshing" ? "Refreshing project details in the background…"
        : inventoryFreshness === "stale" ? "Showing saved project details · refresh needed"
          : "Project list is up to date";

  async function beginScan(forceRefresh = true, maxDirectories?: number) {
    if (activeScanId.current) {
      if (forceRefresh) {
        fullRefreshAfterScan.current = true;
        const paths = projects.map(project => project.path);
        refreshAfterScan.current = mergePaths(refreshAfterScan.current, paths).slice(0, 512);
        setRefreshingPaths(current => mergePaths(current, paths));
      }
      return;
    }
    if (!desktop) {
      setScanStatus("error");
      setScanError("Project scanning is available in the Navis-Fabrica desktop app.");
      return;
    }
    const id = crypto.randomUUID();
    const paths = projects.map(project => project.path);
    scanPaths.current.set(id, paths);
    setRefreshingPaths(current => mergePaths(current, paths));
    setRefreshErrorPaths(current => withoutPaths(current, paths));
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
      failCurrentScan(error);
    }
  }

  async function beginAffectedRefresh(paths: string[]) {
    if (!desktop || !paths.length) return;
    if (activeScanId.current) {
      refreshAfterScan.current = mergePaths(refreshAfterScan.current, paths).slice(0, 512);
      setRefreshingPaths(current => mergePaths(current, paths.slice(0, 512)));
      return;
    }
    const id = crypto.randomUUID();
    const scanPathsToRefresh = mergePaths(paths).slice(0, 512);
    scanPaths.current.set(id, scanPathsToRefresh);
    setRefreshingPaths(current => mergePaths(current, scanPathsToRefresh));
    setRefreshErrorPaths(current => withoutPaths(current, scanPathsToRefresh));
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
      await startAffectedProjectRefresh(id, scanPathsToRefresh);
    } catch (error) {
      failCurrentScan(error);
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

  return <div className={`app-shell${route.section === "runs" ? " activity-shell" : ""}`}>
      <a className="skip-link" href="#main-content" onClick={(event) => { event.preventDefault(); document.getElementById("main-content")?.focus(); }}>Skip to content</a>
    <header className="browser-toolbar">
      <nav className="toolbar-group toolbar-left" aria-label="Browser navigation">
        <button className="toolbar-icon-button" type="button" title="Back" aria-label="Back" disabled={!canGoBack} onClick={goBack}><BrowserIcon name="back" /></button>
        <button className="toolbar-icon-button" type="button" title="Forward" aria-label="Forward" disabled={!canGoForward} onClick={goForward}><BrowserIcon name="forward" /></button>
        <a className="toolbar-icon-button" href="#projects" title="Home" aria-label="Home"><BrowserIcon name="home" /></a>
      </nav>
      <div className="toolbar-context" aria-live="polite">{title}</div>
      <nav className="toolbar-group toolbar-right" aria-label="Workspace tools">
        <a className="toolbar-icon-button" href="#usage" title="Model Usage" aria-label="Model Usage"><BrowserIcon name="usage" /></a>
        <a className="toolbar-icon-button" href={route.projectId ? `#runs?project=${encodeURIComponent(route.projectId)}` : "#runs"} title="Activity" aria-label="Activity"><BrowserIcon name="activity" /></a>
        <a className="toolbar-icon-button" href="#settings" title="Settings" aria-label="Settings"><BrowserIcon name="settings" /></a>
      </nav>
    </header>
    <main id="main-content" className={`content browser-content${route.section === "runs" ? " content-activity" : ""}${route.section === "projects" ? " content-home" : ""}`} tabIndex={-1}>
      <div className="route-layer" hidden={route.section !== "projects"}>
        {watchError && <p role="status">Automatic content updates are unavailable. Use Settings to refresh.</p>}
        {!desktop && <p className="scan-notice">Browser preview: native discovery and Git details require the desktop app.</p>}
        <ProjectCollection projects={projects} onOpenProject={project => navigateToRoute({ section: "project", projectId: project.id })} onOpenChanges={project => navigateToRoute({ section: "changes", projectId: project.id })} onOpenBuild={project => navigateToRoute({ section: "build", projectId: project.id })} onOpenIssues={(project, type) => navigateToRoute({ section: "epics-stories", projectId: project.id, issueType: type })} onRefreshProject={path => { void beginAffectedRefresh([path]); }} refreshingPaths={refreshingPaths} refreshErrorPaths={refreshErrorPaths} refreshVersions={projectRefreshVersions} startupRefreshPending={startupRefreshPending} />
      </div>
      {visitedProjectIds.map(id => {
        const project = projects.find(item => item.id === id);
        if (!project) return null;
        const active = route.projectId === id && isProjectRoute(route);
        return <div className="route-layer" hidden={!active} key={`project:${id}`}><ProjectView project={project} section={active ? route.section as "project" | "changes" | "build" : "project"} /></div>;
      })}
      {mountedIssueRoutes.map(issueRoute => {
        const project = projects.find(item => item.id === issueRoute.projectId);
        const active = isIssueRoute(route) && routeKey(issueRoute) === routeKey(route);
        return <div className="route-layer" hidden={!active} key={routeKey(issueRoute)}>{issueRoute.section === "epics-stories" && project ? <LocalIssuesPanel defaultPath={project.path} initialType={issueRoute.issueType ?? "epic"} autoLoad title={`${project.name} · Epics & Stories`} navigationKey={routeKey(issueRoute)} /> : issueRoute.section === "local-issues" ? <LocalIssuesPanel defaultPath={project?.path ?? projectRoot} navigationKey={routeKey(issueRoute)} /> : <section className="panel"><p>Select a project to browse epics and stories.</p><a href="#projects">Home</a></section>}</div>;
      })}
      <div className="route-layer" hidden={route.section !== "usage"}><UsageDashboard active={route.section === "usage"} /></div>
      <div className="route-layer" hidden={route.section !== "batches"}><BatchRunnerPanel /></div>
      <div className={`route-layer${route.section === "runs" ? " activity-workspace" : ""}`} hidden={route.section !== "runs"}>{desktop ? <RunsPanel mode="activity" issue={null} project={selectedProject ?? undefined} /> : <section className="panel empty-state"><h2>Activity</h2><p>Run history is available in the desktop app.</p></section>}</div>
      <div className="route-layer settings-page" hidden={route.section !== "settings"}>
        <div className="settings-page-heading"><div><p className="eyebrow">WORKSPACE</p><h1>Settings</h1></div><button className="button button-primary" type="button" onClick={scanning ? stopScan : () => { void beginScan(); }} disabled={scanStatus === "cancelling"}>{scanning ? "Cancel scan" : projects.length ? "Refresh projects" : "Discover projects"}</button></div>
        <p role="status">{scanning ? "Discovering folders…" : scanError || freshnessLabel}</p>
        {Boolean(scanResult?.errors.length) && <details className="panel"><summary>Folders with inspection errors</summary><ul>{scanResult?.errors.map((error, index) => <li key={index}>{error}</li>)}</ul></details>}
        <details className="settings-section" open><summary>Projects</summary><ProjectCollection projects={projects} manage onRefreshProject={path => { void beginAffectedRefresh([path]); }} refreshingPaths={refreshingPaths} refreshErrorPaths={refreshErrorPaths} refreshVersions={projectRefreshVersions} /></details>
        <details className="settings-section"><summary>Providers</summary><ProvidersPanel /></details>
        <details className="settings-section"><summary>Workspace</summary><SettingsPanel /></details>
      </div>
      <div className="route-layer" hidden={route.section !== "providers"}><ProvidersPanel /></div>
    </main>
  </div>;
}

export default App;
