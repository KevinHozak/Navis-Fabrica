# Issue 22 Implementation Plan

## User Review Required

None. Issue #22 explicitly selects the Codex coding adapter phase. Use the installed Codex CLI only through its documented argument-array interface. Any live smoke must use a disposable repository, a read-only sandbox where supported, and a prompt that does not request network or repository mutation.

## Acceptance Criteria Mapping

- Validate the installed CLI version and generate the experimental app-server bindings from that installed runtime; normalize JSON-RPC notifications, final assistant output, authentication failures, malformed messages, and cancellation outcomes.
- Pin every invocation to the selected canonical project directory. Resolve only declared skills from the existing skill-discovery boundary. Pass only the configured model and reasoning effort, and reject unsupported values before process launch.
- Represent waiting for approval/input distinctly from running/completed/failed; never silently auto-approve a request. Keep process cancellation connected to the existing managed-run lifecycle.
- Run a live opt-in smoke against a temporary local repository with no remote mutation; separately verify malformed-stream and provider-failure fixtures.

## Proposed Changes

- Extend the Codex provider implementation with CLI version/capability probing, typed invocation construction, JSONL parsing, normalized events/final output, and explicit unsupported-capability reporting.
- Integrate app-server sessions with the existing bounded run manager, preserving sequence numbers, redaction, timeout, process-tree cancellation, thread IDs for provenance, approvals, and user input; report resume/checkpoint support as unavailable.
- Add narrow native commands for Codex start/approval/input, validating canonical project scope, model/reasoning selection, and discovered skill IDs on the Rust side.
- Update UI capability and status text only for behavior the adapter proves it supports.
- Add focused fixtures for malformed JSONL, auth failures, terminal events, and cancellation.

## Verification Plan

- cargo fmt --manifest-path src-tauri/Cargo.toml -- --check.
- Rust provider fixture checks for event normalization and failure handling.
- npm run typecheck and npm run build if frontend code changes.
- Run a single bounded smoke with the installed app-server in an isolated temporary repository, using no remote and a restrictive sandbox; record the observed version and emitted event types. If local Codex state access prevents the run, preserve that as a tooling limitation.
- Review git diff --check and staged diff before commit; do not claim live capabilities beyond directly observed evidence.
