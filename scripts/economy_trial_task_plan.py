"""Build and validate the deterministic implementation portion of a synthetic trial job.

This helper is local and pure: it parses an issue snapshot, applies its full
behavior description and exact Acceptance bullets to the real task declaration,
and renders the supervisor's current implementation prompt for review. It never
starts a provider or executes candidate code.
"""

from __future__ import annotations

from pathlib import PurePosixPath
from typing import Any


class TrialTaskPlanError(ValueError):
    """The issue snapshot and implementation task do not form a safe trial plan."""


def _issue_body(markdown: str) -> list[str]:
    if not isinstance(markdown, str) or not markdown.strip():
        raise TrialTaskPlanError("issue snapshot must be nonempty text")
    # Shared issue storage may include the UTF-8 BOM required by planning
    # Markdown; it is transport metadata, not part of the requirement text.
    if markdown.startswith("\ufeff"):
        markdown = markdown[1:]
    lines = markdown.splitlines()
    if lines and lines[0].strip() == "---":
        try:
            end = next(i for i, line in enumerate(lines[1:], 1) if line.strip() == "---")
        except StopIteration as exc:
            raise TrialTaskPlanError("issue snapshot frontmatter is unterminated") from exc
        lines = lines[end + 1 :]
    return lines


def parse_issue_requirements(markdown: str) -> tuple[str, list[str]]:
    """Return the complete prose specification and exact Acceptance bullets."""
    lines = _issue_body(markdown)
    heading = next((i for i, line in enumerate(lines) if line.startswith("# ")), None)
    if heading is None:
        raise TrialTaskPlanError("issue snapshot must contain a level-one title")
    acceptance_markers = [i for i, line in enumerate(lines) if line.strip() == "Acceptance:"]
    if len(acceptance_markers) != 1:
        raise TrialTaskPlanError("issue snapshot must contain one exact Acceptance: section")
    marker = acceptance_markers[0]
    if marker <= heading:
        raise TrialTaskPlanError("Acceptance section must follow the issue title")
    description_lines = [line.strip() for line in lines[heading + 1 : marker] if line.strip()]
    if not description_lines:
        raise TrialTaskPlanError("issue behavior description is required")
    description = "\n".join(description_lines)
    bullets: list[str] = []
    for line in lines[marker + 1 :]:
        if not line.strip():
            continue
        if not line.startswith("- ") or not line[2:].strip():
            raise TrialTaskPlanError("Acceptance section must contain only nonempty '- ' bullets")
        bullets.append(line[2:].strip())
    if not bullets:
        raise TrialTaskPlanError("issue must declare at least one acceptance bullet")
    return description, bullets


def validate_file_allowlist(actual: Any, expected: list[str] | tuple[str, ...]) -> list[str]:
    """Require a duplicate-free, relative allowlist exactly matching the registry."""
    if not isinstance(actual, list) or any(not isinstance(path, str) for path in actual):
        raise TrialTaskPlanError("implementation task files must be a string list")
    allowed = list(expected)
    if not allowed or len(set(allowed)) != len(allowed):
        raise TrialTaskPlanError("trusted fixture allowlist must be nonempty and unique")
    for path in [*actual, *allowed]:
        normalized = PurePosixPath(path)
        if (
            not path
            or "\\" in path
            or ":" in path
            or path != normalized.as_posix()
            or normalized.is_absolute()
            or any(part in {"", ".", ".."} for part in normalized.parts)
        ):
            raise TrialTaskPlanError("fixture paths must be normalized repository-relative POSIX paths")
    if actual != allowed:
        raise TrialTaskPlanError("implementation task files do not exactly match the trusted fixture allowlist")
    return list(actual)


def complete_acceptance(markdown: str) -> tuple[str, list[str]]:
    """Place the full prose requirement before all verbatim acceptance bullets."""
    description, bullets = parse_issue_requirements(markdown)
    return description, [description, *bullets]


def supervisor_prompt_preview(
    *,
    issue_id: str,
    issue_fingerprint: str,
    issue_snapshot: str,
    task_id: str,
    files: list[str],
    acceptance: list[str],
    plan_task_count: int,
) -> str:
    """Mirror runner-core's `implementation_prompt` format for pre-dispatch review."""
    if not all(isinstance(value, str) and value for value in (issue_id, issue_fingerprint, issue_snapshot, task_id)):
        raise TrialTaskPlanError("issue, fingerprint, snapshot, and task identities are required")
    if not files or not acceptance or plan_task_count < 1:
        raise TrialTaskPlanError("prompt needs allowlisted files, acceptance, and a nonzero plan count")
    file_lines = "\n".join(f"- {path}" for path in files)
    acceptance_lines = "\n".join(f"- {criterion}" for criterion in acceptance)
    return (
        "Implement the selected local issue in this isolated worktree. Do not commit, push, publish, merge, or deploy.\n"
        f"Issue: {issue_id}\n"
        f"Issue snapshot fingerprint: {issue_fingerprint}\n"
        f"Authoritative issue requirements (stored snapshot):\n{issue_snapshot}\n"
        f"Task: {task_id}\n"
        f"Permitted files:\n{file_lines}\n"
        f"Acceptance criteria:\n{acceptance_lines}\n"
        f"Plan task count: {plan_task_count}\n"
        "Work only within the permitted files, run no undeclared external actions, and report a concise completion summary. "
        "If blocked by approval, credentials, or required human input, stop and state the specific requirement."
    )


def correct_implementation_task(
    job: dict[str, Any], issue_markdown: str, expected_files: list[str] | tuple[str, ...]
) -> tuple[dict[str, Any], str]:
    """Return a corrected copy of a BatchRequest job and its exact prompt preview."""
    if not isinstance(job, dict) or not isinstance(job.get("plan"), dict):
        raise TrialTaskPlanError("job must contain a plan")
    if job.get("issue_snapshot") != issue_markdown:
        raise TrialTaskPlanError("issue requirements must come from the exact persisted job snapshot")
    tasks = job["plan"].get("tasks")
    if not isinstance(tasks, list):
        raise TrialTaskPlanError("job plan must contain a task list")
    implementation_tasks = [task for task in tasks if task.get("kind") == "implementation"]
    if len(implementation_tasks) != 1:
        raise TrialTaskPlanError("job must contain exactly one implementation task")
    task = implementation_tasks[0]
    files = validate_file_allowlist(task.get("files"), expected_files)
    description, acceptance = complete_acceptance(issue_markdown)
    corrected = {
        **job,
        "plan": {
            **job["plan"],
            "tasks": [
                {**candidate, "acceptance": acceptance} if candidate is task else candidate
                for candidate in tasks
            ],
        },
    }
    corrected_task = next(candidate for candidate in corrected["plan"]["tasks"] if candidate.get("kind") == "implementation")
    preview = supervisor_prompt_preview(
        issue_id=corrected["issue_id"],
        issue_fingerprint=corrected["source_fingerprint"],
        issue_snapshot=corrected.get("issue_snapshot", ""),
        task_id=corrected_task["task_id"],
        files=files,
        acceptance=corrected_task["acceptance"],
        plan_task_count=len(corrected["plan"]["tasks"]),
    )
    # Defensive check: every source requirement must be represented in both actual
    # request input and the prompt the supervisor will construct.
    if any(item not in corrected_task["acceptance"] or item not in preview for item in [description, *acceptance[1:]]):
        raise TrialTaskPlanError("full issue requirements are missing from the task or prompt preview")
    return corrected, preview


def validate_supervisor_prompt(job: dict[str, Any], actual_prompt: str) -> None:
    """Fail closed unless a captured runtime prompt exactly matches this job."""
    if not isinstance(job, dict) or not isinstance(job.get("plan"), dict):
        raise TrialTaskPlanError("job must contain a plan")
    tasks = job["plan"].get("tasks")
    if not isinstance(tasks, list):
        raise TrialTaskPlanError("job plan must contain a task list")
    implementation_tasks = [task for task in tasks if task.get("kind") == "implementation"]
    if len(implementation_tasks) != 1:
        raise TrialTaskPlanError("job must contain exactly one implementation task")
    task = implementation_tasks[0]
    expected = supervisor_prompt_preview(
        issue_id=job.get("issue_id", ""),
        issue_fingerprint=job.get("source_fingerprint", ""),
        issue_snapshot=job.get("issue_snapshot", ""),
        task_id=task.get("task_id", ""),
        files=validate_file_allowlist(task.get("files"), task.get("files", [])),
        acceptance=task.get("acceptance", []),
        plan_task_count=len(tasks),
    )
    if not isinstance(actual_prompt, str) or actual_prompt != expected:
        raise TrialTaskPlanError("captured supervisor prompt differs from the verified task plan")
