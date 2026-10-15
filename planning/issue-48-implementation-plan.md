# Providers-P5 implementation plan

## User Review Required

None. Issue #48 authorizes implementation, scoped verification, commit, push, and pull request creation. Do not merge.

## Acceptance Criteria

| Criterion | Implementation | Verification |
| --- | --- | --- |
| Store OpenRouter authentication safely | Add a Rust command that stores the user-entered key in Windows Credential Manager and returns only its opaque reference. Never put key values in saved settings, logs, model responses, or errors. | Validate input and reference names; exercise credential validation/target construction and confirm settings contain only a reference. |
| Discover catalog metadata without guessing | Rust makes a bounded authenticated request to OpenRouter's models endpoint. Parse reported context, pricing, modalities, and supported parameters as optional fields; distinguish auth, rate-limit, unavailable, malformed, and ready states. | Fixtures for success, malformed JSON, 401, 429, unknown fields, bounds, and secret redaction. |
| Manual selection with explicit errors | Add OpenRouter to provider settings, probe/model refresh, and the existing manual model selector. Never auto-select a replacement when the selected model disappears. | Frontend build and Rust catalog/selection tests. |
| Route through a configured coding harness | Use an explicit Aider executable as a compatible OpenAI API harness. Pass the selected OpenRouter model and credential through process environment; do not claim the provider alone supplies shell/file/test capabilities. | Verify generated arguments/environment policy and guard unsupported runtime/provider combinations. |
| Bound requests, cancellation, and diagnostics | Apply connect/overall timeouts, no redirects, bounded response size/model count, explicit cancellation-aware Aider run handling, and sanitized errors. | Fixture timeout/error paths, cancellation path, and secret redaction checks. |

## Proposed Changes

- `src-tauri/src/providers.rs`: OpenRouter catalog DTOs, parsing, bounded HTTP request, status mapping, input/reference validation, Aider arguments, capability reporting.
- `src-tauri/src/lib.rs`, `src-tauri/build.rs`, `src-tauri/capabilities/default.json`, generated permission: secure credential write and probe commands; validate selected provider and model before run startup.
- `src-tauri/src/runs.rs`: scoped OpenRouter/Aider execution with API base/key in child-process environment and secret-safe output handling.
- `src/settings.ts`, `src/ProvidersPanel.tsx`, `src/RunsPanel.tsx`: manual provider setup, secure key save, catalog refresh/errors/metadata, selected runtime dispatch.
- `planning/issue-48-implementation-plan.md`, `docs/verification.md`: acceptance mapping and fixture/live-evidence limits.

## Verification

- Focused Rust tests for model parsing, malformed/unknown metadata, HTTP status mapping, reference/key validation, runtime args/environment, and redaction.
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`.
- `cargo test --manifest-path src-tauri/Cargo.toml --locked` and `cargo check --manifest-path src-tauri/Cargo.toml --locked`.
- `npm run build`, `git diff --check`, synchronized version verification, and source review for secret leakage.
- Live OpenRouter check remains opt-in and must not require or persist a real key during automated verification.
