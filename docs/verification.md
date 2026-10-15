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

## Release-P3 Windows installer and release process (issue #30)

### Release configuration

- The application version is synchronized to `0.19.0` in `package.json`, the package-lock root and app entry, `src-tauri/Cargo.toml`, the `navis-fabrica` package entry in `src-tauri/Cargo.lock`, and `src-tauri/tauri.conf.json`.
- Tauri bundling is enabled for the NSIS target only. The existing 512×512 PNG and six-image ICO are included. NSIS defaults to current-user installation; WebView2 uses the silent `downloadBootstrapper` mode. Windows 10 (April 2018 or later) and Windows 11 normally include WebView2. On a clean machine without the runtime, setup requires internet access to download it. This follows the documented [Tauri Windows installer modes](https://v2.tauri.app/distribute/windows-installer/).
- Node.js `24.14.0`, npm `11.11.1`, and Rust `1.96.0`/`x86_64-pc-windows-msvc` are pinned. `npm run release:verify` validates source versions, icons, release targets, WebView2 setup, stable app identity, and unsigned/no-updater configuration. `npm run release:windows` performs locked install, format, test, release build, artifact metadata checks, and an isolated executable launch smoke test.
- Artifacts are unsigned and updater generation is disabled. No release was published or distributed.

### Windows release evidence

- Host toolchain: Node.js `24.14.0`, npm `11.11.1`, Rust `1.96.0`, Windows MSVC x64.
- `npm run release:verify`: passed.
- `npm run release:windows`: passed, including `npm ci` (72 packages), `cargo fmt --check`, 102 Rust unit tests, 6 Rust release contract tests, the Vite production build, and Tauri `build --bundles nsis --ci -- --locked`.
- Produced `src-tauri/target/release/navis-fabrica.exe` (15,558,144 bytes) and `src-tauri/target/release/bundle/nsis/Navis-Fabrica_0.19.0_x64-setup.exe` (4,175,666 bytes / 3.98 MiB). Both report FileVersion `0.19.0`, ProductVersion `0.19.0`, and ProductName `Navis-Fabrica`.
- SHA-256 executable: `45FF3C581172FE37A785886C2A1C17C27BEA4B72CE06C7D88C4DAFE429085337`.
- SHA-256 installer: `42341858301FC3B62F37C147D67E9A6E990BE7AD6D729B529F75047885C9EAEE`.
- The release executable stayed running for 8 seconds with `APPDATA` and `LOCALAPPDATA` pointed to a disposable temporary profile, then closed successfully. This avoids changing the signed-in user's app data.

### Not verified in this environment

- No clean Windows VM or Windows Sandbox was available. Therefore setup behavior on a machine without WebView2, WebView2 installation from the bootstrapper, and installation over a prior released installer were not observed live.
- Upgrade preservation is supported by keeping identifier `com.navis.fabrica` unchanged and keeping settings/run data in the existing Tauri per-app config/data directories; this source/configuration check is not a live installer-upgrade test. The release checklist in `docs/windows-release.md` keeps the clean-VM cases as required pre-distribution evidence.
- The local executable launch confirms this host can start the app. It does not establish clean-machine compatibility or authorize distribution.

## Release-P2 performance, accessibility, and diagnostics (issue #29)

### Measurement boundaries and baselines

- The desktop records `entrypointToMainPageLoadMs` from the Rust application entrypoint through the first completed load of the `main` Tauri page. It is included in the redacted diagnostics export; it excludes Windows process creation and image loading. Browser-only exports have no native startup measurement.
- Each completed project scan reports request-to-finish elapsed milliseconds, directories visited, and projects found in the Projects view. It also reports time to the first progress event that contains a discovered project. Times use the webview's monotonic `performance.now()` clock.
- Scanner fixture targets remain first discovered project under 1 second and full scan under 5 seconds for 100 direct child project folders. The fixture is defined in `src-tauri/src/scanner.rs`; these targets are comparative test gates, not claims of live-root performance. On this Windows 10.0.26300 host (8 logical processors), the 100-project fixture measured first result at 33.32 ms and completion at 211.87 ms (101 folders visited). Prior Windows runs recorded 5.32 s, 6.10 s, and 5.88 s against the 5 s gate; keep those historical outliers as regressions to investigate.
- Output-flood fixture input is 200 chunks × 16 KiB (3.125 MiB). The per-run persisted/streamed output cap is 512 KiB, with a 2,000-event cap per run. Run history retains at most 100 runs. The diagnostics export reports these limits and aggregate event/run counts without including event text.
- This verification host reports Microsoft Windows version `10.0.26300` and 8 processors to the process. CPU model and physical memory were unavailable because CIM access is denied in the managed environment. No machine-specific timing result is inferred from these facts.

### Accessibility, responsiveness, and data controls

- Project discovery already uses a virtualized listbox with Arrow Up/Down, Home/End navigation, active-descendant, selected-state, and set-position metadata. Run output now batches event-driven React updates once per animation frame and bounds queued frontend events to 2,000.
- The global 2 px `:focus-visible` outline and `prefers-reduced-motion` override remain enabled. Contract tests check the listbox keyboard/ARIA markers and these CSS behavior hooks. The quiet text color was raised to `#8492a4`: measured contrast is 6.06:1 against the canvas `#0c0f14`, 5.52:1 against panel `#141a23`, and 5.30:1 against the project-row surface `#151e29`; a Rust contract test enforces at least 4.5:1 for all three.
- Diagnostics export is an allowlist of version, startup timing, retention limits, and aggregate counts by known state/provider/action/event kind. It excludes per-run IDs, executable names/arguments, local paths, thread IDs, model/profile identifiers, prompts, issue URLs, handoff content, and event text. Tests assert those exclusions, including a GitHub-token-shaped fixture.
- The run-history screen offers explicit JSON export and a confirmation before deleting completed history. Deletion preserves queued/running/needs-input/cancelling records, removes deleted IDs from SQLite, and reloads the remaining history.

### Verification in this environment

- `npm ci --ignore-scripts --no-audit --no-fund`: passed; 72 locked packages installed.
- `npm run build`: passed; TypeScript and Vite production build completed (`334.53 kB` JavaScript, `99.21 kB` gzip).
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`: passed.
- `git diff --check`: passed.
- `cargo test --locked --manifest-path src-tauri/Cargo.toml`: passed; 102 library tests and 6 release security/performance contract tests passed. This includes the output-flood cap, privacy allowlist, deletion persistence, scanner benchmark, accessibility/contrast contracts, and capability/CSP checks.
- The Tauri dev window launched under the temporary `com.navis.fabrica.issue29test` identity with title `Navis-Fabrica · v0.18.3`. The Windows Computer Use approval timed out before accessibility-tree inspection, so keyboard/focus behavior and the exported startup value were not observed live. The fixture results and source-level accessibility checks are not represented as a live interaction test.
- CIM refused access to the CPU model and physical-memory fields; fixture and live integration results remain separate.

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

## Providers-P4 Ollama coding harness and portable handoff (issue #24)

- Harness evaluation: Aider's official Ollama integration uses `ollama_chat/<model>`; its CLI supports one-shot messages, test commands, and disabling auto-commits. This gives Navis-Fabrica a process-managed code-edit and test harness without implementing another agent loop. See [Aider's Ollama integration](https://aider.chat/docs/llms/ollama.html) and [CLI options](https://aider.chat/docs/config/options.html).
- Model discovery uses Ollama's local `/api/tags` plus `/api/show`. The probe reads each model's advertised tool capability and architecture context length; it does not download models. See [Ollama's model-list API](https://docs.ollama.com/api/tags), [model-details API](https://docs.ollama.com/api-reference/show-model-details), and [tool-calling capability](https://docs.ollama.com/capabilities/tool-calling).
- Local-only profiles accept numeric IPv4/IPv6 loopback HTTP endpoints only. Provider credentials, proxy variables, environment-file secrets, Aider telemetry, update checks, and automatic Git commits are excluded from this run path. The probe has bounded request timeouts and does not follow redirects.
- Models are eligible only when Ollama reports `tools` and at least 8,192 context tokens. Missing metadata is shown as unknown and is not eligible. Aider's documentation warns that small Ollama contexts can silently discard work; the adapter uses the available physical-memory to model-size ratio to choose a bounded timeout from 300 to 3,600 seconds. GPU memory is not measured, so this timeout is an estimate based on system RAM, not a GPU-capacity claim.
- Optional tests use a user-entered test command passed as one process argument to Aider. It can modify the selected project, and its result is visible in the managed run output.
- A handoff packet is saved on a terminal source run with goal, scope, diff, prior checks, pending work, and recorded commit/branch/PR effects. The user chooses a target provider, saves the checkpoint, then separately presses Start. The target prompt explicitly says not to repeat recorded remote effects; no provider switch or side effect is automatic.
- Aider receives the prompt through `--message` (inline text), not `--message-file` (which expects a path). After navigation or restart, a saved packet has an explicit Load action; it is enabled only when its target provider is selected.
- Handoffs capture their canonical project worktree, changed paths, staged and unstaged binary diffs, and hashes for untracked files. A target run may proceed with the exact saved snapshot only; changed paths, changed content, a different worktree, or additional unrelated edits require a fresh handoff and are left untouched. Older packets without a snapshot still require a clean worktree.
- Live Windows probe: `ollama.exe` is installed, but `http://127.0.0.1:11434/api/tags` was unavailable; Aider is not installed/on PATH. Therefore no live Ollama model capability or Aider run is claimed. The UI surfaces these states and fixture tests cover parsing/eligibility.
- `cargo check --manifest-path src-tauri/Cargo.toml --locked --offline`: passed.
- `npm ci --ignore-scripts` restored locked dependencies; `npm run build`: passed (TypeScript and Vite).
- `cargo test --manifest-path src-tauri/Cargo.toml --locked --offline providers::tests::ollama -- --nocapture`: 3 passed (loopback policy, context/tool eligibility, and RAM-based timeout policy).
- `cargo test --manifest-path src-tauri/Cargo.toml --locked --offline providers::tests::aider_invocation_pins_local_chat_model_and_disables_remote_side_effects -- --nocapture`: 1 passed.
- `cargo test --manifest-path src-tauri/Cargo.toml --locked --offline runs::tests::history_survives_restart_and_orphans_become_interrupted -- --nocapture`: 1 passed, including handoff packet persistence and completed side-effect retention.
- `cargo test --manifest-path src-tauri/Cargo.toml --locked --offline`: full current suite passed, 75 passed, 0 failed. This supersedes earlier baseline failures recorded above for the issue #24 verification run.
- Focused regression fixtures cover Aider inline-message arguments and exact dirty-worktree handoff matching, including rejection of changed and unrelated files.
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check` and `git diff --check`: passed.

## Routing-P1 usage and latency signals (issue #25)

- Usage readings are persisted in the existing local SQLite run-history database. Categories remain separate: subscription windows, API spend, context limits, and local resources. Every reading carries units, provider/model identifiers (token-like values are replaced with `unknown`), collection time, source, confidence, status, and optional window/reset metadata.
- Codex app-server `account/rateLimits/read` readings are normalized from Unix epoch seconds to milliseconds. `thread/tokenUsage/updated` readings are recorded only when the app-server provides token usage and a model context window. Unsupported or absent API-spend and local-resource readings remain `unknown`; manually entered values are labelled estimates.
- Freshness windows are bounded by signal category. Expired readings show `stale`; unavailable and rate-limited responses remain distinct states. Run records now retain provider/model identifiers and latency in milliseconds derived from their start and first terminal timestamp. Recovered interrupted runs do not claim a measured latency across app downtime. Telemetry records do not contain prompts or provider output.
- Fixture verification: `cargo test --manifest-path src-tauri/Cargo.toml --locked --target-dir (Join-Path $env:TEMP 'navis-fabrica-issue25-target') usage::` — 6 passed. Covers reset normalization, category separation, unavailable/rate-limited/unknown values, stale readings, manual estimate validation, measured latency, and token-like identifier sanitization.
- Native compile: `cargo check --manifest-path src-tauri/Cargo.toml --locked --target-dir (Join-Path $env:TEMP 'navis-fabrica-issue25-target')` — passed. Formatting: `rustfmt --edition 2021 --check src-tauri/src/usage.rs src-tauri/src/runs.rs src-tauri/src/lib.rs` — passed. `git diff --check` — passed.
- Frontend typecheck could not run because `tsc` is not installed in this worktree; no frontend dependency installation was attempted. No live authenticated Codex provider run was performed, so no live account telemetry is claimed.
- Protocol references: [Codex app-server rate-limit fixtures](https://github.com/openai/codex/blob/main/codex-rs/app-server/tests/suite/v2/rate_limits.rs) and [Codex app-server notification registry](https://github.com/openai/codex/blob/main/sdk/python/src/openai_codex/generated/notification_registry.py).

## Providers-P5 OpenRouter model provider (issue #48)

- OpenRouter authentication is entered in the desktop provider panel and passed directly to a Rust command. Rust stores it in Windows Credential Manager and settings retain only the opaque target reference. Model discovery uses the bounded authenticated `/api/v1/models` endpoint; authentication failures, rate limits, and unavailable/malformed responses are separate states, and diagnostics omit server response bodies.
- The catalog displays only returned metadata: context length, prompt/completion price, input/output modalities, and supported parameters. Omitted values remain unknown. Refreshing the catalog preserves an existing selected model if present and clears a selection that disappeared; it does not choose a replacement.
- OpenRouter catalog persistence and settings validation share a 500-model limit; other provider profiles retain the 200-model limit. OpenRouter model names may use the catalog's 160-character bound.
- An OpenRouter credential reference must exactly match the target generated from that profile's ID before Rust reads the credential. Other provider references are rejected before Credential Manager lookup.
- Runs require the configured Aider executable, selected OpenRouter model, and a clean project Git worktree. Aider provides file editing and optional user-configured tests; OpenRouter provides model inference. The API key is passed to the child process through `OPENROUTER_API_KEY`, excluded from arguments/settings, and registered for output redaction. Timeout and run-manager cancellation apply.
- Official references: [OpenRouter model catalog API](https://openrouter.ai/docs/api/api-reference/models/get-models), [Aider OpenRouter integration](https://aider.chat/docs/llms/openrouter.html), and [Aider configuration options](https://aider.chat/docs/config/options.html).
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`: passed.
- `cargo test --manifest-path src-tauri/Cargo.toml --locked --target-dir src-tauri/target/issue48-alt`: 81 passed, 0 failed, including catalog metadata/unknown-field handling, 500-model persistence bounds, malformed response, authentication rejection, rate limiting, profile-bound credential references, Aider argument safety, and child-output secret redaction.
- `cargo check --manifest-path src-tauri/Cargo.toml --locked --target-dir src-tauri/target/issue48-alt`: passed for the Windows Tauri target, including credential-manager APIs and command registration.
- `npm ci --ignore-scripts --no-audit --no-fund`: installed all 72 locked packages; `npm run build`: passed (TypeScript and Vite production build).
- No live OpenRouter request was performed; a real key is not required by automated tests. Run cancellation uses the already tested process manager; no live Aider/OpenRouter run was attempted.

## Release-P1 cross-feature regression and security checks (issue #28)

### Fixture and contract coverage

- Existing disposable scanner and Git fixtures cover missing/unreadable roots, partial results, cancellation, worktree metadata, remotes, branch divergence, and project identity changes. Run-manager fixtures cover invalid requests, process lifecycle and cancellation, exact handoff snapshots, restart recovery, output redaction, scoped changes, and verified side-effect receipts; a Windows fixture now explicitly rejects shell metacharacters in `.cmd` paths and arguments, and persisted workflow keys are checked for duplicate suppression. Provider fixtures use recorded protocol/catalog responses for malformed data, authentication/rate-limit failures, missing capabilities, secret-safe diagnostics, and local-only behavior. Routing fixtures cover unavailable or unknown usage, exhausted candidates, checkpoint gating, local-only policy, and manual overrides.
- Added `src-tauri/tests/release_security_contract.rs`. These checks compare the default window's granted IPC commands with the registered Tauri handlers and each generated per-command permission, bound production and development CSP origins, and reject a frontend dependency/reference to the unrestricted shell plugin.
- The IPC check validates registration and capability configuration; it does not invoke commands through a running webview. Scanner, Git, process, provider, and routing behavior is exercised by their module-level fixtures rather than a live desktop session.
- `cargo test --locked --offline --manifest-path src-tauri/Cargo.toml`: 99 unit tests and 3 release security contract tests passed on Windows.
- No live provider workflow, authenticated network request, or desktop IPC session was run for this issue. Fixture success is not represented as live integration evidence. Existing provider-specific live probe observations are recorded with their respective phase entries above.
