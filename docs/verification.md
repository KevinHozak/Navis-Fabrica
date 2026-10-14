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
- To preview the project panel fixtures, use the toolbar's **Preview state** selector, or open the app with `?demo=loading`, `?demo=empty`, or `?demo=error`. These are UI fixtures, not live filesystem results.
- The window and document title include the current package version. The shell uses system fonts and supports keyboard focus, narrow viewports, and reduced motion.
