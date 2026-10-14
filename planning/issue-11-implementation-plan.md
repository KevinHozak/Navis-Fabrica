# Discovery-P1 implementation plan

## User review required

None. The issue and `dev-work` workflow authorize implementation and delivery.

## Acceptance criteria mapping

| Requirement | Implementation | Verification |
| --- | --- | --- |
| Scan saved root with depth and exclusions | Bounded breadth-first Rust scanner | ✅ Fixture tests passed |
| Progress and cancellation | Tauri progress/complete events and cancellation command | ✅ Cancellation fixture and native compile passed |
| Coding folders, nested Git, `.git` worktree files | Marker detection plus `.git` file/directory resolution | ✅ Nested repo/worktree fixtures passed |
| Junction cycles, missing/inaccessible roots, spaces, duplicates, partial results | Canonical path tracking, bounded errors, path-aware traversal | ⚠️ Missing/non-folder, spaces, broken metadata, and partial results covered; Unix permission tests added; Windows junction/ACL fixtures unavailable in this run |

## Proposed files

- `src-tauri/src/scanner.rs`: scan models, bounded traversal, tests.
- `src-tauri/src/lib.rs`: expose start/cancel scan commands and events.
- `src-tauri/capabilities/default.json`: narrow event-listen permission.
- `src/App.tsx`: project scan UI, progress, cancellation, results, errors.
- `src/settings.ts`: scan IPC types and wrappers.

## Verification plan

- Rust scanner unit tests with disposable filesystem fixtures: 12 passed on Windows.
- Locked Rust check and frontend TypeScript/Vite build: passed.
- Live desktop integration: not run; recorded separately in `docs/verification.md`.
