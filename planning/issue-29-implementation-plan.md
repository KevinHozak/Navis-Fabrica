# Release-P2 implementation plan

**Issue:** #29 - Performance accessibility and diagnostics
**Status:** In progress
**User review required:** No additional product decision is needed; export and deletion will be explicit, user initiated actions.

## Acceptance criteria mapping

1. **Performance evidence:** Record the Windows machine/runtime, app startup method, scan/list fixture size, large-list interaction method, and output-flood limits/results in `docs/verification.md`. Keep fixture measurements distinct from live desktop integration evidence.
2. **Accessibility and large lists:** Inspect keyboard navigation, focus visibility, text/control contrast, reduced-motion behavior, and project/run list rendering. Preserve virtualization/bounds, fix observed issues, and document checks.
3. **Diagnostics, export, retention:** Provide an explicit export of allowlisted, redacted diagnostic metadata. Exclude prompts, process arguments, local paths, thread IDs, event text, and credentials. Retain bounded run/event history and expose a clear action to delete completed history while preserving active runs.

## Proposed changes

- `src-tauri/src/runs.rs`: add a privacy-minimized diagnostic summary and finished-history deletion operation; keep the existing 100-run, 2,000-event, and 512 KiB output caps; add focused store/serialization tests.
- `src-tauri/src/lib.rs`: register narrowly scoped commands and per-command permissions.
- `src/settings.ts`, `src/RunsPanel.tsx`, `src/styles.css`: add explicit JSON export and delete-finished-history controls with accessible status, confirmation, and responsive layout.
- `src/App.tsx`, `src/styles.css`: verify scan/project list keyboard and reduced-motion behavior; change only evidenced regressions.
- `docs/verification.md`: record measured fixture and host evidence, accessibility checks, export contents/exclusions, retention limits, and live integration limitations.
- Synchronize all five application version files as required by `AGENTS.md`.

## Verification plan

- Add Rust tests for export field allowlisting/redaction and deletion that preserves active records; exercise output/event caps with fixtures.
- Run `cargo test --locked --offline --manifest-path src-tauri/Cargo.toml`, `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`, `npm run build`, and `git diff --check`.
- Measure the disposable fixture sizes/timings and record OS, CPU, memory, build mode, and exact commands. Do not describe fixture checks as live desktop/provider tests.
