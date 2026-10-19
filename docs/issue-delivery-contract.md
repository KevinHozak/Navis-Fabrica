# Shared issue delivery contract

Version: **1.0.0**. Maintained in Navis-Fabrica. Read this contract for `dev-work`, `dev-push`, and `dev-audit`; do not duplicate its policy in each skill. Repository instructions and explicit user scope take precedence. Helpers record evidence; they do not grant execution permission or enforce the agent's entire tool loop.

## Invocation and authorization

`dev-work` implements one selected issue, delivers its PR, and completes the bounded review/repair loop below. `dev-push` verifies and publishes the selected changes without independently launching the repair loop. `dev-audit` inspects code, reviews, acceptance, and checks; it changes source or remote state only when fixes are requested. When called by `dev-work`, audit repairs are already within scope. Commit/push/PR creation are authorized by dev-work/dev-push invocation; do not request them again. Merge, deployment, destructive operations, and scope expansion need their own authorization. Plan mode still forbids execution.

## Preflight and checkout

1. Identify repository, selected issue, default branch, dependencies, existing branches/PRs, and any already-merged implementation before editing. Read relevant repo instructions/product/architecture docs. Verify native sub-issues/dependencies and project placement when those relationships are changed. Do not infer remote freshness from local refs.
2. Prefer connected GitHub tools for supported reads and writes. Use Git for local operations and branch transport; use authenticated `gh` only for missing connector capabilities. Browser control is a fallback for unsupported board operations. A failed local CLI/proxy request does not establish invalid credentials. Never copy desktop tokens or alter system proxy settings.
3. Inspect primary/attached worktrees and shared Git metadata paths. Reuse a suitable issue checkout, including an already-isolated current checkout. If none exists, prefer a managed worktree based on the verified remote default branch; wait for completion and verify Git recognizes its returned path before branch creation. Managed creation does not relocate shared Git metadata or guarantee sandbox write permission. Do not create another worktree to solve that same denial.
4. Otherwise create a linked worktree under `/.worktrees/<issue>-<slug>` off the verified remote default branch and ensure `/.worktrees/` is ignored. Never switch the primary checkout for issue implementation or stash/reset unrelated work. Follow repo branch naming; otherwise use `codex/<issue>-<slug>`.
5. Record branch/base SHA, status, prerequisites, acceptance criteria, preserved invariants, and applicable required checks. Declare unavailable checks explicitly; do not omit them to get a green report. Scope UI changes against existing limits, accessibility, selection, and error behavior as well as new acceptance criteria.

## Environment and recovery

Run `python <fabrica-root>/scripts/workflow_evidence.py preflight --repo <repo> --issue <number> --base <verified-base>` for read-only local facts. It never fetches, creates a branch, installs packages, or queries GitHub. Its check suggestions need review against repository policy and the selected change. Manifest traversal is bounded; inspect deeper/custom stacks explicitly. npm `.cmd` version probing needs a verified host, so a missing probe result is unknown.

Use `npm ci` or the repo's declared lockfile installer and package caches; never borrow another checkout's node_modules as verification. Keep build caches outside tracked files; do not share mutable Rust targets across concurrent builds. Detect nested Cargo manifests, existing typecheck/build scripts, and required release contracts. Separate a cold compile from a confirmed hang using process/output evidence.

Classify a failure as permission, authentication, transport, dependency, or product. Classification heuristics are hints: confirm the diagnostic. Permit one targeted recovery per class, retain its result, and stop repeating unchanged attempts. Narrow sandbox elevation follows the platform's approval path; do not alter ACLs, delete unexplained locks, or broaden scope. Record unresolved blockers and continue independent work. A changed diagnostic may justify a new investigation, not resetting the same failed recovery budget.

For runner/reviewer protocol failures, record a `failure-capsule` checkpoint on the existing evidence run before choosing a recovery. Keep protocol diagnosis separate from product repair: passed product checks do not justify rewriting already-correct source to fix a parser. The checkpoint captures current source identity, existing check outcomes, parser/schema identifiers, bounded response metadata and declared retained usage reservations. It does not dispatch work, restart terminal attempts, resolve billing, or release unknown holds. Use an actual observed code such as `invalid_json`, `schema_mismatch`, `duplicate_output`, `timeout` or `truncated`; do not infer a historical cause from an unavailable response.

Raw response bodies and free-text error dumps are deliberately not accepted. When useful, supply a private synthetic reproduction file of at most 16 KiB; the helper records its path and SHA-256 without copying its content. Otherwise retain the explicit body-omitted limitation. A review failure leaves review unavailable, preserving the previous review observation; only a later explicitly observed review checkpoint can restore review completion. This optional helper is not an automatic Fabrica job-store integration or proof of real runner acceptance.

## Verification and publication

Prepare Tauri versions with the installed `dev-version` before final checks. Set an explicit target when managed versions disagree. Verify app manifests, lockfile root entries, application Cargo entry, and existing version-bearing window titles. An already-prepared branch version is not bumped again.

Keep evidence outside the target repository, for example under a caller-configured private state directory. Before a check, record its source fingerprint; after observing completion, retain a redacted result file, command, exit code, environment/runtime version, duration, and evidence hash. A build that rewrites tracked source requires reconciliation and a new check; do not associate an earlier check with later code. Identical content remains reusable after committing it; source changes invalidate results. Required CI evidence must identify the exact PR head and environment. Static source assertions do not substitute for behavioral/UI acceptance.

Remove generated artifacts only when they were created by this run and no prior/concurrent change would be lost. Compare the pre-run snapshot and current bytes; do not broadly restore generated directories or auto-update expectations merely to silence a regression.

Stage explicit scoped paths and inspect the staged diff. Commit conventionally; push the issue branch and create/update its PR after reconciling existing remote state. Missing/failing required checks or acceptance evidence mean **draft**. An existing ready PR becomes draft when this workflow's new change removes its readiness evidence. Partial implementations use `Refs #N`, not an automatic closing keyword, until acceptance is met or the user explicitly authorizes a deferral/closure policy. Attach each created/updated working PR to the current chat. Verify remote branch/head, PR URL/state/draft status, and issue linkage after persistence. An already-merged issue with remaining manual evidence gets an evidence handoff, not duplicate implementation.

### Authorized merge readback

After an authorized merge, perform a separate read-only lookup of the same PR before reporting completion. Confirm its merged state and merge commit SHA; the merge command response alone is not verification. If readback is unavailable or disagrees with the intended PR, report merge verification as pending or blocked, preserve the observed result, and do not repeat the merge or expand scope.

## Bounded review and repair (dev-work)

After draft publication, inspect review summaries, inline threads, conversations, required checks, and the current diff. If no reviewer result exists, perform a source-level audit yourself and record external review separately as pending/unavailable; do not imply an independent review happened. A new review request/comment is not required by this contract.

For at most **two repair rounds**: verify actionable findings against current code, fix scoped defects, rerun affected required checks and acceptance, commit/push, then inspect the updated head. Wait at most **five minutes per round** for pending checks/review; polls are at least **30 seconds apart**, with individual waits at most 60 seconds. Use event waits when supported. Review on an old head can inform fixes, but cannot certify the new head. No findings and no pending work ends the loop without consuming a repair round. Stop earlier on no progress, external blockers, user action, or missing authorization. Never create a background monitor implicitly.

Readiness requires declared applicable checks and acceptance to pass on current content, a clean scoped checkout, a verified open PR at the expected head, no unresolved actionable findings, and completed available required review. Pending or unavailable review stays explicit and does not become a fabricated pass. An explicit user deferral is tracked separately and does not turn a skipped check into passed evidence. Do not merge automatically.

## Evidence helper interface

The Python standard-library CLI is local-only and takes structured JSON input files. It does not execute arbitrary supplied check commands. Keep all inputs/logs redacted; the helper applies only a limited additional redaction heuristic. Do not put credentials or raw personal data into the record. Do not use generic source snapshots on files containing secrets. One agent owns each run record; coordinate concurrent runs separately.

```powershell
python <fabrica-root>/scripts/workflow_evidence.py checkpoint --repo <repo> --record <outside-repo/run.json> --action init --input <spec.json>
python <fabrica-root>/scripts/workflow_evidence.py checkpoint --repo <repo> --record <run.json> --action begin-check --input <check-id.json>
# Run the actual declared check through an authorized tool and observe its exit.
python <fabrica-root>/scripts/workflow_evidence.py record-check --repo <repo> --record <run.json> --input <result.json>
python <fabrica-root>/scripts/workflow_evidence.py report --repo <repo> --record <run.json>
```

Init spec: `issue`, `base`, `workflow_versions`, `required_checks` (IDs), `criteria` and `invariants` (objects with `id`, `description`, initially `status: pending`). Begin-check input: `{"id":"frontend"}`. Result input: `{"id":"frontend","argv":["npm","run","build"],"status":"passed","exit_code":0,"evidence":"<redacted-result-file>","environment":{"node":"<observed-version>"},"duration_seconds":12}`. Supported result states: passed, failed, blocked, pending, skipped. The helper binds locally observed results to content; remote CI results must be imported only after verifying CI head/tree identity and recorded as checks against identical local content.

Checkpoint actions: `acceptance` (`id`, `kind: criteria|invariants`, `status`, `evidence`; deferred requires `user_authorization`), `lookup` (observed absent/existing remote result), `remote` (`url`, `head`, `state: open|closed|merged`), `effect` (unique `id`, `kind`, result/artifact identity), `review` (`status: pending|complete|unavailable`, `head`, `actionable` count), `repair`, `recovery` (`error`, `remedy`), and `next` (`next_action`). Observe actual side effects before recording them. Effect IDs include operation and intended SHA/identity; before retry, reconcile the real GitHub/Git state, including crashes after success but before a checkpoint. Local records alone cannot guarantee exactly-once external execution.

Additive checkpoint action `failure-capsule` uses the same command with `--action failure-capsule --input <metadata.json>` on an existing run. Example synthetic review failure input:

```json
{
  "id": "review-failure-1",
  "attempt_id": "existing-attempt-1",
  "stage": "review",
  "diagnostic_code": "invalid_json",
  "parser": {"name": "review-parser", "version": "observed-version", "schema": "review-1"},
  "response": {"state": "malformed", "size_bytes": 10},
  "usage_reservations": [{"id": "existing-reservation-1", "state": "unknown"}],
  "next_action": "inspect-protocol"
}
```

Replace example identifiers with observed values, use `[]` when no reservation is known, and optionally supply `response.sha256` or `synthetic_reproduction` as a private file path. Stages are preflight/implementation/check/review/publication; other diagnostic codes are permission/authentication/transport/dependency/product. Response states are malformed/schema_mismatch/truncated/timeout/unavailable. Next actions are inspect-protocol/inspect-environment/repair-product/wait-for-user/reconcile-remote. Identifiers are bounded to 128 ASCII identifier characters; input is limited to 16 KiB, complete capsule metadata to 32 KiB, and retained reservations to 32. Unknown fields and duplicate IDs fail without changing the run. Do not supply secrets in identifiers or paths. Existing v1 records remain readable; the additive field does not certify upstream attempt IDs or imported reservation claims.

## Reporting and rollout

Report the result first, then meaningful changes, actual verification and limits, PR/commit/artifacts, and pending action. Keep implementation, verification, review, merge, and manual acceptance distinct. No canned passing rows or mandatory kickoff tables.

Pilot on the next **three explicitly selected** issues across the selected projects. Record setup seconds, repeated failures by class, missing required checks, user-requested follow-up audits, repair rounds, and review defects. Baselines are the sampled chats (qualitative only where timing is unavailable). Do not invent issue selection or claim a prospective pilot is completed. Keep the prior contract/skill snapshots until pilot evaluation; roll back targeted changes if correctness or preservation regresses. Fabrica UI/product integration is separate.
