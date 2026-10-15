# Routing-P1 usage and latency implementation plan

## User review required

None. Scope is bounded by issue #25.

## Acceptance criteria mapping

1. Use a typed usage signal with separate categories, numeric value, unit, provider/model, collection time, optional window/reset, source, confidence, and availability state.
2. Collect Codex subscription windows and context usage only when its app-server reports them. Represent unsupported API spend and local resource telemetry as unknown; permit explicitly labelled manual estimates.
3. Normalize provider Unix reset timestamps to milliseconds, mark aged measurements stale, preserve unavailable/rate-limited states, and persist sanitized measurements. Derive latency from run start and finish timestamps without saving prompt/output as telemetry.

## Proposed files

- Add `src-tauri/src/usage.rs` for normalization, bounded persistence, unknown/stale projection, and fixtures.
- Update `src-tauri/src/runs.rs` to collect app-server signals and record model/latency metadata.
- Update `src-tauri/src/lib.rs` for usage commands and signal APIs.
- Update `src/settings.ts`, `src/ProvidersPanel.tsx`, and `src/RunsPanel.tsx` for typed signal display, manual estimates, and latency display.
- Update `docs/verification.md` with fixture results and live telemetry limitations.

## Verification

- Rust focused usage tests for timestamps, unknown/unavailable/rate-limited/stale readings, and signal redaction.
- Rust compile check and frontend typecheck.
- No live provider call unless existing authenticated configuration makes one explicitly available; report live results separately.
