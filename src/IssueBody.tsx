import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { ListedIssue } from "./runnerClient";

export function withoutDuplicateIssueHeading(body: string, issueId: string, title: string): string {
  const lines = body.split(/\r?\n/);
  const headingIndex = lines.findIndex(line => line.trim() !== "");
  if (headingIndex < 0) return body;
  const match = lines[headingIndex].trim().match(/^#\s+(.+?)\s*#*$/);
  const heading = match?.[1]?.trim();
  if (heading !== title.trim() && heading !== `${issueId.trim()}: ${title.trim()}`) return body;
  lines.splice(headingIndex, 1);
  return lines.join("\n").replace(/^\s*\n/, "");
}

export function IssueBody({ body, title = "", issueId = "", issues, onSelect }: { body: string; title?: string; issueId?: string; issues: ListedIssue[]; onSelect: (item: ListedIssue) => void }) {
  const displayBody = title ? withoutDuplicateIssueHeading(body, issueId, title) : body;
  return <div className="issue-body"><Markdown remarkPlugins={[remarkGfm]} skipHtml components={{
    a({ href, children }) {
      const id = href?.match(/^(?:\.\/)?([^/\\?#]+)\.md(?:#[^\s]*)?$/)?.[1];
      const issue = issues.find(item => item.record.frontmatter.id === id);
      if (issue) return <button className="issue-relation" onClick={() => onSelect(issue)}>{children}</button>;
      return href && /^(https?:|mailto:)/i.test(href) ? <a href={href} target="_blank" rel="noreferrer">{children}</a> : <span>{children}</span>;
    },
    img({ alt }) { return <span>{alt}</span>; },
    input({ checked }) { return <input type="checkbox" checked={Boolean(checked)} disabled aria-label={checked ? "Complete" : "Incomplete"} />; },
  }}>{displayBody}</Markdown></div>;
}
