# Issue 14 Implementation Plan

## User Review Required
None. The selected GitHub-P1 scope adds local remote recognition and optional account references. It performs no network requests and stores no credential values.

## Acceptance Criteria Mapping
- Parse canonical github.com HTTPS and SSH remotes into owner/repository identity; reject other hosts and malformed paths.
- Persist optional GitHub account labels and opaque credential references; reject inline token values and invalid selections.
- Keep Git inspection local and read-only. Report remote authorization as unchecked, retaining local project details when access is unavailable.

## Proposed Changes
- Extend Rust Git remote facts with an optional canonical GitHub identity and fixture coverage.
- Extend schema-versioned settings with account references and a selected account; validate references natively and in browser preview.
- Add account configuration to Settings and display recognized repository/account state in project details.

## Verification Plan
- Rust formatting and focused/native tests for remote parsing, secret-reference validation, settings recovery, and persistence.
- Frontend production build for TypeScript and Vite output.
- No live GitHub authentication check is available in this phase; explicitly report that limitation.
