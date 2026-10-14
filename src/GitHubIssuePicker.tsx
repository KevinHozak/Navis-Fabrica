import { useEffect, useRef, useState } from "react";
import { getGitHubIssue, GitHubIssue, GitHubIssueSummary, listGitHubIssues } from "./settings";

export function GitHubIssuePicker({ owner, repository, onWork }: { owner: string; repository: string; onWork: (issue: GitHubIssue) => void }) {
  const [issues, setIssues] = useState<GitHubIssueSummary[]>([]);
  const [reference, setReference] = useState("");
  const [selected, setSelected] = useState<GitHubIssue | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const issueRequestId = useRef(0);

  useEffect(() => {
    let active = true;
    issueRequestId.current += 1;
    setIssues([]); setSelected(null); setReference(""); setError("");
    listGitHubIssues(owner, repository).then((items) => { if (active) setIssues(items); })
      .catch((reason: unknown) => { if (active) setError(errorMessage(reason)); });
    return () => { active = false; issueRequestId.current += 1; };
  }, [owner, repository]);

  async function loadIssue(value: string) {
    const requestId = ++issueRequestId.current;
    setLoading(true); setError(""); setSelected(null);
    try {
      const issue = await getGitHubIssue(owner, repository, value);
      if (issueRequestId.current === requestId) setSelected(issue);
    } catch (reason) {
      if (issueRequestId.current === requestId) setError(errorMessage(reason));
    } finally {
      if (issueRequestId.current === requestId) setLoading(false);
    }
  }

  function updateReference(value: string) {
    issueRequestId.current += 1;
    setReference(value);
    setSelected(null);
    setLoading(false);
    setError("");
  }

  return <section className="issue-picker" aria-label={`Issues for ${owner}/${repository}`}>
    <strong>Choose an issue</strong>
    <p>Pick an open issue from this repository or enter its number or GitHub issue URL.</p>
    <div className="issue-picker-entry">
      <label className="sr-only" htmlFor="issue-reference">Issue number or URL</label>
      <input id="issue-reference" value={reference} onChange={(event) => updateReference(event.target.value)} placeholder="Issue number or URL" onKeyDown={(event) => { if (event.key === "Enter" && reference.trim()) void loadIssue(reference); }} />
      <button className="button button-secondary" type="button" disabled={loading || !reference.trim()} onClick={() => { void loadIssue(reference); }}>{loading ? "Loading…" : "Open issue"}</button>
    </div>
    {issues.length > 0 && <div className="issue-picker-list" role="list" aria-label="Open repository issues">{issues.map((issue) => <div role="listitem" key={issue.number}><button type="button" onClick={() => { updateReference(String(issue.number)); void loadIssue(String(issue.number)); }}><span>#{issue.number} · {issue.title}</span><span aria-hidden="true">↗</span></button></div>)}</div>}
    {error && <p className="issue-feedback error" role="alert">{error}</p>}
    {selected && <IssueDetails issue={selected} onWork={() => onWork(selected)} />}
  </section>;
}

function IssueDetails({ issue, onWork }: { issue: GitHubIssue; onWork: () => void }) {
  return <div className="issue-detail" aria-live="polite">
    <h4><a href={issue.url} target="_blank" rel="noreferrer">#{issue.number} · {issue.title} ↗</a></h4>
    <span className={`issue-state ${issue.state === "open" ? "open" : "closed"}`}>{issue.state}</span>
    {issue.acceptanceCriteria.length > 0 && <div><strong>Acceptance criteria</strong><ul>{issue.acceptanceCriteria.map((item, index) => <li key={`${index}-${item}`}>{item}</li>)}</ul></div>}
    {issue.parentEpic && <div><strong>Parent epic</strong><a href={issue.parentEpic.url} target="_blank" rel="noreferrer">#{issue.parentEpic.number} · {issue.parentEpic.title}</a></div>}
    {issue.pullRequestUrls.length > 0 && <div><strong>PR context</strong>{issue.pullRequestUrls.map((url) => <a key={url} href={url} target="_blank" rel="noreferrer">{url.split("/").slice(-2).join("/")} ↗</a>)}</div>}
    {!issue.blockersAvailable && issue.state === "open" && <p className="issue-feedback" role="status">GitHub did not confirm blocker status, so this issue cannot start work yet.</p>}
    {issue.blockedBy.length > 0 && <div><strong>Blocked by</strong>{issue.blockedBy.map((blocker) => <a key={blocker.number} href={blocker.url} target="_blank" rel="noreferrer">#{blocker.number} · {blocker.title} ({blocker.state})</a>)}</div>}
    {issue.blockersTruncated && <p className="issue-feedback" role="status">There are more blockers than this view can show; work is disabled.</p>}
    {issue.state === "closed" && <p className="issue-feedback" role="status">Closed issues cannot be selected for work.</p>}
    {issue.blocked && issue.blockersAvailable && <p className="issue-feedback" role="status">Resolve the open blockers before starting work.</p>}
    <button className="button button-primary" type="button" disabled={!issue.selectable} onClick={onWork}>Work on this issue</button>
    <small>The issue is read-only here. Starting work does not edit it.</small>
  </div>;
}

function errorMessage(reason: unknown): string {
  const value = reason instanceof Error ? reason.message : String(reason);
  if (value.includes("unauthorized") || value.includes("authExpired")) return "GitHub could not provide this repository’s issues. Check the selected account and repository access.";
  if (value.includes("rateLimited")) return "GitHub’s rate limit was reached. Try again later.";
  if (value.includes("unavailable")) return "GitHub is unavailable right now. Local project details remain available.";
  return value;
}
