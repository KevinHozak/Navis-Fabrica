# Issue 19 Implementation Plan

## User Review Required
None. The selected workflow actions must remain opt-in, scoped, and honest about runtime capabilities.

## Acceptance Criteria Mapping
- Start dev-work/dev-push through the configured Codex adapter, passing the selected project, issue context where applicable, and an explicit scope/effect request.
- Preflight canonical project path, Git facts, selected provider/model/runtime, workflow skill compatibility, authentication/provider availability, and repository cleanliness. Do not stage or stash automatically. Use the skill's isolated-worktree procedure for issue work and block unsafe push conditions.
- Persist enough run identity and outcome evidence to surface completed commits, pushes, and PR URLs; prevent retry of an interrupted mutating run from repeating unverified side effects.

## Proposed Changes
- Replace registry-only cards with a focused launch form for dev-work and dev-push.
- Collect user-selected change paths and effect opt-ins; build a bounded Codex prompt from verified context.
- Extend native preflight and run persistence only where existing Codex and run contracts do not already cover the requirement.
- Display existing workflow runs and their terminal/side-effect evidence; do not add a generic shell path.

## Verification Plan
- Add focused fixture tests for scope validation, action readiness, and duplicate-side-effect retry blocking.
- Run frontend typecheck/build and Rust focused checks, plus an opt-in disposable-repository smoke run if the installed runtime permits it.
- Report fixture results separately from live provider, GitHub auth, push, and PR integration evidence.