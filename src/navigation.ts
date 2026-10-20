export type PageSection = "projects" | "project" | "changes" | "build" | "epics-stories" | "local-issues" | "batches" | "runs" | "providers" | "settings";
export type AppRoute = {
  section: PageSection;
  projectId?: string;
  issueType?: "epic" | "story";
};

const knownSections = new Set<PageSection>([
  "projects", "project", "changes", "build", "epics-stories", "local-issues", "batches", "runs", "providers", "settings",
]);

export function parseRoute(hash: string): AppRoute | null {
  const value = hash.replace(/^#/, "");
  const [path, query = ""] = value.split("?", 2);
  const [rawSection, rawProjectId] = path.split("/", 2);
  const section = rawSection === "home" ? "projects" : rawSection === "activity" ? "runs" : rawSection;
  if (!knownSections.has(section as PageSection)) return null;
  let projectId: string | undefined;
  try {
    projectId = rawProjectId ? decodeURIComponent(rawProjectId) : undefined;
  } catch {
    return null;
  }
  const params = new URLSearchParams(query);
  const type = params.get("type");
  return {
    section: section as PageSection,
    projectId: projectId ?? params.get("project") ?? undefined,
    issueType: type === "epic" || type === "story" ? type : undefined,
  };
}

export function routeHash(route: AppRoute): string {
  if (["project", "changes", "build"].includes(route.section)) {
    return `#${route.section}/${encodeURIComponent(route.projectId ?? "")}`;
  }
  const params = new URLSearchParams();
  if (route.projectId) params.set("project", route.projectId);
  if ((route.section === "epics-stories") && route.issueType) params.set("type", route.issueType);
  const query = params.toString();
  return `#${route.section}${query ? `?${query}` : ""}`;
}

export function routeKey(route: AppRoute): string {
  return `${route.section}:${route.projectId ?? ""}:${route.issueType ?? ""}`;
}

export function routeTitle(route: AppRoute, projectName?: string): string {
  if (route.section === "project") return projectName ?? "Project";
  if (route.section === "changes") return `${projectName ?? "Project"} · Changes`;
  if (route.section === "build") return `${projectName ?? "Project"} · Build`;
  const titles: Record<PageSection, string> = {
    projects: "Home", project: "Project", changes: "Changes", build: "Build",
    "epics-stories": "Epics & Stories", "local-issues": "Local issues", batches: "Runner batches",
    runs: "Activity", providers: "Providers", settings: "Settings",
  };
  return titles[route.section];
}
