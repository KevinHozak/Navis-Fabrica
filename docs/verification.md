# Initial scaffold verification

The initial scaffold delivered only the basic structure. Later phase work is recorded below; workflow execution, model routing, and live provider runs remain outside the completed phases so far.

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
