import { useWorkflowActivity, useBatchActivity, activityLabel, sameProjectPath } from "./Activity";
import { useEffect, useState } from "react";
import { getProjectIcon, loadSettings, saveProjectDisplayPreferences, defaultProjectDisplayPreferences, isDesktopApp, type ProjectEntry, type WorkflowRun } from "./settings";
import { issueCommand, type IssuePage } from "./runnerClient";
import { LocalIssuesPanel } from "./LocalIssuesPanel";

export function ProjectCollection({ projects, manage = false, onSelect }: { projects: ProjectEntry[]; manage?: boolean; onSelect: (id: string) => void }) {
  const [preferences, setPreferences] = useState(defaultProjectDisplayPreferences);
  const [ready, setReady] = useState(false);
  const [message, setMessage] = useState("");
  const [query, setQuery] = useState("");
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
  const visible = unique.filter(p => (manage || preferences.favoriteProjectIds.includes(p.id)) && (!manage || `${p.name} ${p.path}`.toLowerCase().includes(query.toLowerCase())));
  return <section className="my-projects">
    {manage && <><h2>My Projects collection</h2><p>Choose your projects. Discovered clones stay outside your collection until you select them. Existing favorites are your initial saved selection.</p></>}
    {manage && <label className="settings-field"><span>Find a project to include</span><input type="search" value={query} onChange={e => setQuery(e.target.value)} /></label>}
    {message && <p role="status">{message}</p>}
    {!ready ? <p>Loading saved collection…</p> : <>
      {manage ? <><div className="collection-options">{visible.map(p => <label key={p.id}><input type="checkbox" disabled={saving} checked={preferences.favoriteProjectIds.includes(p.id)} onChange={e => setPreferences({ ...preferences, favoriteProjectIds: e.target.checked ? [...preferences.favoriteProjectIds, p.id] : preferences.favoriteProjectIds.filter(id => id !== p.id) })} /><span><strong>{p.name}</strong><small>{p.path}</small></span></label>)}</div>
      {preferences.favoriteProjectIds.filter(id => !unique.some(p => p.id === id)).map(id => <label className="collection-missing" key={id}><input type="checkbox" checked disabled={saving} onChange={() => setPreferences({ ...preferences, favoriteProjectIds: preferences.favoriteProjectIds.filter(item => item !== id) })} />Unavailable saved project: {id}</label>)}
      {!visible.length && <p>No discovered candidates match. Use Discover project folders in Settings or review the folder below.</p>}
      <button className="button button-primary" disabled={saving} onClick={() => void save()}>{saving ? "Saving…" : "Save My Projects"}</button></> : <>
      {!visible.length && <div className="empty-state"><h3>{preferences.favoriteProjectIds.length ? "No available projects match" : "Choose your projects"}</h3><p>Your collection only includes projects you select.</p><a href="#settings">Manage My Projects in Settings</a></div>}
      <div className="my-project-grid" role="list" aria-label={manage ? "Project candidates" : "My Projects"}>{visible.map(p => <div role="listitem" key={p.id}><ProjectCard project={p} runs={runs} selected={p.id === selected} onClick={() => { setSelected(p.id); setOpenedIds(ids => ids.includes(p.id) ? ids : [...ids, p.id]); onSelect(p.id); }} /></div>)}</div>
      {unique.filter(project => openedIds.includes(project.id)).map(project => <section key={project.id} hidden={project.id !== selected} className="project-inspector panel" aria-label={`${project.name} details`}><div className="panel-heading"><h2>{project.name}</h2><button className="text-button" onClick={() => setSelected(null)}>Close details</button></div><p>{project.path}</p><h3>Changed files</h3>{project.git.changedFiles == null ? <p>Changed files unavailable. Reopen the app or rediscover folders in Settings to inspect current Git state.</p> : project.git.changedFiles.length ? <ul>{project.git.changedFiles.map(path => <li key={path}><code>{path}</code></li>)}</ul> : <p>Working tree clean.</p>}<div className="project-actions"><a className="button button-secondary" href="#runs">Project actions & activity</a><a className="button button-secondary" href="#batches">Run local issue batch</a></div><h3>Local issues</h3><LocalIssuesPanel key={project.id} defaultPath={project.path} refreshToken={project} /></section>)}
      </>}
    </>}
  </section>;
}

export function ProjectCard({ project, runs, selected, onClick }: { project: ProjectEntry; runs: WorkflowRun[] | null; selected: boolean; onClick: () => void }) {
  const batch = useBatchActivity();
  const [icon, setIcon] = useState<string | null>(null);
  const [issues, setIssues] = useState<string>("Loading…");
  const [issueError, setIssueError] = useState("");
  useEffect(() => {
    let alive = true;
    setIcon(null);
    if (!isDesktopApp()) return;
    getProjectIcon(project.id).then(value => { if (alive) setIcon(value); }).catch(() => { if (alive) setIcon(null); });
    return () => { alive = false; };
  }, [project.id, project.path]);
  useEffect(() => {
    let alive = true;
    setIssues("Loading…"); setIssueError("");
    if (!isDesktopApp()) { setIssues("Desktop required"); return; }
    issueCommand<IssuePage>("issues-page", project.path, `${project.path}\\runner.json`, { limit: 1, offset: 0 }).then(page => { if (alive) setIssues(String(page.total)); }).catch(e => { if (alive) { setIssues("0"); setIssueError(String(e)); } });
    return () => { alive = false; };
  }, [project]);
  const relevant = runs?.filter(run => sameProjectPath(run.cwd, project.path)) ?? null;
  return <button className={`my-project-card${selected ? " selected" : ""}`} onClick={onClick} aria-pressed={selected}>
    <ProjectCardHeading name={project.name} icon={icon} /><small>{project.git.branch ?? "Branch unavailable"}{project.isWorktree ? " · Worktree" : ""}</small>
    <dl><div><dt>Changed files</dt><dd>{project.git.changedFiles?.length ?? "Unavailable"}</dd></div><div title={issueError}><dt>Local issues</dt><dd>{issues}</dd></div></dl>
    <span>{activityLabel(relevant)}{batch?.projectPath && sameProjectPath(batch.projectPath, project.path) ? ` · Batch ${batch.needsAttention ? "needs attention" : batch.state}` : ""}</span>
  </button>;
}

export function ProjectCardHeading({ name, icon }: { name: string; icon: string | null }) {
  return <div className="my-project-card-heading">{icon && <img className="my-project-icon" src={icon} alt="" />}<h3>{name}</h3></div>;
}
