import { useEffect, useId, useMemo, useRef, useState } from "react";
import { issueCommand, type IssuePlan, type IssuePage, type IssueRecord, type ListedIssue } from "./runnerClient";
import { IssueBody } from "./IssueBody";
import { isDesktopApp } from "./settings";

const text = (value: unknown) => typeof value === "string" ? value : "";
const ids = (value: unknown): string[] => Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
const statuses = ["open", "ready", "in_progress", "in_review", "blocked", "done", "closed"];

export function LocalIssuesPanel({ defaultPath, refreshToken, initialType = "", autoLoad = false, title = "Local issues", navigationKey }: { defaultPath: string; refreshToken?: unknown; initialType?: "" | "epic" | "story"; autoLoad?: boolean; title?: string; navigationKey?: string }) {
  const headingId = useId();
  const [project, setProject] = useState(defaultPath);
  const [config, setConfig] = useState(`${defaultPath}\\runner.json`);
  const [loadedScope, setLoadedScope] = useState<{ project: string; config: string } | null>(null);
  const [issues, setIssues] = useState<ListedIssue[]>([]);
  const [selected, setSelected] = useState<ListedIssue | null>(null);
  const [draft, setDraft] = useState<IssueRecord | null>(null);
  const [parentText, setParentText] = useState("");
  const [dependencyText, setDependencyText] = useState("");
  const [page, setPage] = useState(0);
  const [editing, setEditing] = useState(false);
  const [issueType, setIssueType] = useState<"" | "epic" | "story">("");
  const [collapsedEpics, setCollapsedEpics] = useState<Set<string>>(() => new Set());
  useEffect(() => { setIssueType(""); }, [initialType]);
  useEffect(() => { if (autoLoad && isDesktopApp()) void load(); }, []);
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState("");
  const [priority, setPriority] = useState("");
  const [parent, setParent] = useState("");
  const [plan, setPlan] = useState<IssuePlan | null>(null);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [epicProgressStale, setEpicProgressStale] = useState(false);
  const [busy, setBusy] = useState(false);
  const requestGeneration = useRef(0);
  const dirty = Boolean(selected && draft && JSON.stringify(selected.record) !== JSON.stringify(draft));
  const scopeChanged = Boolean(loadedScope && (loadedScope.project !== project || loadedScope.config !== config));
  const previousDefault = useRef(defaultPath);
  useEffect(() => {
    const previous = previousDefault.current;
    previousDefault.current = defaultPath;
    if (!loadedScope && !dirty && project === previous && config === `${previous}\\runner.json`) {
      setProject(defaultPath);
      setConfig(`${defaultPath}\\runner.json`);
    }
  }, [defaultPath]);
  const priorities = [...new Set(issues.map(item => text(item.record.frontmatter.priority)).filter(Boolean))].sort();
  const parents = [...new Set(issues.flatMap(item => ids(item.record.frontmatter.parent)))].sort();
  const visible = useMemo(() => issues.filter(({ record }) => {
    const f = record.frontmatter;
    return (!issueType || (issueType === "epic" ? f.type === "epic" : f.type !== "epic")) && (!status || f.status === status) && (!priority || f.priority === priority) && (!parent || ids(f.parent).includes(parent)) && `${text(f.id)} ${text(f.title)}`.toLowerCase().includes(query.toLowerCase());
  }), [issues, query, status, priority, parent, issueType]);
  useEffect(() => setPage(0), [query, status, priority, parent, issueType]);
  useEffect(() => {
    if (!dirty) return;
    const warn = (event: BeforeUnloadEvent) => { event.preventDefault(); };
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [dirty]);
  useEffect(() => {
    const key = navigationKey ?? `${project}\u0000${title}`;
    const notify = (value: boolean) => { window.dispatchEvent(new CustomEvent("fabrica-issue-edit-state", { detail: { key, dirty: value } })); };
    notify(dirty);
    return () => notify(false);
  }, [dirty, navigationKey, project, title]);

  useEffect(() => {
    if (refreshToken && isDesktopApp() && !dirty && !busy && !scopeChanged && project === defaultPath && config === `${defaultPath}\\runner.json`) void load(true);
  }, [refreshToken]);

  async function load(preserveEditor = false) {
    if (dirty) { setError("Save or discard the current edits before opening another backlog."); return; }
    const generation = ++requestGeneration.current;
    setBusy(true); setError(""); setNotice("");
    try {
      const result: ListedIssue[] = [];
      let total = 1;
      for (let offset = 0; offset < total; offset += 100) {
        const response = await issueCommand<IssuePage>("issues-page", project, config, { offset, limit: 100 });
        if (response.total > 5000 || response.items.length > 100 || response.offset !== offset) throw new Error("Invalid or oversized issue page");
        total = response.total;
        result.push(...response.items.map(item => ({ record: { frontmatter: { id: item.id, title: item.title, status: item.status, type: item.type, priority: item.priority, parent: item.parent, depends_on: item.dependsOn }, body: "" }, revision: item.revision, unresolvedDependencies: item.unresolvedDependencies, unresolvedParents: item.unresolvedParents, epicProgress: item.epicProgress })));
      }
      if (generation !== requestGeneration.current) return;
      setPage(0); setIssues(result); setLoadedScope({ project, config });
      if (!preserveEditor) { setSelected(null); setDraft(null); setPlan(null); }
      setEpicProgressStale(false);
      setNotice("");
    } catch (reason) { setError(String(reason)); } finally { setBusy(false); }
  }
  async function select(item: ListedIssue) {
    if (dirty) { setError("Save or discard the current edits before switching issues."); return; }
    if (!loadedScope || scopeChanged) return;
    setBusy(true); setError(""); setNotice("");
    try {
      const current = await issueCommand<ListedIssue>("issues-show", loadedScope.project, loadedScope.config, { issueId: text(item.record.frontmatter.id) });
      setParentText(ids(current.record.frontmatter.parent).join(", ")); setDependencyText(ids(current.record.frontmatter.depends_on).join(", "));
      setEditing(false); setSelected(current); setDraft(structuredClone(current.record)); setPlan(null);
      if (current.revision !== item.revision) setNotice("This issue changed since the list was loaded; showing the latest version. Refresh to update the list.");
    } catch (reason) { setError(String(reason)); } finally { setBusy(false); }
  }
  function change(key: string, value: unknown) {
    setDraft(previous => previous ? { ...previous, frontmatter: { ...previous.frontmatter, [key]: value } } : previous);
  }
  async function save() {
    if (!selected || !draft || !loadedScope || scopeChanged) return;
    setBusy(true); setError("");
    try {
      const result = await issueCommand<ListedIssue>("issues-update", loadedScope.project, loadedScope.config, { expectedRevision: selected.revision, record: draft });
      const epicProgressMayChange = text(selected.record.frontmatter.type) === "epic"
        || text(result.record.frontmatter.type) === "epic"
        || ids(selected.record.frontmatter.parent).length > 0
        || ids(result.record.frontmatter.parent).length > 0;
      setIssues(items => items.map(item => item.record.frontmatter.id === result.record.frontmatter.id
        ? { ...result, epicProgress: text(result.record.frontmatter.type) === "epic" ? item.epicProgress : undefined }
        : item));
      if (epicProgressMayChange) setEpicProgressStale(true);
      setEditing(false); setSelected(result); setDraft(structuredClone(result.record)); setPlan(null); setNotice(epicProgressMayChange ? "Saved. Refresh to update epic progress." : "Saved.");
    } catch (reason) { setError(`${String(reason)}. Your edits are retained. Discard and refresh to load external changes.`); } finally { setBusy(false); }
  }
  async function preview() {
    if (!selected || !loadedScope || scopeChanged || dirty) return;
    setBusy(true); setError("");
    try { setPlan(await issueCommand<IssuePlan>("plan", loadedScope.project, loadedScope.config, { issueId: text(selected.record.frontmatter.id) })); }
    catch (reason) { setError(String(reason)); } finally { setBusy(false); }
  }
  const epics = issues.filter(item => item.record.frontmatter.type === "epic");
  const groups: { epic: ListedIssue | null; children: ListedIssue[] }[] = epics.map(epic => ({ epic, children: visible.filter(item => item.record.frontmatter.type !== "epic" && ids(item.record.frontmatter.parent).includes(text(epic.record.frontmatter.id))) })).filter(group => visible.includes(group.epic) || group.children.length > 0);
  const other = visible.filter(item => item.record.frontmatter.type !== "epic" && !ids(item.record.frontmatter.parent).some(id => epics.some(epic => epic.record.frontmatter.id === id)));
  if (other.length) groups.push({ epic: null, children: other });
  function relation(id: string) {
    const item = issues.find(item => item.record.frontmatter.id === id);
    return item ? <button key={id} className="issue-relation" disabled={busy || scopeChanged} onClick={() => void select(item)}>{text(item.record.frontmatter.title)}</button> : <span key={id}>{id} (unavailable)</span>;
  }
  function issueButton(item: ListedIssue) {
    const id = text(item.record.frontmatter.id);
    const statusLabel = text(item.record.frontmatter.status).replaceAll("_", " ");
    return <button type="button" className="local-issue-item" key={id} aria-pressed={selected?.record.frontmatter.id === id} disabled={busy || scopeChanged} onClick={() => void select(item)}>
      <strong title={text(item.record.frontmatter.title)}>{text(item.record.frontmatter.title)}</strong>
      <span className="issue-row-meta"><span className="issue-row-id">{id}</span><span className={`issue-status-badge issue-status-${statusLabel.replaceAll(" ", "-")}`}>{statusLabel}</span></span>
    </button>;
  }
  if (!isDesktopApp()) return <section className="panel"><h2>{title}</h2><p>Open Fabrica desktop to read and edit local Markdown issues.</p></section>;
  return <section className="panel local-issues" aria-labelledby={headingId}>
    <h2 id={headingId}>{title}</h2>
    <div className="local-issues-filters"><label>Search<input type="search" value={query} onChange={event => setQuery(event.target.value)} placeholder="Epics and stories" /></label><label>Status<select value={status} onChange={event => setStatus(event.target.value)}><option value="">All statuses</option>{statuses.map(value => <option key={value} value={value}>{value.replaceAll("_", " ")}</option>)}</select></label><button className="button button-secondary" disabled={busy || dirty || !project || !config} onClick={() => void load()}>Refresh</button><details className="issue-extra-filters"><summary>Filters</summary><label>Type<select value={issueType} onChange={event => setIssueType(event.target.value as "" | "epic" | "story")}><option value="">All</option><option value="epic">Epics</option><option value="story">Stories</option></select></label><label>Priority<select value={priority} onChange={event => setPriority(event.target.value)}><option value="">All priorities</option>{priorities.map(value => <option key={value}>{value}</option>)}</select></label><label>Epic<select value={parent} onChange={event => setParent(event.target.value)}><option value="">All epics</option>{parents.map(value => <option key={value} value={value}>{text(issues.find(item => item.record.frontmatter.id === value)?.record.frontmatter.title) || value}</option>)}</select></label></details><details><summary>Settings</summary><div className="local-issues-paths"><label>Project folder<input value={project} onChange={event => setProject(event.target.value)} disabled={busy || dirty} /></label><label>Runner config<input value={config} onChange={event => setConfig(event.target.value)} disabled={busy || dirty} /></label></div></details></div>
    {scopeChanged && <p role="status">Refresh to open the selected project.</p>}
    {error && <p role="alert" className="scan-notice">{error}</p>}{notice && <p role="status">{notice}</p>}{epicProgressStale && <p role="status">Refresh to update epic progress.</p>}
    <div className="local-issues-columns"><nav className="local-issues-list" aria-label="Epics and stories">
      <div className="issue-list-heading"><h3>Backlog</h3><span>{visible.length}</span></div>
      {groups.slice(page * 100, (page + 1) * 100).map(({ epic, children }) => {
        if (!epic) return <section className="issue-other-stories" key="other"><h4>Other stories</h4>{children.map(issueButton)}</section>;
        const id = text(epic.record.frontmatter.id);
        const epicTitle = text(epic.record.frontmatter.title);
        const collapsed = collapsedEpics.has(id) && !query.trim();
        const progress = epic.epicProgress;
        const accepted = progress?.acceptedChildren ?? 0;
        const total = progress?.totalChildren ?? children.length;
        const childrenId = `${headingId}-epic-children-${encodeURIComponent(id)}`;
        return <section className="issue-epic" key={id}>
          <div className="issue-epic-heading">
            <button className="issue-epic-toggle" type="button" aria-label={`${collapsed ? "Expand" : "Collapse"} ${epicTitle}`} aria-expanded={!collapsed} aria-controls={childrenId} onClick={() => setCollapsedEpics(current => { const next = new Set(current); if (next.has(id)) next.delete(id); else next.add(id); return next; })}>
              <svg viewBox="0 0 16 16" aria-hidden="true"><path d="m5 6 3 3 3-3" /></svg>
            </button>
            <div className="issue-epic-identity"><span className="issue-epic-kicker">Epic</span><span className="issue-epic-id">{id}</span></div>
            <button type="button" className="issue-epic-title" title={epicTitle} aria-pressed={selected?.record.frontmatter.id === id} disabled={busy || scopeChanged} onClick={() => void select(epic)}>{epicTitle}</button>
            {progress && <span className="issue-epic-progress">{accepted}/{total}</span>}
          </div>
          {progress && <progress className="issue-epic-progress-bar" aria-label={`${epicTitle} progress`} value={accepted} max={total || 1} />}
          <div className="issue-children" id={childrenId} hidden={collapsed}>{children.map(issueButton)}</div>
        </section>;
      })}
      {!groups.length && <p>{busy ? "Loading…" : issues.length ? "No matching epics or stories." : "No epics or stories yet."}</p>}
      {groups.length > 100 && <div className="local-issues-actions"><button className="button button-secondary" disabled={page === 0} onClick={() => setPage(value => value - 1)}>Previous</button><span>{page + 1} / {Math.ceil(groups.length / 100)}</span><button className="button button-secondary" disabled={(page + 1) * 100 >= groups.length} onClick={() => setPage(value => value + 1)}>Next</button></div>}
    </nav>
    <div>{draft && selected ? editing ? <fieldset disabled={busy} className="local-issue-editor"><h3>{text(draft.frontmatter.id)}</h3>{dirty && <p role="status">Unsaved edits</p>}<label>Title<input value={text(draft.frontmatter.title)} onChange={event => change("title", event.target.value)} disabled={busy} /></label><label>Status<select value={text(draft.frontmatter.status)} onChange={event => change("status", event.target.value)} disabled={busy}>{statuses.map(value => <option key={value}>{value}</option>)}</select></label><label>Priority<input value={text(draft.frontmatter.priority)} onChange={event => change("priority", event.target.value || null)} disabled={busy} /></label><label>Parents (comma separated)<input value={parentText} onChange={event => { setParentText(event.target.value); change("parent", event.target.value.split(",").map(value => value.trim()).filter(Boolean)); }} disabled={busy} /></label><label>Dependencies (comma separated)<input value={dependencyText} onChange={event => { setDependencyText(event.target.value); change("depends_on", event.target.value.split(",").map(value => value.trim()).filter(Boolean)); }} disabled={busy} /></label><label>Markdown body<textarea value={draft.body} onChange={event => setDraft({ ...draft, body: event.target.value })} disabled={busy} rows={20} /></label><div className="local-issues-actions"><button className="button button-primary" disabled={busy || !dirty || scopeChanged} onClick={() => void save()}>Save</button><button className="button button-secondary" disabled={busy} onClick={() => { setEditing(false); setDraft(structuredClone(selected.record)); setParentText(ids(selected.record.frontmatter.parent).join(", ")); setDependencyText(ids(selected.record.frontmatter.depends_on).join(", ")); setError(""); }}>Cancel</button><button className="button button-secondary" disabled={busy || dirty || scopeChanged} onClick={() => void preview()}>Preview dependencies</button></div>{plan && <div role="status"><h4>{plan.readyToStart ? "Dependencies ready" : "Work is blocked or not ready"}</h4>{plan.dependencies.map(dep => <p key={dep.id}>{dep.id}: {dep.state} · {dep.satisfied ? "satisfied" : "blocked"}</p>)}<p>Preview only. No model calls or job launch.</p></div>}</fieldset> : <article className="issue-reader"><header><span className="muted">{text(draft.frontmatter.id)}</span><h3>{text(draft.frontmatter.title)}</h3><div className="local-issues-actions"><span className="issue-status">{text(draft.frontmatter.status).replaceAll("_", " ")}</span><button className="button button-secondary" disabled={busy || scopeChanged} onClick={() => setEditing(true)}>Edit</button></div></header>{ids(draft.frontmatter.parent).length > 0 && <p className="issue-relations">Epic: {ids(draft.frontmatter.parent).map(id => relation(id))}</p>}{ids(draft.frontmatter.depends_on).length > 0 && <details className="issue-relations"><summary>Dependencies · {ids(draft.frontmatter.depends_on).length}</summary>{ids(draft.frontmatter.depends_on).map(id => relation(id))}</details>}{selected.unresolvedDependencies.length > 0 && <p role="status" className="scan-notice">Missing dependencies: {selected.unresolvedDependencies.join(", ")}</p>}<IssueBody body={draft.body} title={text(draft.frontmatter.title)} issueId={text(draft.frontmatter.id)} issues={issues} onSelect={item => void select(item)} /></article> : <p className="issue-empty">{busy ? "Loading…" : "Select an epic or story to read it."}</p>}</div></div>
  </section>;
}
