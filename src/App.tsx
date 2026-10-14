import { useEffect, useState, type FormEvent } from "react";

type Section = "projects" | "runs" | "providers" | "settings";
type PreviewState = "ready" | "loading" | "empty" | "error";

const sections: { id: Section; label: string; icon: string }[] = [
  { id: "projects", label: "Projects", icon: "◫" },
  { id: "runs", label: "Runs", icon: "↗" },
  { id: "providers", label: "Providers", icon: "◇" },
  { id: "settings", label: "Settings", icon: "⚙" },
];

const settingsKey = "navis-fabrica.settings.v1";

function readRootPath() {
  try {
    const saved = window.localStorage.getItem(settingsKey);
    if (saved) {
      const parsed: unknown = JSON.parse(saved);
      if (typeof parsed === "object" && parsed !== null && "rootPath" in parsed && typeof parsed.rootPath === "string") {
        return parsed.rootPath;
      }
    }
  } catch {
    // Storage may be unavailable in private browser contexts; keep the usable default.
  }
  return "C:\\Projects";
}

function sectionFromHash(): Section {
  const candidate = window.location.hash.slice(1);
  return sections.some((section) => section.id === candidate) ? (candidate as Section) : "projects";
}

function App() {
  const [section, setSection] = useState<Section>(sectionFromHash);
  const [rootPath, setRootPath] = useState(readRootPath);
  const [saved, setSaved] = useState(false);
  const [preview, setPreview] = useState<PreviewState>(() => {
    const candidate = new URLSearchParams(window.location.search).get("demo");
    return candidate === "loading" || candidate === "empty" || candidate === "error" ? candidate : "ready";
  });

  useEffect(() => {
    const handleNavigation = () => setSection(sectionFromHash());
    window.addEventListener("hashchange", handleNavigation);
    return () => window.removeEventListener("hashchange", handleNavigation);
  }, []);

  useEffect(() => {
    document.title = `Navis-Fabrica · v${__APP_VERSION__}`;
  }, []);

  function saveSettings(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    try {
      window.localStorage.setItem(settingsKey, JSON.stringify({ rootPath: rootPath.trim() || "C:\\Projects" }));
      setRootPath((current) => current.trim() || "C:\\Projects");
      setSaved(true);
      window.setTimeout(() => setSaved(false), 2200);
    } catch {
      setSaved(false);
    }
  }

  const current = sections.find((item) => item.id === section) ?? sections[0];
  const collectionTitle = section === "projects" ? "Your projects" : section === "runs" ? "Recent runs" : "Connected providers";
  const emptyTitle = preview === "empty"
    ? section === "projects" ? "No projects here yet" : section === "runs" ? "No runs yet" : "No providers connected"
    : "A little room to grow";
  const emptyDescription = preview === "empty"
    ? section === "projects" ? "Add a project to begin shaping your workspace." : section === "runs" ? "Completed and active work will appear here." : "Provider connections will appear here once configured."
    : "This section is ready for a future phase.";

  return (
    <div className="app-shell">
      <a className="skip-link" href="#main-content">Skip to content</a>
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
              {item.id === "projects" && <span className="nav-indicator" aria-label="Current section" />}
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
          <div className="toolbar-actions">
            <label className="preview-control">
              <span>Preview state</span>
              <select aria-label="Preview a fixture state" value={preview} onChange={(event) => setPreview(event.target.value as PreviewState)}>
                <option value="ready">Default</option><option value="loading">Loading</option><option value="empty">Empty</option><option value="error">Error</option>
              </select>
            </label>
            <span className="toolbar-divider" />
            <button className="avatar" type="button" aria-label="Local profile">K</button>
          </div>
        </header>

        <main id="main-content" className="content" tabIndex={-1}>
          <div className="page-heading">
            <div>
              <p className="eyebrow">YOUR WORKSPACE</p>
              <h1>{section === "projects" ? "Projects" : current.label}</h1>
              <p className="page-subtitle">{section === "projects" ? "A clear view of the work taking shape." : section === "settings" ? "Make this workspace feel like yours." : `${current.label} will live here as the foundation grows.`}</p>
            </div>
            {section === "projects" && <button className="button button-primary" type="button" disabled title="Project discovery arrives in a later phase"><span aria-hidden="true">＋</span> Add project</button>}
          </div>

          {section === "settings" ? (
            <section className="panel settings-panel" aria-labelledby="root-setting-title">
              <div className="panel-heading"><div><h2 id="root-setting-title">Project location</h2><p>Choose the folder Navis-Fabrica will use to find projects.</p></div><span className="setting-glyph" aria-hidden="true">⌂</span></div>
              <form className="settings-form" onSubmit={saveSettings}>
                <label htmlFor="root-path">Projects folder</label>
                <div className="input-row"><input id="root-path" value={rootPath} onChange={(event) => setRootPath(event.target.value)} placeholder="C:\\Projects" autoComplete="off" /><button className="button button-primary" type="submit">Save location</button></div>
                <p className={`form-feedback${saved ? " visible" : ""}`} role="status">{saved ? "Location saved on this device." : "Saved locally in this browser profile."}</p>
              </form>
            </section>
          ) : (
            <section className="panel project-panel" aria-labelledby="project-list-title">
              <div className="panel-heading panel-heading-row">
                <div><h2 id="project-list-title">{collectionTitle}</h2><p>{section === "projects" ? <>Projects in <code>{rootPath}</code></> : "This workspace section is part of the foundation shell."}</p></div>
                <button className="icon-button" type="button" aria-label="More project options" disabled>···</button>
              </div>
              <div className="panel-rule" />
              {preview === "loading" ? (
                <div className="state-box loading-state" role="status"><span className="spinner" aria-hidden="true" /><div><strong>Finding your projects</strong><p>Looking through your workspace…</p></div></div>
              ) : preview === "error" ? (
                <div className="state-box error-state" role="alert"><span className="state-symbol" aria-hidden="true">!</span><div><strong>We couldn’t open this folder</strong><p>Check that the location exists and that this app can access it.</p><button className="text-button" type="button" onClick={() => setPreview("ready")}>Try again</button></div></div>
              ) : (
                <div className="empty-state">
                  <div className="empty-illustration" aria-hidden="true"><span className="orbit orbit-one" /><span className="orbit orbit-two" /><span className="orbit orbit-three" /><span className="empty-spark spark-one">✦</span><span className="empty-spark spark-two">·</span><span className="folder-shape"><span /></span></div>
                  <h3>{emptyTitle}</h3>
                  <p>{emptyDescription}</p>
                  {section === "projects" && <button className="button button-secondary" type="button" disabled title="Project discovery arrives in a later phase">＋ Add your first project</button>}
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
