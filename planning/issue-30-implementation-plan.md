# Release-P3 implementation plan

**Issue:** #30 - Windows installer and verified release process
**Status:** Implemented; clean-VM acceptance checks remain pending
**User review required:** No product decision is needed for an unsigned, unpublished local release build. Distribution remains separately authorized.

## Acceptance criteria mapping

1. **App icons, NSIS, repeatable locked commands:** enable the existing branded PNG/ICO in Tauri, produce a per-user NSIS setup executable, keep WebView2 installation explicit, add locked verification/build commands, and synchronize the application version.
2. **Artifact, clean-machine, launch, and upgrade verification:** verify executable and installer metadata, exercise local launch and data-directory preservation where the host allows, and add fixture/contract checks for version identity and installer prerequisites. Clearly record which checks require a clean Windows VM and were not available here.
3. **Release and recovery docs:** publish a checklist, recovery/data backup instructions, unsigned-build status, code-signing strategy, updater strategy, and the boundary that publishing/distribution needs separate authorization.

## Proposed changes

- `src-tauri/tauri.conf.json`: enable NSIS bundling with the existing icons and per-user install mode.
- `package.json` and `scripts/`: add locked Windows release verification/build entrypoints and a release contract verifier.
- `src-tauri/Cargo.toml`, `src-tauri/Cargo.lock`, `package-lock.json`: synchronize the application version only.
- `docs/windows-release.md`, `docs/verification.md`: document commands, checks, recovery, signing/update policy, and evidence/limitations.
- `planning/issue-30-implementation-plan.md`: retain this acceptance mapping and verification record.

## Verification results

- `npm run release:verify`: passed for synchronized `0.19.0` manifests, icons, NSIS target, WebView2 mode, and release policy.
- `npm run release:windows`: passed on Windows with Node.js 24.14.0, npm 11.11.1, and Rust 1.96.0. The run completed locked dependency installation, formatting, 102 Rust unit tests, 6 release contract tests, the frontend build, the NSIS release bundle, artifact metadata verification, and isolated executable launch.
- Both executable and installer metadata report product/version `Navis-Fabrica` / `0.19.0`; their SHA-256 hashes and sizes are recorded in `docs/verification.md`.
- Clean-machine WebView2 setup and a live install-over-upgrade data preservation test remain pending because this environment has no disposable Windows VM. Do not claim these from source/configuration checks.
