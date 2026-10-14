# Issue 13 Implementation Plan

## User Review Required
None. Discovery-P3 is explicitly selected. The work remains local and read-only with respect to discovered repositories.

## Acceptance Criteria Mapping
- Debounce recursive filesystem events and reuse unchanged project facts; keep manual refresh and cancellation available.
- Reconcile snapshots by stable filesystem identity where supported, replacing stale entries and preserving selection when a project is renamed. Fall back to canonical paths when file identity is unavailable.
- Add a disposable 100-project fixture benchmark and retain per-folder errors alongside usable results.

## Proposed Changes
- Add a cross-platform filesystem watcher and bounded debounce in the native layer; refresh the watched root when settings change.
- Add a process-local fact cache keyed by stable project identity and invalidate entries from watcher paths; avoid reusing facts after relevant changes.
- Reconcile refreshed projects in the UI by stable ID and continue displaying partial scan results and errors.
- Add focused checks for watcher exclusion filtering, rename/removal reconciliation, partial failures, and 100-project elapsed-time reporting; capture live watcher behavior separately.

## Verification Plan
- Run locked Rust tests, including a 100-project fixture that reports elapsed time and directory counts against the documented scan targets.
- Run frontend build/typecheck and locked Cargo check.
- Inspect staged changes and report measured local fixture results separately from live filesystem watcher behavior.
