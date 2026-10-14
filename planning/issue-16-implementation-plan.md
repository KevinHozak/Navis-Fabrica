# GitHub-P3 Issue Picker and Workflow Context

## User Review Required
None. This is a read-only GitHub issue selection flow; it does not modify GitHub issues or start provider processes.

## Acceptance Criteria Mapping
- Pick an issue from a bounded repository list or enter a number/URL; native code enforces selected repository scope and identifies closed and blocked issues.
- Show checklist acceptance criteria, native blockers, parent epic, and PR links present in the issue body.
- Enable Work only for open, unblocked issues after blocker status is confirmed. Pass a bounded issue record to the Runs view without calling a GitHub write endpoint.

## Proposed Changes
- Extend the Rust GitHub adapter with bounded issue reads, reference validation, blocker and parent lookup, and bounded issue context types.
- Add narrowly scoped Tauri permissions and typed frontend calls.
- Add a project details issue picker and a Runs context panel; explain that provider execution remains a later phase.
- Add fixture tests for scoped references, context extraction, status gating, and field bounds.

## Verification Plan
- `cargo test --manifest-path src-tauri/Cargo.toml --locked github::tests`
- `npm run build`
- Inspect the final diff and confirm the issue branch is based on `origin/main`.
