# Current integration checkpoint — 0.26.0

FAB-S011 and FAB-S012 accepted within the documented managed-host scope. V9 supplemental immutable external-host samples prove the same worker identity progressed across the completed requesting Codex turn (59 samples, heartbeats 53 to 113); the original observer protocol remains failed and its gate closed. Fresh v10 closed the exact Fabrica process, resumed the same paused batch using current 0.26.0 debug test CLI/worker, and reached verified at attempt 1/1 with one implementation start/completion and unchanged deadline. Reopened native UI read the same verified job without another submission. No paid API calls occurred in these synthetic fixtures; two unknown synthetic usage reservations remain unresolved. Historical pinned 0.23.1 native/lifetime observations are linked only through canonical equality of the relevant hosting/worker functions, not whole-binary identity. Current integrated checks pass: 295 core library tests, seven CLI/integration cases, native tests, frontend build and both Rust formatting checks. Auto Router medium selected GLM for the final acceptance review; root verified the evidence and corrected its wording: cross-turn samples were between turns, not during the later turn, and the successful paused v10 job began at attempt zero. Managed CLI host requires its terminal/session to remain open; no terminal-close survival, automatic service startup or physical reboot/power-loss guarantee is claimed. Existing cold-host reconstruction tests cover restart recovery. GP-109 manual acceptance and FAB-S013.4 intervention accounting remain open.

Local evidence is retained outside Git. The historical sections below describe earlier checkpoints and do not override this current status. No 0.26.0 packaged release or installer is claimed by the debug resume test.

---

# Shared runner CLI

`runner-core/` supplies the headless `fabrica-runner` CLI, detached `fabrica-worker`, and Fabrica's typed `runner_core_execute` bridge. They use protocol version 1 and the same explicit runner data folder. One CLI invocation returns one JSON `CoreResponse`: `protocolVersion`, `ok`, `exitCode`, `result`, and `error`. This is not a persistent JSON-lines session.

## Build and inspect local issues

With Cargo's default target folder:

```powershell
$repo = 'C:\Projects\Navis-Fabrica'
cargo build --locked --release --manifest-path "$repo\runner-core\Cargo.toml" --bins
$runner = "$repo\runner-core\target\release\fabrica-runner.exe"
$worker = "$repo\runner-core\target\release\fabrica-worker.exe"
& $runner --help
& $runner capabilities
& $runner issues-page --project 'C:\Projects\GP-Grimoire' --config 'C:\Projects\GP-Grimoire\runner.json' --offset 0 --limit 100
& $runner issues-show --project 'C:\Projects\GP-Grimoire' --config 'C:\Projects\GP-Grimoire\runner.json' --id GP-109
```

Issue config is UTF-8 JSON:

```json
{"schemaVersion":1,"issueScope":"GP","issuesDir":"issues"}
```

`issues-page` returns at most 100 body-free summaries plus `total`, `offset`, and `limit`. The dependency index is validated with actual-read limits of 5,000 issue files, 2 MiB per file and 32 MiB in total. `issues-show` returns the full selected record, its revision and exact `rawMarkdown`; use those exact bytes for a job snapshot. Legacy `issues-list` remains available for compatibility, but new queue views should use pages. Read-only issue commands do not create the allocator journal.

`issues-create` and `issues-update` read one `IssueRecord` JSON object (`frontmatter`, `body`) from stdin. Updates and deletes require `--expected-revision`. Create allocates a durable local E ID for epics and S ID for stories (all other types), using separate per-project counters; imported numeric and legacy L IDs remain unchanged. Markdown stays authoritative. See [local issue format](local-issues.md) for supported frontmatter, relationships, transitions and ID allocation.

`doctor`, `issues-validate`, and `plan --id ID` inspect the backlog. The single-issue `plan` command reports dependency readiness and no execution; it is different from a runnable batch preview. `provider-settings --settings PATH` reads the shared versioned provider configuration with secret references only. `providers-doctor --settings PATH` checks configured runtimes; network probes require `--live-probe`. Neither provider inspection command dispatches coding work.

`provider-observations-refresh --settings PATH --data-root PATH [--live-probe]` explicitly refreshes the bounded local routing snapshot. It does not run model inference. The optional `--live-probe` permits an OpenRouter catalog/authentication network probe; otherwise each adapter uses its documented local or model-free metadata checks. See [runner routing](runner-routing.md) for how previews consume the saved snapshot.

## Preview, submit and reconnect

Codex prepares a typed `BatchRequest` plan containing selected issue snapshots and canonical `issueSources` paths, explicit task/provider/model assignments and resource limits, declared checks, a distinct reviewer, a verified base commit, isolated workspace/build folders, and the permitted local effects. Batch requests and runtime requests are bounded to 1 MiB at the CLI input boundary. A missing prerequisite needs explicit closed-external evidence or a verified predecessor job; absence does not count as completion.

Use one state folder for both CLI and Fabrica. Copy the default shown in Fabrica's **Runner batches** screen, or explicitly select the same folder in both clients. Preview is storage-free and does not contact a model. It binds the normalized request, authoritative Markdown bytes, provider settings and worker identity. Submit requires its fingerprint and launches detached workers, then returns.

```powershell
$state = 'C:\RunnerState\Fabrica'
$plan = 'C:\RunnerPlans\selected-batch.json'
$preview = & $runner batch-preview --data-root $state --worker-executable $worker --batch-request $plan | ConvertFrom-Json
if (-not $preview.ok) { throw $preview.error.message }
$confirmed = 'C:\RunnerPlans\confirmed-batch.json'
$json = $preview.result.value.request | ConvertTo-Json -Depth 100
[IO.File]::WriteAllText($confirmed, $json, [Text.UTF8Encoding]::new($false))
$fingerprint = $preview.result.value.fingerprint
& $runner submit --data-root $state --worker-executable $worker --batch-request $confirmed --preview-fingerprint $fingerprint
& $runner status --data-root $state --batch-id selected-batch
& $runner events --data-root $state --batch-id selected-batch --cursor 0 --limit 100
```

Use the actual `batchId` from the preview. Results for batch operations are tagged `{kind,value}` inside `result`. Event pages include `nextCursor` and `hasMore`; keep the returned cursor. A changed request, source, settings or worker requires a fresh preview. Reopening a client or reading status does not submit another implementation attempt.

## Explicit foreground hosting

Some Windows hosts place commands in a Job Object that forbids detached child processes. In that environment, `submit` fails with `WORKER_LAUNCH_FAILED`; admission may already have persisted the batch. Read its status before doing anything else. The UI reconnects to that persisted batch only for this typed launch failure. An unrelated ID conflict does not attach another batch.

Choose `run-batch` explicitly to keep workers under a long-lived CLI host:

```powershell
& $runner run-batch --data-root $state --worker-executable $worker --batch-request $confirmed --preview-fingerprint $fingerprint
```

This command remains active until durable terminal or control state. Keep its process/session alive. Closing the host can interrupt workers and require reconciliation. It never silently falls back to detached execution. Status, events and controls can be read from another CLI or Fabrica using the same state folder. Reconnecting with the identical admitted request preserves attempts, budgets and the absolute deadline; jobs needing input or ambiguous recovery require explicit resolution first. Terminal batches cannot restart through this command.

Detached execution still requires an OS host that permits breakaway. The synthetic hosted acceptance receipt is `C:\Projects\Temp\Agents\local-issue-pilot\unattended fixture 9wfu38b0\evidence.json`; it proves hosted completion, not detached lifetime under every Windows host.

## Controls and recovery

```powershell
& $runner pause --data-root $state --batch-id selected-batch
& $runner resume --data-root $state --batch-id selected-batch --worker-executable $worker
& $runner stop --data-root $state --batch-id selected-batch
& $runner cancel --data-root $state --batch-id selected-batch
& $runner reconcile --data-root $state --batch-id selected-batch
@'
{"jobId":123,"decision":"safe_to_retry","evidence":"Inspected the isolated worktree and reconciled the interrupted effects.","processConfirmedStopped":true,"effectsReconciled":true}
'@ | & $runner recover --data-root $state --batch-id selected-batch
```

Pause prevents new claims while current work reaches its safe terminal checkpoint. Resume reattaches a persisted running batch idempotently, checks the admitted settings and issue sources, and replenishes only missing worker capacity. Stop is graceful; cancel requests active cancellation and targets only recorded matching process identities. Reconcile inspects expired owned work and reports ambiguity; it does not automatically retry or certify workspace effects as safe. `recover` requires a typed per-job decision and evidence. `safe_to_retry` additionally requires both confirmation fields, current batch membership, exhausted worker/runtime/reviewer process identities, and no interrupted check lacking an inspectable process identity; attempt and budget limits remain enforced. An ambiguous or live process blocks every recovery action. Terminal batches cannot be silently restarted. No scheduler is installed or enabled automatically.

Unknown billing retains its reserved token/cost/time envelope after the process finishes. It frees process concurrency for subsequent review, but never becomes a measured zero. Source changes and lost verification ownership cannot pass the completion gate. Declared checks and a separate read-only reviewer must verify the exact source snapshot; manual acceptance and ambiguous interrupted effects remain blocking.

## Current runtime support and limits

`runtime-run --settings PATH --project PATH` reads one explicit `RuntimeRequest` from stdin for a foreground run; durable issue work should use batch submission. Codex uses app-server stdio with typed approval/input events. Local Ollama and configured Antigravity adapters have fixture coverage. Independent unattended review currently requires the supported read-only Codex adapter. Raw external adapters do not gain stronger pre-effect ownership guarantees merely by being configured.

Paid OpenRouter execution uses the capped loopback gateway, source-audited Aider 0.86.2 guard, and authenticated pricing provenance. Focused validations cover 12 OpenRouter-provenance tests, 32 runtime unit tests, and one installed-Aider production guard probe; the earlier full 291-core/seven-integration gate predates the latest provenance source change. The separate CLI/worker release is CLI-only; desktop and installer were not rebuilt. The bounded GP-109 v14 run recorded one implementation dispatch, 66,084 tokens, $0.220320 settled OpenRouter cost, and four passing framework checks. Its valid independent review failed because the supplied review context lacked the required v13 candidate hashes; manual acceptance remains incomplete and the job is not accepted. The policy envelope was at most four calls/$13 within the $20 provider-key ceiling, and the dedicated child-call ledger plus build-bound gateway source prove one completed OpenRouter call. Original Luna subscription usage and total interventions remain unknown. The latest key-usage observation ($0.63376093 at 2026-10-17 09:46 UTC) is not fresh for a later admission. See runner-readiness.md and the linked v14 measurement record for the bounded result, supplements, and comparison limits; any new paid dispatch requires fresh provider metadata and its own reviewed preview.

Exit codes: `0` success, `2` invalid input/configuration, `3` missing record, `4` revision conflict, `5` unsupported protocol/capability/schema, `6` I/O/store failure, `7` issue validation/duplicate failure. Inspect the structured error code and message before retrying. See [supervisor behavior](runner-supervisor.md), [verification](runner-verification.md), [routing and budgets](runner-routing.md), and [Fabrica controls](runner-ui.md).

## Build output isolation

The backend supplies each owned workspace's build directory as `CARGO_TARGET_DIR` to implementation, review and declared-check subprocesses. Frontend requests cannot provide an arbitrary environment map. This isolates Cargo's default output convention; it is not a filesystem sandbox against commands that explicitly choose other paths.
