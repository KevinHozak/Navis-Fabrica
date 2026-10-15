# Issue #57: Persist project inventory between app runs

## User Review Required

None for the scoped local cache. The cache is app-local and stores only project discovery facts; provider secrets and credentials stay outside it.

## Acceptance Criteria Mapping

| Requirement | Implementation | Verification |
|---|---|---|
| Restore projects promptly and refresh in the background | Add an app-local inventory read command; restore it before automatically starting the normal scan after scan event listeners are ready. | Focused inventory restore coverage and frontend typecheck. |
| Reconcile additions, removals, renames, and moves | Persist complete scan results as a replacement snapshot; preserve stable IDs and update paths; retain the prior snapshot on partial scans. | Refresh reconciliation coverage, including rename/move, add, and remove. |
| Keep manual refresh progress, cancellation, and partial failures | Keep the existing scan event and cancellation path; report a cache write failure as a partial scan error. | Existing scan contract review plus focused persistence coverage. |
| Recover from missing, incompatible, or malformed data | Treat unreadable, oversized, wrong-root, future-dated, malformed, and unsupported-schema snapshots as empty; the regular scan repopulates them. | Invalid-cache recovery coverage. |
| Bound and atomically write local cache without credentials | Cap entries and serialized bytes; write a same-directory temporary file and atomically replace the snapshot. Store only `ProjectEntry` discovery facts. | Atomic replacement and size-limit coverage; inspect persisted shape. |
| Add focused coverage | Add unit coverage for restore/replace, reconciliation, and invalid cache fallback. | Rust focused test suite. |

## Proposed File Changes

- `src-tauri/src/inventory.rs`: schema-versioned, bounded atomic snapshot read/write and focused tests.
- `src-tauri/src/scanner.rs`: deserialize support for persisted project facts.
- `src-tauri/src/lib.rs` and `src-tauri/build.rs`: register the restore command, seed the incremental facts cache, and save successful and partial refreshes.
- `src/settings.ts`: typed inventory restore invocation.
- `src/App.tsx`: render restored projects promptly and start a background refresh.
- `planning/issue-57-implementation-plan.md`: this acceptance and verification map.
- Synchronized Tauri version manifests: bump the user-facing feature version for the PR.

## Verification Plan

- Run the focused Rust inventory tests, then the full Rust test suite if focused results pass.
- Run the frontend typecheck/build.
- Inspect storage format and staged diff for bounded size, atomic replacement, and absence of provider credential fields.
- Verify branch, commit, pushed remote branch, and PR read-back. Do not merge.
