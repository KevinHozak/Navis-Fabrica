import { useEffect, useMemo, useRef, useState } from "react";
import { issueCommand, type IssuePlan, type IssuePage, type IssueRecord, type ListedIssue } from "./runnerClient";
import { isDesktopApp } from "./settings";

const text = (value: unknown) => typeof value === "string" ? value : "";
const ids = (value: unknown): string[] => Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
const statuses = ["open", "ready", "in_progress", "in_review", "blocked", "done", "closed"];

export function LocalIssuesPanel({ defaultPath }: { defaultPath: string }) {
  const [project, setProject] = useState(defaultPath);
  const [config, setConfig] = useState(`${defaultPath}\\runner.json`);
  const [loadedScope, setLoadedScope] = useState<{ project: string; config: string } | null>(null);
  const [issues, setIssues] = useState<ListedIssue[]>([]);
  const [selected, setSelected] = useState<ListedIssue | null>(null);
  const [draft, setDraft] = useState<IssueRecord | null>(null);
  const [parentText, setParentText] = useState("");
  const [dependencyText, setDependencyText] = useState("");
  const [page, setPage] = useState(0);
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
    return (!status || f.status === status) && (!priority || f.priority === priority) && (!parent || ids(f.parent).includes(parent)) && `${text(f.id)} ${text(f.title)}`.toLowerCase().includes(query.toLowerCase());
  }), [issues, query, status, priority, parent]);
  useEffect(() => setPage(0), [query, status, priority, parent]);
  useEffect(() => {
    if (!dirty) return;
    const warn = (event: BeforeUnloadEvent) => { event.preventDefault(); };
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [dirty]);

  async function load() {
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
      setPage(0); setIssues(result); setLoadedScope({ project, config }); setSelected(null); setDraft(null); setPlan(null);
      setEpicProgressStale(false);
      setNotice(`Loaded ${result.length} issues. Refresh to check external Markdown edits.`);
    } catch (reason) { setError(String(reason)); } finally { setBusy(false); }
  }
  async function select(item: ListedIssue) {
    if (dirty) { setError("Save or discard the current edits before switching issues."); return; }
    if (!loadedScope || scopeChanged) return;
    setBusy(true); setError(""); setNotice("");
    try {
      const current = await issueCommand<ListedIssue>("issues-show", loadedScope.project, loadedScope.config, { issueId: text(item.record.frontmatter.id) });
      setParentText(ids(current.record.frontmatter.parent).join(", ")); setDependencyText(ids(current.record.frontmatter.depends_on).join(", "));
      setSelected(current); setDraft(structuredClone(current.record)); setPlan(null);
      if (current.revision !== item.revision) setNotice("This issue changed since the list was loaded; the editor shows its current Markdown. Refresh the list to update filters.");
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
      setSelected(result); setDraft(structuredClone(result.record)); setPlan(null); setNotice(epicProgressMayChange ? "Markdown saved and read back. Refresh the backlog to recalculate epic progress." : "Markdown saved and read back.");
    } catch (reason) { setError(`${String(reason)}. Your edits are retained. Discard and refresh to load external changes.`); } finally { setBusy(false); }
  }
  async function preview() {
    if (!selected || !loadedScope || scopeChanged || dirty) return;
    setBusy(true); setError("");
    try { setPlan(await issueCommand<IssuePlan>("plan", loadedScope.project, loadedScope.config, { issueId: text(selected.record.frontmatter.id) })); }
    catch (reason) { setError(String(reason)); } finally { setBusy(false); }
  }
  if (!isDesktopApp()) return <section className="panel"><h2>Local issues</h2><p>Open Fabrica desktop to read and edit local Markdown issues.</p></section>;
  return <section className="panel local-issues" aria-labelledby="local-issues-title">
    <h2 id="local-issues-title">Local issues</h2><p>Markdown is authoritative. Open any local project, including projects without GitHub.</p>
    <div className="local-issues-paths"><label>Project folder<input value={project} onChange={event => setProject(event.target.value)} disabled={busy} /></label><label>Runner config file<input value={config} onChange={event => setConfig(event.target.value)} disabled={busy} /></label><button className="button button-secondary" disabled={busy || dirty || !project || !config} onClick={() => void load()}>Open / refresh</button></div>
    <p className="muted">Config: schemaVersion 1, issueScope (such as GP), and issuesDir relative to the project.</p>
    {scopeChanged && <p role="status">Folder settings changed. Open the backlog to apply them.</p>}
    {error && <p role="alert" className="scan-notice">{error}</p>}{notice && <p role="status">{notice}</p>}{epicProgressStale && <p role="status" className="scan-notice">Epic progress may be stale after an issue save. Open / refresh the backlog to recalculate it.</p>}
    <div className="local-issues-filters"><label>Search IDs and titles<input type="search" value={query} onChange={event => setQuery(event.target.value)} /></label><label>Status<select value={status} onChange={event => setStatus(event.target.value)}><option value="">All statuses</option>{statuses.map(value => <option key={value}>{value}</option>)}</select></label><label>Priority<select value={priority} onChange={event => setPriority(event.target.value)}><option value="">All priorities</option>{priorities.map(value => <option key={value}>{value}</option>)}</select></label><label>Epic / parent<select value={parent} onChange={event => setParent(event.target.value)}><option value="">All parents</option>{parents.map(value => <option key={value}>{value}</option>)}</select></label></div>
    <p role="status">{visible.length} of {issues.length} issues · Page {page + 1} of {Math.max(1, Math.ceil(visible.length / 100))}</p><div className="local-issues-actions"><button className="button button-secondary" disabled={page === 0} onClick={() => setPage(value => value - 1)}>Previous page</button><button className="button button-secondary" disabled={(page + 1) * 100 >= visible.length} onClick={() => setPage(value => value + 1)}>Next page</button></div>
    <div className="local-issues-columns"><div className="local-issues-list" aria-label="Issues">{visible.slice(page * 100, (page + 1) * 100).map(item => <button type="button" className="local-issue-item" key={text(item.record.frontmatter.id)} aria-pressed={selected?.record.frontmatter.id === item.record.frontmatter.id} disabled={busy || scopeChanged} onClick={() => void select(item)}><strong>{text(item.record.frontmatter.id)} · {text(item.record.frontmatter.title)}</strong><span>{text(item.record.frontmatter.status)} · {text(item.record.frontmatter.priority)}</span>{item.epicProgress && <span>Epic progress: {item.epicProgress.acceptedChildren} of {item.epicProgress.totalChildren} direct children accepted{item.epicProgress.incompleteChildren.length > 0 ? ` · incomplete: ${item.epicProgress.incompleteChildren.join(", ")}` : item.epicProgress.totalChildren === 0 ? " · no direct children; cannot be complete" : item.epicProgress.complete ? " · all child checklists accepted" : ""}</span>}{item.unresolvedDependencies.length > 0 && <span>Missing dependencies: {item.unresolvedDependencies.join(", ")}</span>}</button>)}</div>
    <div>{draft && selected ? <div className="local-issue-editor"><h3>{text(draft.frontmatter.id)}</h3>{dirty && <p role="status">Unsaved edits</p>}<label>Title<input value={text(draft.frontmatter.title)} onChange={event => change("title", event.target.value)} disabled={busy} /></label><label>Status<select value={text(draft.frontmatter.status)} onChange={event => change("status", event.target.value)} disabled={busy}>{statuses.map(value => <option key={value}>{value}</option>)}</select></label><label>Priority<input value={text(draft.frontmatter.priority)} onChange={event => change("priority", event.target.value || null)} disabled={busy} /></label><label>Parents (comma separated)<input value={parentText} onChange={event => { setParentText(event.target.value); change("parent", event.target.value.split(",").map(value => value.trim()).filter(Boolean)); }} disabled={busy} /></label><label>Dependencies (comma separated)<input value={dependencyText} onChange={event => { setDependencyText(event.target.value); change("depends_on", event.target.value.split(",").map(value => value.trim()).filter(Boolean)); }} disabled={busy} /></label><label>Markdown body<textarea value={draft.body} onChange={event => setDraft({ ...draft, body: event.target.value })} disabled={busy} rows={20} /></label><div className="local-issues-actions"><button className="button button-primary" disabled={busy || !dirty || scopeChanged} onClick={() => void save()}>Save Markdown</button><button className="button button-secondary" disabled={busy || !dirty} onClick={() => { setDraft(structuredClone(selected.record)); setParentText(ids(selected.record.frontmatter.parent).join(", ")); setDependencyText(ids(selected.record.frontmatter.depends_on).join(", ")); setError(""); }}>Discard edits</button><button className="button button-secondary" disabled={busy || dirty || scopeChanged} onClick={() => void preview()}>Preview dependencies</button></div>{plan && <div role="status"><h4>{plan.readyToStart ? "Dependencies ready" : "Work is blocked or not ready"}</h4>{plan.dependencies.map(dep => <p key={dep.id}>{dep.id}: {dep.state} · {dep.satisfied ? "satisfied" : "blocked"}</p>)}<p>Preview only. No model calls or job launch.</p></div>}</div> : <p>Select an issue to view or edit it.</p>}</div></div>
  </section>;
}
