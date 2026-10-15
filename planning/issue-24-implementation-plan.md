# Providers-P4 implementation plan

## User Review Required
None. Issue #24 authorizes scoped implementation, verification, commit, push, and pull request creation. Do not merge.

## Acceptance Criteria

| Criterion | Implementation | Verification |
| --- | --- | --- |
| Discover configured local models and endpoint health; evaluate a compatible agent harness | Probe loopback Ollama `/api/tags` and `/api/show`; report loaded models, metadata/context, and explicit health. Use the Aider Ollama Chat integration for edits and an explicitly supplied test command; disable auto-commits, update checks, and analytics. | Disposable HTTP fixtures for health/model metadata and Aider argument construction; record live CLI/endpoint availability separately. |
| Enforce local-only profile, context/tool capability eligibility and hardware-aware timeout | Restrict Ollama endpoint to loopback HTTP; reject selected models without reported tool capability or adequate known context; compute bounded timeout from available memory and model size, with unknown hardware surfaced rather than guessed. | Fixture matrix for endpoint, context, tools, memory, timeout bounds; unsupported/unknown states stay ineligible or explicit. |
| Manual switching checkpoints goal/scope/diff/checks/pending work; no silent switch or duplicate effects | Persist a portable, redacted handoff packet against a finished run; require explicit target selection and confirmation; carry completed remote side effects into the next run prompt and prevent automatic resumption/replay. | Fixture tests for packet validation, serialization, redaction, and side-effect preservation; UI state only offers manual switching. |

## Proposed Changes
- `src-tauri/src/providers.rs`: Ollama probe DTOs, local endpoint validation, Aider capabilities and eligibility.
- `src-tauri/src/runs.rs`: bounded Aider launch and durable handoff packet persistence/validation.
- `src-tauri/src/lib.rs`, Tauri permissions: probe, start, create/list handoff commands.
- `src/settings.ts`, `src/ProvidersPanel.tsx`, `src/RunsPanel.tsx`: endpoint/model readiness, local-only status, handoff creation and explicit target launch.
- `src-tauri/Cargo.toml` and synchronized lock/version metadata if required.
- `docs/verification.md`, this plan: fixture results and separate live integration evidence.

## Verification
- Focused Rust tests for Ollama endpoint/probe parsing, model eligibility, context/hardware timeout policy, Aider arguments, and handoff persistence/side-effect behavior.
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`.
- `cargo test --manifest-path src-tauri/Cargo.toml --locked <focused filters>` and `cargo check --manifest-path src-tauri/Cargo.toml --locked`.
- `npm run build`, `git diff --check`, and version synchronization checks.
- Live Ollama/Aider probe only when installed and reachable; report fixture and live evidence separately.