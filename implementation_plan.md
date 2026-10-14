# Issue 21 Implementation Plan

## User Review Required
None. This implements the explicitly selected Providers-P1 phase and does not connect to or launch provider processes.

## Acceptance Criteria Mapping
- Provider adapters expose discovery, health, model listing, run start/cancel, event streaming contracts, and checkpoint handoff capabilities.
- Settings persist provider identity/type, selected model, reasoning effort, runtime, and opaque secret references as distinct fields. Secret values are never accepted or stored.
- Provider discovery reports executable and authentication-reference status. Unsupported operations are represented as unsupported; no executable or model is installed or downloaded.

## Proposed Changes
- Add a Rust provider domain module with typed provider/model/runtime config, capability flags, health/discovery snapshots, run/event/checkpoint types, and an adapter trait.
- Extend schema-versioned settings with provider profiles and manual selection, preserving existing settings recovery behavior.
- Add scoped Tauri commands for provider catalog/configuration and availability inspection.
- Add a Providers view for configuration and honest capability/availability status.
- Add fixtures and focused native/frontend checks as appropriate.

## Verification Plan
- Rust unit checks for configuration validation, PATH discovery fixtures, secret-reference-only serialization, and adapter capability representation.
- Frontend typecheck/build to validate the Providers screen and typed invoke contract.
- Report fixture-level checks separately from live provider integrations; this phase does not run or authenticate provider CLIs.
