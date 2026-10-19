# Shared version preparation

`scripts/dev-version/prepare_version.py` prepares a synchronized Tauri version for both dev-push and dev-build. It performs no build, network request, staging, commit, push, branch switch or synchronization. The caller retains those actions and their verification.

## Prepare before either workflow

```powershell
python C:/Projects/Navis-Fabrica/scripts/dev-version/prepare_version.py prepare --repo <app-root> --state-dir <external-state-directory> --base <verified-local-baseline>
```

On first use, supply an existing local baseline whose version and source identity have been checked. A missing state file does not establish that an app has never been built or published. Consult prior release evidence first. If the current synchronized version is already newer than the baseline, preparation adopts it. Equal versions with changed app content allocate the next patch; identical content seeds a baseline without a bump. A baseline-only record does not consume the first future bump. An existing baseline ref is read locally; building never requires fetching or pulling.

After initialization, omit `--base`. Repeat the same command for build-first, push-first and retries. Both workflows must use the same state directory and app source profile. `FABRICA_VERSION_STATE_DIR` supplies a shared default; otherwise state uses the platform's external local state directory under `Navis-Fabrica/version-preparation`. Installed personal skills explicitly select their configured private state directory. Runtime state is never committed.

The JSON result includes `version`, `token`, `fingerprint`, `action`, `managed_paths`, and separate `last_build`/`last_push` receipts. Include all managed version edits in the scoped push, and run final checks after preparation. Committing identical content changes HEAD without changing the prepared version or token. Failed attempts leave the allocation reusable. Corrections before either success may update its content fingerprint without consuming another version. New app content after a successful build or push allocates the next patch; old success receipts remain visible separately from the new preparation.

Use `--version <stable-semver>` for an exact target. Downgrades and reusing a completed version for new app content are rejected. Unexpected managed-field mismatches require explicit reconciliation with the existing setter; preparation does not silently repair them. The legacy `bump_version.py` remains available for explicit ref-based calls, but automatic push/build workflows use this shared helper.

## Record only observed success

After building, checking the exact executable/installer version and completing applicable startup verification:

```powershell
python C:/Projects/Navis-Fabrica/scripts/dev-version/prepare_version.py record-build --repo <app-root> --state-dir <external-state-directory> --token <prepared-token> --artifact <release-executable> --artifact <installer>
```

The helper rechecks source/version freshness and stores artifact sizes and SHA-256 hashes. It does not prove that artifacts were built from those files, verify executable metadata, or perform the startup check; the workflow must observe those facts before calling it. Do not record an artifact left by a failed build as a success.

After pushing, independently read back the remote branch SHA, then:

```powershell
python C:/Projects/Navis-Fabrica/scripts/dev-version/prepare_version.py record-push --repo <app-root> --state-dir <external-state-directory> --token <prepared-token> --commit <delivered-commit> --remote-head <observed-remote-sha>
```

The helper verifies the local commit's app source and managed versions against preparation and requires its SHA to equal the caller's observed remote SHA. It never queries the remote itself. Dirty app changes excluded from that commit cannot be represented as published. Failed or ambiguous push outcomes must be reconciled by the caller before recording success. If publication succeeded but receipt persistence failed, read back and record that same commit; do not bump or republish. Repeating identical success receipts is safe.

Inspect preparation and separate success receipts without changing manifests:

```powershell
python C:/Projects/Navis-Fabrica/scripts/dev-version/prepare_version.py status --repo <app-root> --state-dir <external-state-directory>
```

Status reports `source_matches`, `version_matches`, `built_current_source` and `pushed_current_source` separately. A historical receipt remains visible without claiming it applies to newly edited source.

## App source profile

The default profile includes `src/`, `src-tauri/`, `runner-core/`, `public/`, root npm/Cargo manifests and lockfiles, root HTML/Vite/TypeScript/toolchain configuration, and `scripts/build-worker.mjs`. Source inventory includes tracked content, tracked deletions and nonignored untracked files. Generated `target/`, `node_modules/`, `dist/` and Git/worktree metadata are excluded. Markdown/reStructuredText documentation is excluded unless its individual path is explicitly included as an imported app input. Only the managed application versions and existing version-bearing titles are masked; dependency versions and version literals in code remain meaningful content. UTF-8 text line endings are canonicalized so CRLF checkout and LF commit content compare consistently. Binary content is hashed unchanged.

For a different app layout, repeat `--include <app-relative-file-or-directory>` to replace the default roots. Managed version files are always included. Declare all runtime source, assets and build inputs, including any imported documentation or custom build scripts that need a custom profile. Review the default inventory for each new project; the helper cannot infer every build system dependency. The chosen profile is persisted and cannot silently change on later calls. An app in a monorepo is selected by passing its frontend directory as `--repo`; additional paths must remain within that app profile. Symlink source and Git submodule entries are unsupported and fail explicitly. Source reads are limited to 32 MiB per file, 256 MiB total and 20,000 files.

## Durable preparation and recovery

State identity uses the resolved Git common directory plus the app's repository-relative location, so linked checkouts share the same app record. A process-held OS lock serializes decisions; process exit releases the lock without guessing whether an old PID is still alive. State is atomically replaced and contains a pending preparation intent before any managed version file is written.

A retry completes an interrupted write only when app content still matches the saved intent and each managed version is either its old or intended value. It then reuses the same allocation/token. Source drift, unrelated versions, malformed state and changed profiles stop for reconciliation. Do not delete state to force a bump. The record is local operational evidence, not independent proof of a build or remote publication; neither version numbers nor timestamps substitute for the recorded source and observed effects.

Behavioral tests: `python -m unittest discover -s scripts/tests -p test_version_preparation.py -v`.
