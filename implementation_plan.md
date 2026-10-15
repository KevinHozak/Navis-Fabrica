# Implementation Plan — Jev-P2: Define bounded task-triage requests (#60)

## User review required

None. This issue defines an internal Rust contract and has no UI, live-key, or live-service requirement.

## Acceptance criteria mapping

| Criterion | Planned implementation |
| --- | --- |
| Bounded request with summary, capabilities, eligible IDs and minimal metadata | Dedicated strict Rust DTOs and validated TypeSafe System One request builder. |
| Jev Choice response and per-choice confidence | Strict parser keyed to the submitted route IDs; selected probability is returned as confidence. |
| Reject invalid selection/schema/probabilities | Typed sanitized errors for unknown routes, missing distribution entries, invalid values, and malformed Choice answers. |
| Exclude repository files, diffs, credentials and full issue criteria | Builder accepts only a bounded summary, capability enum list, and minimal route metadata; deny unknown serialized fields. |
| Bound bodies, timeout, sanitized errors | Fixed request/response byte limits, a short HTTP timeout, redirect refusal, and static public error messages. |

## Proposed files

- `src-tauri/src/jev.rs` — request contract, response parser, bounded HTTP client, and fixture tests.
- `src-tauri/src/lib.rs` — register the Jev contract module for the Rust application.
- `docs/verification.md` — record fixture coverage and explicitly state no live call was made.

## Verification

- Run focused Rust module tests if the crate supports filtering; otherwise run `cargo test --locked --manifest-path src-tauri/Cargo.toml jev::`.
- Run formatting check for the changed Rust files.
- Inspect the diff and verify no frontend or credential-bearing state is involved.
