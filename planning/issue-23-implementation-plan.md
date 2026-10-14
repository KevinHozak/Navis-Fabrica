# Providers-P3 implementation plan

## User Review Required
None. The issue workflow authorizes implementation, verification, commit, push, and pull request creation. No merge is in scope.

## Acceptance Criteria

| Criterion | Implementation | Verification |
| --- | --- | --- |
| Probe installed executable/SDK and documented noninteractive entry point | Probe the configured `agy` version and use the installed `--print` headless entry point. | Local version probe; authenticated model/run probes report explicit unavailable state when auth cannot be accessed. |
| Map settings, skills, progress, input/cancel semantics | Pass the configured model; rely on Antigravity CLI workspace/global skills; stream available process stdout/stderr and terminate the child on cancel/timeout. Installed 1.0.10 does not expose structured progress or effort flags. | Fixture tests for command argument construction and the capability matrix; approvals, interactive input, checkpoints, and effort are unsupported. |
| Document limitations and verify Windows temp-repo run or explicit unavailable state | Record tested version, official entry point, security/sandbox behavior, and exact local auth limitation in verification docs. | Attempt smoke in a disposable Windows directory; report auth/tooling limitation without secrets. |

## Proposed Changes
- `src-tauri/src/providers.rs`: Antigravity defaults, tested capability metadata, probe result, and installed-version command builder.
- `src-tauri/src/runs.rs`: launch the headless adapter, stream normalized events, preserve cancellation/timeout and run history.
- `src-tauri/src/lib.rs` and `src-tauri/permissions/`: scoped provider probe and start command.
- `src/settings.ts`, `src/ProvidersPanel.tsx`, `src/RunsPanel.tsx`: expose probe/readiness and provider run launch.
- `docs/verification.md`, `planning/issue-23-implementation-plan.md`: evidence and scope.
- Synchronized app version metadata: `0.15.0`.

## Verification Plan
- Focused Rust tests for Antigravity command argument boundaries and capability declarations.
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`.
- `cargo test --manifest-path src-tauri/Cargo.toml --locked` or focused tests if existing environment-sensitive suite failures recur.
- `npm run build` and `git diff --check`.
- Disposable Windows CLI smoke; separate authenticated live evidence from fixtures and report unavailable authentication explicitly.
