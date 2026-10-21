# Commit message drafting latency benchmark

Date: 2026-10-21. Scope: synthetic, non-production benchmark only. No production source, settings, startup entries, credentials, commits, or releases were changed.

## Method

Three repetitions each for a small SaveButton accessibility diff and a medium commit-dialog/message-validation diff. The same synthetic diffs and two-string JSON output requirement were used across routes; each provider used its native JSON/schema control. No repository diff was sent to any provider.

- **Fabrica's current Codex route:** `gpt-6-luna` low using the same newline-delimited Codex app-server protocol and explicit model, read-only ephemeral thread, and no tools. Startup is process launch through initialization and `thread/start`; generation is `turn/start` through `turn/completed`.
- **Gemini API:** configured API credential, `gemini-3.5-flash-lite`, streamed JSON. First text and complete response measured. Current model name was discovered from the API's response after the older `gemini-2.5-flash-lite` identifier returned 404.
- **Ollama:** installed `qwen2.5-coder:7b`, JSON mode, three back-to-back calls per diff. The first small case loaded the model from idle; all later calls were warm. The model was unloaded after the run.

These are one-machine, low-sample measurements, not service-level guarantees. Output constraints matched; the Codex app-server's project/branch/path wrapper differs from the direct API and local harness. Network and machine load can move these numbers.

## Results

| Route | Small | Medium | Warm behavior | JSON / quality |
|---|---:|---:|---|---|
| Codex app-server (`gpt-6-luna`, low) | 8.90s average (1.26s startup + 7.64s generation) | 9.27s (1.41s startup + 7.86s generation) | Fresh app-server each call; no reliable repeated-call improvement appeared. | 6/6 valid; short relevant subjects; medium subjects sometimes omit the draft-button detail. |
| Gemini API (`gemini-3.5-flash-lite`) | 0.98s average; first text 0.91s | 0.81s; first text 0.72s | Repeats were 0.74–1.02s total; no meaningful cold/warm gap isolated. | 6/6 valid; concise, accurate subjects; medium bodies captured more of the diff. |
| Ollama (`qwen2.5-coder:7b`) | 28.10s first (15.28s load, 7.87s prompt evaluation, 4.86s generation); 5.40s warm average | 19.72s first prompt; 5.02s warm average | Model load fell to ~9–23ms after first load. Prompt evaluation fell from 7.87s / 15.38s to ~0.27–0.32s on repeats, consistent with a useful warm prefix/cache effect. | 6/6 valid; concise and accurate; warm medium subject/body left out some detail. |

For Codex, app-server startup is only about 1.0–1.8s of roughly 8–10s total. A persistent app-server could hide or save much of that startup portion, but the 7.3–8.2s generation time remains. Gemini was the clear fastest measured route by end-to-end time. Ollama can stay local and private, but on this machine's CPU-only execution the cold call is poor and warm calls remain ~5s.

## Available runtime and local startup evidence

- Codex CLI 0.162.1 was signed in. Antigravity CLI 1.0.10 was installed but signed out, so it was not benchmarked. `gcloud` showed no active account. Ollama was reachable after the elevated read-only check and had five installed models: `qwen2.5-coder:7b` (4.7 GB on disk), `Kimiko-20B` (12 GB), `gemma3:4b` (3.3 GB), `llama3.2:1b` (1.3 GB), and `moondream` (1.7 GB). Only Qwen was benchmarked.
- The machine reported 63.8 GiB RAM, 40.1 GiB free before loading Qwen. Ollama reported Qwen using 100% CPU and about 5.1 GB resident model memory while loaded. Its tray UI used about 3–4 GB in the observed process snapshots; this was a transient observation, not a steady-state profile. Keeping Qwen warm therefore costs roughly 5 GB plus server/UI overhead, and it did not use the GPU in this run.
- A Windows Startup-folder script `OllamaServer.vbs` already exists and launches `ollama.exe serve` hidden. At inspection, the server process had started at 08:21 and the tray UI at 08:51; the server was already running before the tray UI opened. No matching Run key, scheduled task, or service entry was found. So this snapshot does not show Ollama auto-start as absent; it shows server and tray UI can be separate. It cannot establish why the user sometimes does not see the tray app after login.
- After the benchmark, `ollama stop qwen2.5-coder:7b` unloaded the model; the daemon remained active as before. No startup preference was changed.

## Recommendation

1. **Fastest measured option:** use a direct Gemini Flash-Lite API call for this narrow drafting task if sending the bounded diff to Google's API and API billing are acceptable. This requires a separate Fabrica route; today's source pins drafts to Codex and forbids silent model/provider fallback.
2. **Private/offline option:** keep Ollama's daemon available, but load the model only when useful and let a short keep-alive expire when idle. Warm Qwen materially improves latency, but still trails Gemini here and occupies about 5 GB of system memory on CPU. A smaller installed model could be evaluated next, but is unmeasured.
3. **Codex-compatible option:** a reusable app-server could save roughly the measured 1.2–1.4s process/bootstrap cost. It is not the main fix for the current pause; model generation dominates. Persistent processes also need lifecycle, authentication-refresh, restart, and memory handling.
4. **Hide latency without blocking the interface:** keep project discovery and changed-path display immediate; start lightweight bounded diff/fingerprint capture asynchronously. Warm a selected provider in the background during Fabrica startup without waiting for it. After a project is selected and its diff settles, draft opportunistically in the background and show a ready suggestion when available. Debounce and coalesce changes; do not queue one request per file event or pre-draft every project.
5. **Cache suggestion separately from commit authorization:** key exact results by snapshot fingerprint for deduplication, but keep the previous wording as provisional across small edits. Whitespace-only or similarly minor edits can retain it; added/deleted files or clear semantic changes can trigger a background refresh while the existing wording remains visible. Significance is heuristic, so label reused wording as potentially stale. Always bind the final reviewed commit to a freshly checked exact repository fingerprint; draft reuse must never relax commit preflight.

This approach can move most waiting out of the moment the user opens Push and have a suggestion ready, but it does not remove inference latency. If pre-drafting starts only after project selection, the user may still see a short wait; keep editing and manual confirmation usable throughout.

## Implementation and release

- Fabrica now calls Google's Gemini Developer API `generateContent` endpoint with the measured `gemini-3.5-flash-lite` model. It reads `GEMINI_API_KEY` only in the Rust backend, sends it as an HTTPS header, sets a 60-second timeout and response-size limits, and returns generic status-based errors without exposing response bodies or the key. The coding provider selector is untouched; no fallback is attempted.
- The API key exists as a user-scoped Windows environment variable, and its value matches the current benchmark/build process environment. It was never printed, placed in settings, or sent to the frontend. The API endpoint identifies Gemini Developer API (not Vertex AI or Antigravity). Google's docs state API keys are associated with Google Cloud projects and inherit the project's quota/billing status; if billing is enabled, API usage is attributed to that project's Cloud Billing account. This environment did not reveal which project/account owns the key or whether billing is active, so the payer cannot be named from this check ([API key docs](https://ai.google.dev/gemini-api/docs/api-key), [billing docs](https://ai.google.dev/gemini-api/docs/billing)).
- The exact Rust backend route completed one synthetic request in 1,155 ms and returned a valid subject. Focused draft tests passed (4 passed; the live test is ignored by default and was run explicitly).
- Windows release v1.0.5 passed release source verification, the full Rust suite on retry (the first cold run tripped an unrelated scanner timing threshold; the isolated rerun and full release rerun passed), artifact version metadata checks, and an 8-second isolated launch smoke test. Build record was saved after source fingerprint and artifact hashes matched.
- Executable: `src-tauri/target/release/navis-fabrica.exe` (20,951,040 bytes; FileVersion/ProductVersion 1.0.5). Installer: `src-tauri/target/release/bundle/nsis/Navis-Fabrica_1.0.5_x64-setup.exe` (8,109,260 bytes; same verified version). No commit, push, installation, or app-window replacement was performed. Launch the new executable or installer for the trial; an already-running older app does not update itself.
## External evidence

- Google's official model page calls Gemini Flash-Lite its fastest/most cost-efficient model for low-latency, high-frequency tasks and documents structured output support: [Gemini models](https://ai.google.dev/gemini-api/docs/models), [Flash-Lite model details](https://ai.google.dev/gemini-api/docs/models/gemini-2.5-flash-lite).
- Ollama documents `keep_alive` and returns `load_duration`, `prompt_eval_duration`, and `eval_duration`, which were used for the local timing split: [Ollama chat API](https://docs.ollama.com/api/chat). Its FAQ documents checking loaded model size and whether it runs on CPU or GPU with `ollama ps`: [Ollama FAQ](https://docs.ollama.com/faq).
- OpenAI documents the app-server's persistent newline-delimited protocol and `initialize` → `thread/start` → `turn/start` sequence: [Codex app-server](https://developers.openai.com/siwc/token-sharing-open-source/codex-app-server).
- GitHub reports are user reports, not official performance guarantees: [#16675 reports 2–3s warm Codex CLI startup in versions 0.117–0.118](https://github.com/openai/codex/issues/16675); [#32526 reports first-turn delays with a cold model catalog and improvement with an explicit model](https://github.com/openai/codex/issues/32526); [#21796 reports prompt-cache affinity differences between fresh and forked app-server threads](https://github.com/openai/codex/issues/21796). These provide context, not measurements for this machine or proof that a persistent Fabrica process will improve inference latency.

## Artifacts

- `codex-results.json`: six app-server runs and structured outputs.
- `gemini-parity-results.json`: six direct API runs, first-text timings, and outputs.
- `ollama-results.json`: six local calls, load/prompt/generation timing split, and outputs.


