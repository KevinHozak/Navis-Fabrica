# Issue 20 Implementation Plan

## User Review Required
None. Build actions request no repository or remote side effects.

## Acceptance Criteria Mapping
- Select a Tauri-specific dev-build profile; release is the default and debug is clearly labeled as development-only.
- Persist bounded run events, build artifact receipts, and process IDs. Verify receipts against the selected profile, an existing nonempty artifact inside the project, and FileVersion and ProductVersion matching the declared Tauri version.
- On history load, active runs become interrupted. Reconcile saved PIDs on refresh and defer build retry while the prior process is still running.
- Offer a new side-effect-free build retry for failed, interrupted, cancelled, or timed-out builds only after process exit; do not expose this retry for dev-work/dev-push.
- Existing event persistence and exact operation-key guards retain side-effect evidence and block duplicate workflow retries.

## Changes
- Add dev-build profile selection and launch through the compatible Codex skill.
- Extend persisted workflow metadata with profile, artifact receipt, and process checkpoint.
- Add safe build retry and scoped artifact receipt validation.

## Verification
- Rust fixture tests for artifact profile/path/version, PID reconciliation, and run history recovery.
- Frontend production build, Rust tests, rustfmt, and diff checks.
- Report live build-provider evidence separately from fixture tests.
