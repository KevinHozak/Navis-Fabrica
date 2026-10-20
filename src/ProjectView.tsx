import { useEffect, useState } from "react";
import { sameProjectPath, useWorkflowActivity } from "./Activity";
import { RunsPanel } from "./RunsPanel";
import { getReleaseExecutableVersion, isDesktopApp, openProjectFolder, type ProjectEntry, type ReleaseExecutableVersion } from "./settings";

type ProjectPage = "project" | "changes" | "build";

export function ProjectView({ project, section }: { project: ProjectEntry; section: ProjectPage }) {
  const runs = useWorkflowActivity();
  const [release, setRelease] = useState<ReleaseExecutableVersion | null>(null);
  const [folderError, setFolderError] = useState("");
  const buildRefreshKey = runs?.filter(run => sameProjectPath(run.cwd, project.path) && run.workflow?.action === "dev-build")
    .map(run => `${run.id}:${run.finishedAtMs}`).join("|") ?? "";
  useEffect(() => {
    let alive = true;
    if (!isDesktopApp()) { setRelease(null); return; }
    getReleaseExecutableVersion(project.id).then(value => { if (alive) setRelease(value); })
      .catch(() => { if (alive) setRelease(null); });
    return () => { alive = false; };
  }, [project.id, project.path, buildRefreshKey]);

  const projectRuns = runs?.filter(run => sameProjectPath(run.cwd, project.path)) ?? null;
  const latestBuild = projectRuns?.filter(run => run.workflow?.action === "dev-build")
    .sort((left, right) => right.startedAtMs - left.startedAtMs)[0] ?? null;
  const changedFiles = project.git.changedFiles;
  const unpushed = project.git.upstream.status === "tracking" ? project.git.upstream.ahead : null;

  async function openFolder() {
    setFolderError("");
    try { await openProjectFolder(project.id); }
    catch (error) { setFolderError(error instanceof Error ? error.message : String(error)); }
  }

  return <section className="project-view" aria-label={`${project.name} ${section}`}>
    <nav className="project-view-tabs" aria-label={`${project.name} views`}>
      <a href={`#project/${encodeURIComponent(project.id)}`} aria-current={section === "project" ? "page" : undefined}>Overview</a>
      <a href={`#changes/${encodeURIComponent(project.id)}`} aria-current={section === "changes" ? "page" : undefined}>Changes</a>
      <a href={`#build/${encodeURIComponent(project.id)}`} aria-current={section === "build" ? "page" : undefined}>Build</a>
    </nav>

    {section === "project" && <>
      <section className="panel project-summary">
        <p className="eyebrow">PROJECT</p>
        <h1>{project.name}</h1>
        <details className="project-meta-details"><summary>Project details</summary><p className="project-view-path">{project.path}</p><div className="project-summary-facts">
          <div><span>Branch</span><strong>{project.git.branch ?? "Unavailable"}{project.isWorktree ? " · Worktree" : ""}</strong></div>
          <div><span>Latest commit</span><strong>{project.git.latestCommit?.subject ?? "Unavailable"}</strong></div>
          <div><span>Working tree</span><strong>{project.git.dirty === true ? "Has changes" : project.git.dirty === false ? "Clean" : "Unavailable"}</strong></div>
        </div></details>
        <div className="project-view-actions">
          <a className="button button-secondary" href={`#changes/${encodeURIComponent(project.id)}`}>Changes · {changedFiles?.length ?? 0}</a>
          <a className="button button-secondary" href={`#build/${encodeURIComponent(project.id)}`}>Build details</a>
          <a className="button button-secondary" href={`#runs?project=${encodeURIComponent(project.id)}`}>Activity</a>
          <a className="button button-secondary" href={`#batches?project=${encodeURIComponent(project.id)}`}>Batches</a>
          <a className="button button-secondary" href={`#epics-stories?project=${encodeURIComponent(project.id)}&type=epic`}>Epics & Stories</a>
          <a className="button button-secondary" href="#batches">Run batch</a>
        </div>
      </section>
      {isDesktopApp() && <section className="project-view-section panel"><h2>Project actions</h2><RunsPanel key={project.id} mode="actions" defaultCwd={project.path} project={project} issue={null} /></section>}
    </>}

    {section === "changes" && <section className="panel project-view-section">
      <p className="eyebrow">{project.name}</p><h1>Changes</h1>
      {!changedFiles ? <p>Changed files are unavailable. Refresh project details in Home.</p>
        : changedFiles.length ? <>
          <p className="project-view-muted">{changedFiles.length} working-tree {changedFiles.length === 1 ? "file" : "files"}{unpushed && unpushed > 0 ? ` · ${unpushed} unpushed ${unpushed === 1 ? "commit" : "commits"}` : ""}</p>
          <ul className="changed-file-list">{changedFiles.map(path => <li key={path}><code>{path}</code></li>)}</ul>
        </> : <p>Working tree clean.{unpushed && unpushed > 0 ? ` ${unpushed} committed ${unpushed === 1 ? "change is" : "changes are"} ahead of upstream.` : ""}</p>}
    </section>}

    {section === "build" && <section className="panel project-view-section">
      <p className="eyebrow">{project.name}</p><h1>Build</h1>
      {release ? <><p>Release {release.version ?? "version unavailable"}</p><details className="project-meta-details"><summary>Executable details</summary><dl className="project-build-facts">
        <div><dt>Release version</dt><dd>{release.version ?? "Unavailable"}</dd></div>
        <div><dt>Executable</dt><dd><code>{release.path}</code></dd></div>
      </dl></details></> : <p className="project-view-muted">Release executable details are unavailable.</p>}
      {latestBuild ? <div className="project-build-latest">
        <h2>Latest build</h2><p>{latestBuild.state} · {latestBuild.workflow?.buildProfile === "debug" ? "Debug" : "Release"}</p>
        {latestBuild.workflow?.buildArtifact && <details className="project-meta-details"><summary>Build artifact details</summary><dl className="project-build-facts">
          <div><dt>Artifact</dt><dd><code>{latestBuild.workflow.buildArtifact.path}</code></dd></div>
          <div><dt>File version</dt><dd>{latestBuild.workflow.buildArtifact.fileVersion || "Unavailable"}</dd></div>
          <div><dt>Product version</dt><dd>{latestBuild.workflow.buildArtifact.productVersion || "Unavailable"}</dd></div>
          <div><dt>Verification</dt><dd>{latestBuild.workflow.buildArtifact.verified ? "Verified" : latestBuild.workflow.buildArtifact.verificationMessage}</dd></div>
        </dl></details>}
      </div> : <p className="project-view-muted">No recorded build is available for this project.</p>}
      <div className="project-view-actions"><button className="button button-secondary" type="button" disabled={!isDesktopApp()} onClick={() => void openFolder()}>Open project folder</button></div>
      {folderError && <p role="status">{folderError}</p>}
    </section>}
  </section>;
}
