# Verification history

The initial scaffold delivered only the basic structure. Its original verification is followed by phase-specific delivery evidence below.

- `npm install --ignore-scripts`: locked dependencies installed; audit reported zero vulnerabilities.
- `npm run build`: TypeScript and Vite production build passed.
- `cargo check --manifest-path src-tauri/Cargo.toml --locked -j 2` with `CARGO_PROFILE_DEV_DEBUG=0` and `CARGO_PROFILE_DEV_INCREMENTAL=false`: passed on Windows after removing this scaffold's generated cache and adding the required Windows icon. An earlier attempt hit low disk space.
- `npm run typecheck`, `cargo fmt --manifest-path src-tauri/Cargo.toml --check`, and staged whitespace checks: passed.
- Local roadmap validation: all 23 phase dependency references resolve, with no cycles.
- GitHub read-back: private repository; private linked Project #9 with Board layout, five status options and Priority. All 30 issues are open enhancements and all 30 cards are in Backlog.
- Native hierarchy read-back: Foundation/Discovery/GitHub/Routing/Release each have 3 sub-issues; Workflows/Providers each have 4, all 0 completed. Child membership and every native blocking dependency match planning/issue-map.json.
- Installer bundling remains disabled until Release-P3.

Do not interpret compile checks as live desktop UX or provider integration verification.

## Foundation-P1 shell

- The main sections use hash navigation (`#projects`, `#runs`, `#providers`, `#settings`) so browser history and the Tauri webview share the same navigation behavior.
- Settings remain a shell-only placeholder in Foundation-P1. Root persistence, validation and app-specific storage are deferred to Foundation-P2; no project scanning is performed in this phase.
- At the Foundation-P1 stage, the project panel used preview-state fixtures (`?demo=loading`, `?demo=empty`, or `?demo=error`); Discovery-P1 replaces that panel with live desktop scanning.
- The window and document title include the current package version. The shell uses system fonts and supports keyboard focus, narrow viewports, and reduced motion.

## Discovery-P1 bounded project scanner (issue #11)

### Fixture and compile verification

- `cargo test --locked --manifest-path src-tauri/Cargo.toml`: 13 tests passed on Windows. Fixtures cover spaces in paths, nested Git/coding projects, `.git` worktree pointer files, depth/hidden/exclusion settings, missing and non-folder roots, cancellation, broken worktree metadata, marker-named directories, and partial results. Unix-only permission-denied root/subfolder, duplicate-alias, and file-symlink fixtures are present but were not run on Windows.
- `cargo check --locked --manifest-path src-tauri/Cargo.toml`: passed.
- `npm run build`: TypeScript and Vite production build passed.
- A Windows symlink/junction cycle fixture was not run: creating a directory symlink failed with Windows error 1314 (required privilege not held). Unix builds run the duplicate-alias symlink fixture.
- An ACL-denied root was not exercised in this Windows environment. Unix fixtures exercise permission-denied roots and subfolders where the test user can enforce directory permissions. The scanner reports canonicalization and directory-read failures and preserves results from other folders.

### Live desktop integration

- Not run in this change. The Tauri window was not launched to invoke a scan against the configured live root. Native command compilation and scanner cancellation fixtures passed; end-to-end event delivery and desktop UI behavior remain unverified.

## Discovery-P2 Git facts and stack detection (issue #12)

- The scanner reports Git branch/state, dirty status including normal untracked files, latest commit hash/subject, remotes, and ahead/behind counts against the already available upstream ref. It never fetches or changes refs; Git status disables optional index writes and fsmonitor execution.
- Git inspection states include available, detached, unborn, unsafe ownership, unavailable Git executable, missing repository metadata, and generic inspection errors. Remotes have embedded credentials, query strings, and fragments removed before returning to the UI.
- Stack manifests include Tauri/Rust, Node.js, Go, Python, PHP, Ruby, Java/Gradle, .NET, Flutter, Android, iOS, Expo, and React Native. Toolchain presence is reported from executable discovery; it does not promise that a workflow integration is installed.
- `cargo fmt --manifest-path src-tauri/Cargo.toml --check`: passed.
- `cargo test --locked --manifest-path src-tauri/Cargo.toml`: 21 passed. Fixtures cover unborn and detached HEAD, clean/dirty worktrees, sanitized remotes, no upstream, ahead/behind divergence, Tauri/mobile manifests, and prerequisite reporting.
- `npm run build`: blocked because this worktree has no installed TypeScript executable. `npm ci --ignore-scripts --prefer-offline` failed inside npm with `Exit handler never called!`; frontend build/typecheck remains unverified.
- Live desktop scanning and live repositories outside fixtures remain unverified.

## GitHub-P1 auth and remote association (issue #14)

### Fixture and build verification

- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`: passed.
- `cargo test --manifest-path src-tauri/Cargo.toml --locked github_`: 2 passed. Fixtures cover HTTPS/SSH/scp-style GitHub remotes, rejection of non-GitHub and malformed/HTTP remotes, opaque credential-reference validation, selected-account validation, and settings recovery.
- Full `cargo test --manifest-path src-tauri/Cargo.toml --locked`: 25 passed, 2 failed for unrelated environment-sensitive checks. The rename fixture received Windows access denied while manipulating its temporary repository; the 100-project benchmark measured 5.32 seconds against its 5-second target. The issue-specific tests passed.
- `npm.cmd ci --ignore-scripts --no-audit --no-fund`: passed (72 locked packages installed).
- `npm run build`: TypeScript and Vite production build passed.
- `git diff --check`: passed.

### Live GitHub integration

- Not run. This phase performs local, read-only remote parsing and stores optional account labels plus opaque credential references; no GitHub authentication adapter exists to verify credentials or repository access. The UI reports access as unchecked and continues to show local project data. No live access result is inferred from fixture tests.

## GitHub-P3 issue picker and workflow context (issue #16)

### Fixture and build verification

- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`: passed.
- `cargo test --manifest-path src-tauri/Cargo.toml --locked github::tests`: 8 passed. Fixtures cover case-insensitive repository-scoped issue references, parent and checklist extraction, Markdown-wrapped linked PRs restricted to the selected repository, timeline PR filtering, bounded fields, plus existing GitHub parsing and status behavior.
- `cargo test --manifest-path src-tauri/Cargo.toml --locked`: 34 passed, 2 failed outside issue #16. The rename fixture hit Windows access denied; the 100-project benchmark measured 6.10 seconds against its 5-second target.
- `npm run build`: blocked because this worktree does not have the TypeScript compiler installed (`tsc` was not recognized). `npm ci --ignore-scripts --prefer-offline --no-audit --no-fund` failed with npm's internal `Exit handler never called!`; frontend typecheck/build remains unverified.
- `git diff --check`: passed.

### Live GitHub integration

- The connected GitHub integration read issue #16 successfully. The desktop app's direct API calls, native dependency lookup, and repository issue picker were not exercised live in this environment; no live blocker or PR-link behavior is claimed.

## Providers-P3 Antigravity coding adapter (issue #23)

- Installed Windows CLI probe: `agy --version` reported `1.0.10`. The executable is `agy.exe` on PATH.
- Installed-version noninteractive surface: `agy --sandbox --print-timeout <seconds>s --model <slug> --print <prompt>`. The installed `1.0.10` help exposes model selection, sandbox, print mode, and timeout; it does not expose the newer docs' `--output-format` or `--effort` flags. stdout/stderr are streamed through the run manager and cancellation terminates the process tree. Output is plain text/diagnostics, not structured per-step events.
- Skills: the CLI automatically discovers workspace skills under `.agents/skills` and its global Antigravity CLI skill folder. This adapter does not inject Codex-specific skill files.
- Headless limitations: approval requests, interactive user input, and resumable checkpoints are unsupported. The CLI permission policy may soft-deny tools that require interactive approval; the adapter never passes `--dangerously-skip-permissions`.
- Provider probe runs `agy --version` and `agy models` from a disposable temporary directory and omits raw diagnostics. The local model probe returned “Please sign in to view available models”; the profile is not authenticated in this environment. This is an explicit unavailable state, not evidence of a broken installation.
- A live model run in a Windows temporary repository was not possible because Antigravity authentication is unavailable. No live provider response is claimed. Rust fixtures cover installed-version argument construction and the declared capability matrix.
- Official references: [Headless mode](https://www.antigravity.google/docs/cli/headless/), [Agent skills](https://www.antigravity.google/docs/skills?tab=ide), and [CLI permissions](https://www.antigravity.google/docs/cli/modes).
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`: passed.
- `cargo test --manifest-path src-tauri/Cargo.toml --locked providers::tests::antigravity -- --nocapture`: 2 passed.
- `cargo check --manifest-path src-tauri/Cargo.toml --locked`: passed.
- `git diff --check`: passed.
- `npm run build`: unavailable because `tsc` is not installed in this worktree; frontend typecheck/build remains unverified.
- Full Rust suite: `cargo test --manifest-path src-tauri/Cargo.toml --locked` — 52 passed, 3 failed outside the changed adapter. The Windows project-rename fixture hit access denied; the Codex preflight fixture's temporary `git init` returned failure; and the 100-project scanner benchmark took 5.88 s against the 5 s target. The focused Antigravity tests and native compile check passed.

## Routing-P1 usage and latency signals (issue #25)

- Usage readings are persisted in the existing local SQLite run-history database. Categories remain separate: subscription windows, API spend, context limits, and local resources. Every reading carries units, provider/model identifiers (token-like values are replaced with `unknown`), collection time, source, confidence, status, and optional window/reset metadata.
- Codex app-server `account/rateLimits/read` readings are normalized from Unix epoch seconds to milliseconds. `thread/tokenUsage/updated` readings are recorded only when the app-server provides token usage and a model context window. Unsupported or absent API-spend and local-resource readings remain `unknown`; manually entered values are labelled estimates.
- Freshness windows are bounded by signal category. Expired readings show `stale`; unavailable and rate-limited responses remain distinct states. Run records now retain provider/model identifiers and latency in milliseconds derived from their start and first terminal timestamp. Recovered interrupted runs do not claim a measured latency across app downtime. Telemetry records do not contain prompts or provider output.
- Fixture verification: `cargo test --manifest-path src-tauri/Cargo.toml --locked --target-dir (Join-Path $env:TEMP 'navis-fabrica-issue25-target') usage::` — 6 passed. Covers reset normalization, category separation, unavailable/rate-limited/unknown values, stale readings, manual estimate validation, measured latency, and token-like identifier sanitization.
- Native compile: `cargo check --manifest-path src-tauri/Cargo.toml --locked --target-dir (Join-Path $env:TEMP 'navis-fabrica-issue25-target')` — passed. Formatting: `rustfmt --edition 2021 --check src-tauri/src/usage.rs src-tauri/src/runs.rs src-tauri/src/lib.rs` — passed. `git diff --check` — passed.
- Frontend typecheck could not run because `tsc` is not installed in this worktree; no frontend dependency installation was attempted. No live authenticated Codex provider run was performed, so no live account telemetry is claimed.
- Protocol references: [Codex app-server rate-limit fixtures](https://github.com/openai/codex/blob/main/codex-rs/app-server/tests/suite/v2/rate_limits.rs) and [Codex app-server notification registry](https://github.com/openai/codex/blob/main/sdk/python/src/openai_codex/generated/notification_registry.py).
