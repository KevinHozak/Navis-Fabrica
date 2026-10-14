# Initial scaffold verification

Only the basic structure is in scope. No scanner, workflow runner, provider adapter, model routing, or dashboard functionality is implemented.

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

- `cargo test --locked --manifest-path src-tauri/Cargo.toml`: 12 tests passed on Windows. Fixtures cover spaces in paths, nested Git/coding projects, `.git` worktree pointer files, depth/hidden/exclusion settings, missing and non-folder roots, cancellation, broken worktree metadata, and partial results. Unix-only permission-denied root/subfolder and duplicate-alias fixtures are present but were not run on Windows.
- `cargo check --locked --manifest-path src-tauri/Cargo.toml`: passed.
- `npm run build`: TypeScript and Vite production build passed.
- A Windows symlink/junction cycle fixture was not run: creating a directory symlink failed with Windows error 1314 (required privilege not held). Unix builds run the duplicate-alias symlink fixture.
- An ACL-denied root was not exercised in this Windows environment. Unix fixtures exercise permission-denied roots and subfolders where the test user can enforce directory permissions. The scanner reports canonicalization and directory-read failures and preserves results from other folders.

### Live desktop integration

- Not run in this change. The Tauri window was not launched to invoke a scan against the configured live root. Native command compilation and scanner cancellation fixtures passed; end-to-end event delivery and desktop UI behavior remain unverified.
