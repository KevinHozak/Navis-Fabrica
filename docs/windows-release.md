# Windows Release and Recovery

## Scope and current release posture

The Windows release is a per-user NSIS installer with the bundled Navis-Fabrica application icons. It uses the WebView2 `downloadBootstrapper` mode: Windows 10 (April 2018 or later) and Windows 11 normally include WebView2; on a clean supported machine that lacks the runtime, setup downloads and runs Microsoft's bootstrapper and therefore needs internet access. The installer remains compact and does not carry the roughly 127 MB offline runtime.

This workflow creates local, unsigned artifacts. No Authenticode certificate, updater key, updater endpoint, or automatic updater is configured. Do not publish or distribute these artifacts from this issue; distribution requires a separate authorization. Before distribution, sign the executable and installer with an organization-approved Authenticode certificate and trusted timestamp service, protect signing credentials outside the repository, and verify signatures on the exact artifacts. Revisit an updater only with a separately approved key custody, endpoint, rollback, and signature-rotation plan. Until then, deliver updates by a separately approved full installer.

## Pinned release toolchain and commands

Use Windows x64 with the MSVC C++ build tools, Node.js `24.14.0`, npm `11.11.1`, and Rust `1.96.0` with `x86_64-pc-windows-msvc`. The repository pins these versions in `.node-version`, `package.json`, and `rust-toolchain.toml`. `npm ci` installs only the dependency graph in `package-lock.json`; Cargo commands use `Cargo.lock` in locked mode.

Before each new release, use the maintained `dev-version` workflow to bump the application SemVer and synchronize Tauri, Cargo, npm lockfile metadata, and versioned native window titles. Commit and sync that version change before final release checks and packaging. `dev-build` and `npm run desktop:build` package the version already declared by the repository; they never increment it. Do not build a new release before the version bump is complete.

Run the source/configuration checks on any platform:

```powershell
npm run release:verify
```

Build and verify the Windows release artifacts from the repository root:

```powershell
npm run release:windows
```

The Windows command performs `npm ci`, release-contract verification, Rust formatting and locked tests, a release NSIS build with `--locked` passed to Cargo, then Windows executable/installer metadata verification and an isolated executable launch smoke test. The smoke test points `APPDATA` and `LOCALAPPDATA` into a disposable temporary directory so it does not touch the signed-in user's app data. The expected outputs are under `src-tauri/target/release/` and `src-tauri/target/release/bundle/nsis/`. Preserve the command output and artifact hashes with any future release record; this issue does not publish the files.

## Release checklist

- [ ] Bump the application version for this release and verify all managed version fields agree before building.
- [ ] Confirm the five application version declarations agree and `npm run release:verify` passes.
- [ ] Run `npm run release:windows` on the pinned Windows toolchain.
- [ ] Confirm executable and installer file/product versions and product names match the declared application version.
- [ ] On a disposable clean Windows VM, install with WebView2 already present and absent. In the absent case, confirm the online bootstrapper installs WebView2 and setup launches the application afterward.
- [ ] On that VM, create test settings and run-history records, install the next build over the prior build, and verify settings/history remain. Also verify recovery by reinstalling the prior installer without deleting the app-data directories.
- [ ] Record OS build, tool versions, commands, artifact sizes/hashes, VM state, and observed results. Keep fixture and live-machine evidence distinct.
- [ ] Before any separate distribution request, select and document the signing identity, timestamp service, verification procedure, and delivery channel.

## User data and recovery

The application identifier is `com.navis.fabrica`; release builds must keep it stable. Settings are written under Tauri's per-app config directory (`%APPDATA%\com.navis.fabrica`, including `settings.json`). Workflow history and usage data are stored in Tauri's per-app data directory (`%APPDATA%\com.navis.fabrica`, including `workflow-runs.sqlite3` and usage data). WebView2 profile data is under the per-app local data directory (`%LOCALAPPDATA%\com.navis.fabrica`).

Before reinstalling for recovery, close Navis-Fabrica and copy both per-app directories above to a safe location. Reinstall the intended setup executable, launch it, and check that Settings and Runs open. If recovery needs a data restore, close the app first and restore the saved app-data directory; do not edit SQLite files while the app is running. Keep the backup until the restored app has been checked. Uninstalling the application is not a data-reset procedure; manage user data separately and deliberately.

## Verification boundary

`npm run release:verify` validates source manifests, icon file headers, NSIS/WebView2 configuration, and security posture. `npm run release:windows` validates build outputs and their Windows version resources. A successful build or source contract does not establish clean-machine WebView2 installation, interactive launch, or upgrade data preservation. Those are VM checks listed above and must be recorded only after they are run.
