# Issue #74 implementation plan

## User review required
None for the scoped UI change. Preserve the existing local-only project data boundary and route project actions through the selected `ProjectEntry.path`.

## Acceptance criteria mapping

| Criterion | Implementation | Verification |
|---|---|---|
| Responsive cards fill available width | Replace the fixed-height split browser with a CSS grid of project cards in normal page flow | Inspect wide and narrow desktop widths |
| Key repository details per card | Show user-facing path, branch/status, working-tree state, detected stacks, and latest commit; label unavailable values explicitly | Review conditional rendering for each Git state |
| Existing detail and workflow context | Retain selected-project detail panel below the card grid, including GitHub activity and issue picker; preserve selected canonical path passed to Runs | Inspect selection and `onWork` path flow |
| Existing browser states and filters | Keep search, stack filter, count, loading, empty, partial-error, cancellation, refresh, and retry handling in `App` | Build and inspect state branches |
| Accessible keyboard interaction and status | Use native buttons for card selection, `aria-pressed`, focus-visible styling, and text labels alongside color | Inspect DOM semantics and CSS |
| No invented facts | Render unavailable/unknown statuses from existing `ProjectEntry.git` data | Review all card fact fallbacks |

## Proposed file changes
- `src/App.tsx`: replace virtualized list layout with responsive project cards; keep `ProjectDetails`, issue flow, and scan states.
- `src/styles.css`: add card grid and narrow-width rules; remove obsolete split-pane/virtual viewport rules.
- `src-tauri/tests/release_security_contract.rs`: update the existing accessibility contract for button-based project cards and remove a redundant stale permission-count assertion; its set comparison already checks exact permission coverage.
- `src-tauri/tauri.conf.json`, `src-tauri/Cargo.toml`, `src-tauri/Cargo.lock`, `package.json`, and `package-lock.json`: synchronize the PR feature version to `0.24.0`.

## Verification plan
- Run a strict TypeScript check and Vite production build with the locked dependency versions.
- Run `cargo test --manifest-path src-tauri/Cargo.toml --locked`, including the security contract tests.
- Attempt an app preview to inspect wide/narrow layout and keyboard focus/selection; record any environment limitation.
- Review the diff and confirm actions still receive the original `project.path`.
