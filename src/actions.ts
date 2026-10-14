import type { GitHubIssue, ProjectEntry } from "./settings";

export type WorkflowActionId = "dev-work" | "dev-push" | "dev-build";
export type WorkflowInput = {
  id: string;
  label: string;
  type: "text" | "url" | "select" | "project-context" | "issue-context" | "file-selection";
  required: boolean;
  options?: string[];
  defaultValue?: string;
};
export type WorkflowEffect = "commit" | "push" | "createPullRequest";
export type WorkflowEffectScope = { permitted: WorkflowEffect[]; defaultRequested: WorkflowEffect[] };
export type WorkflowSkillStatus = { id: WorkflowActionId; status: "available" | "missing" | "unavailable" };
export type WorkflowAction = {
  id: WorkflowActionId;
  version: number;
  title: string;
  summary: string;
  skillReference: string;
  supportedStacks: "all" | string[];
  inputs: WorkflowInput[];
  checks: string[];
  outputArtifacts: string[];
  effectScope: WorkflowEffectScope;
  defaultBuildProfile?: "release" | "debug";
};

const repositoryEffects: WorkflowEffectScope = {
  permitted: ["commit", "push", "createPullRequest"],
  defaultRequested: [],
};
const noEffects: WorkflowEffectScope = { permitted: [], defaultRequested: [] };

export const workflowActions: WorkflowAction[] = [
  {
    id: "dev-work",
    version: 1,
    title: "Work on issue",
    summary: "Investigate and implement a selected GitHub issue in this project.",
    skillReference: "dev-work",
    supportedStacks: "all",
    inputs: [
      { id: "project", label: "Selected project", type: "project-context", required: true },
      { id: "issue", label: "Selected issue", type: "issue-context", required: true },
      { id: "agent", label: "Agent", type: "select", required: true, options: ["Codex", "Antigravity"] },
      { id: "model", label: "Model", type: "text", required: false },
    ],
    checks: ["Issue is open and unblocked", "Project stack is supported", "Compatible skill and execution adapter are available"],
    outputArtifacts: ["Scoped code changes", "Verification results", "Commit and pull request when opted in"],
    effectScope: repositoryEffects,
  },
  {
    id: "dev-push",
    version: 1,
    title: "Push changes",
    summary: "Review selected changes, commit them, push the branch, and open or update a pull request.",
    skillReference: "dev-push",
    supportedStacks: "all",
    inputs: [
      { id: "project", label: "Selected project", type: "project-context", required: true },
      { id: "changes", label: "Selected changes", type: "file-selection", required: true },
      { id: "commitMessage", label: "Commit message", type: "text", required: true },
      { id: "pullRequestTitle", label: "Pull request title", type: "text", required: false },
    ],
    checks: ["Repository and branch are verified", "Selected changes are scoped", "Required checks are declared"],
    outputArtifacts: ["Commit SHA", "Remote branch", "Pull request URL when PR creation is opted in"],
    effectScope: repositoryEffects,
  },
  {
    id: "dev-build",
    version: 1,
    title: "Build project",
    summary: "Build a supported project and verify the generated artifact.",
    skillReference: "dev-build",
    supportedStacks: ["Tauri"],
    inputs: [
      { id: "project", label: "Selected project", type: "project-context", required: true },
      { id: "buildProfile", label: "Build profile", type: "select", required: true, options: ["release", "debug"], defaultValue: "release" },
    ],
    checks: ["Tauri stack detected", "Rust toolchain available", "Artifact metadata matches the declared version"],
    outputArtifacts: ["Release executable and package path", "Verified executable metadata"],
    effectScope: noEffects,
    defaultBuildProfile: "release",
  },
];

export function actionDisabledReason(
  action: WorkflowAction,
  project: ProjectEntry | null,
  issue: GitHubIssue | null,
  skills: WorkflowSkillStatus[],
): string | null {
  if (!project) return "Select a project before preparing this action.";
  if (action.id === "dev-work") {
    if (!project.isGitRepository) return "Work on issue requires a Git repository.";
    if (!project.git.remotes.some((remote) => remote.github)) return "Work on issue requires a project linked to GitHub.";
    if (!issue || !issue.selectable) return issue ? "This issue is closed or blocked." : "Choose an open, unblocked issue from the project first.";
  }
  if (action.id === "dev-push" && !project.isGitRepository) return "This project is not a Git repository.";
  if (action.supportedStacks !== "all" && !action.supportedStacks.some((stack) => project.detectedStacks.includes(stack))) {
    return `Requires ${action.supportedStacks.join(" or ")}; detected ${project.detectedStacks.join(", ") || "no stack"}.`;
  }
  const skill = skills.find((entry) => entry.id === action.id);
  if (!skill || skill.status !== "available") {
    return skill?.status === "unavailable"
      ? `The ${action.skillReference} skill location could not be read.`
      : `Compatible skill ${action.skillReference} was not found. Markdown skill files are never executed.`;
  }
  if (action.id === "dev-build") {
    const cargo = project.workflowPrerequisites.find((item) => item.name === "cargo" && item.requiredFor.includes("Tauri"));
    if (!cargo?.available) return "The Rust cargo prerequisite is unavailable.";
  }
  return "The workflow execution adapter is not available in this phase.";
}
