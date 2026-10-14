# Issue 15 Implementation Plan

## User Review Required
None. This phase adds optional read-only GitHub enrichment; project discovery remains local and succeeds independently of network access.

## Acceptance Criteria Mapping
- Open issue and pull request totals, recent commit summary, repository links, and remote freshness are exposed independently of local Git freshness.
- GitHub pagination, API rate limits, missing fields, and explicit refresh are represented in typed results. Authentication availability is reported honestly; no credential values enter the frontend or logs.
- Remote errors preserve local project data and do not expose repository data when GitHub denies access.

## Proposed Changes
- Add a native GitHub API client and bounded in-memory activity cache, with typed freshness/auth/error states and conservative pagination.
- Add narrowly scoped Tauri commands for cached reads and explicit refresh.
- Add activity state and refresh controls to repository details, with external links opened only from canonical github.com URLs.
- Add fixture tests for API parsing, pagination/link handling, and unavailable/rate-limit/auth responses.

## Verification Plan
- Run focused Rust tests and `cargo check --locked` for the Tauri backend.
- Run frontend typecheck and production build.
- Treat fixture checks separately from live GitHub requests; live integration is optional and must not log credentials or response bodies.

## Verification Results
- `cargo check --manifest-path src-tauri/Cargo.toml --locked` passed.
- The four GitHub fixture tests passed. The focused 100-project scanner benchmark passed when run alone.
- Full `cargo test --manifest-path src-tauri/Cargo.toml --locked` ran 32 tests; 30 passed. Two existing scanner tests failed in this environment: a temporary-directory rename returned Windows access denied, and the concurrent 100-project benchmark exceeded its five-second threshold. The rename test also failed when rerun alone.
- `npm run build` passed (TypeScript and Vite production build).
- No live GitHub API request was made. Fixture results are not live integration evidence.
