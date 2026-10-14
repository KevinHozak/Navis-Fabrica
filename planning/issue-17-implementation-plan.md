# Workflows-P1 Issue 17 Implementation Plan

## User Review Required
None. This phase declares actions and resolves skill references only. It does not start workflows or alter projects.

## Acceptance Criteria Mapping
- Declare typed action inputs, compatible stacks, skill reference, checks, and expected output artifacts for dev-work, dev-push, and dev-build.
- Resolve only referenced installed skill files from the configured Codex skills directory. Report absence or incompatibility as a disabled reason; never execute Markdown.
- Represent commit, push, and PR creation as permitted only by explicit opt-in, with no effects requested by default. Set dev-build's default profile to release.

## Proposed Changes
- Add a typed frontend action registry and project/issue compatibility evaluator.
- Add a narrow Tauri command to check the three known skill references in the configured Codex skills folder.
- Add the action registry to Runs and explain each disabled reason, inputs, checks, artifacts, effect scope, and release default.
- Add a disposable Rust fixture test for allowlisted skill resolution.

## Verification Plan
- Run the focused Rust fixture and locked Tauri `cargo check`.
- Run `npm run build` and Rust formatting check.
- Record local skill discovery as machine evidence, separately from fixture coverage; do not execute skill files or workflow commands.

## Verification Results
- `npm run build` passed at version 0.12.0 (TypeScript check and Vite production build).
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check` passed.
- `cargo check --manifest-path src-tauri/Cargo.toml --locked` passed.
- Focused fixture `cargo test --manifest-path src-tauri/Cargo.toml --locked discovers_only_known_skill_references_inside_the_codex_skills_root` passed (1 test).
- Local integration evidence: `dev-work`, `dev-push`, and `dev-build` SKILL.md references all exist under the user's Codex skills folder. No skill Markdown was executed and no workflow process was started.
- Tauri IPC was compile-checked; no live desktop IPC session was available in this validation run.