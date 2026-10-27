# Bounded runner pilot measurement

Recorded 2026-10-26 (America/Chicago). Pilot: `fab-s013-pilot-v15r1`. Fabrica source: `4dfc72fe8dcb60985481a718d2d3c4ee73837162`; runner-core 0.26.0, built offline with locked dependencies. Runtime: Codex CLI 0.162.1, GPT-6 Luna Medium for separate implementation and independent-review turns.

## Result and acceptance

The newly authorized pilot completes FAB-S013's remaining measurement criterion. It records a declared baseline, cost coverage, elapsed time, an explicit failed product outcome, scoped intervention counts, preparation recoveries and retries. It does not repair the historical v14 intervention gap. A failed candidate is a valid measured outcome; it is not successful product delivery or unattended-run readiness.

One real GRIM-S011 implementation attempt ran in an isolated source copy at GP-Grimoire commit `ded8a7983932914da0d5db1234a50860bfc38e4a`. Its scope was the Python adapter phase only, with native/UI integration excluded. The six host-owned synthetic adapter checks passed. The independent reviewer rejected the candidate because helper stdout was written to a temporary file without an enforced streaming size limit; the size was checked only after process exit. A read-only source check confirmed that finding. The terminal job stays `failed` with `verification_failed`. The CLI's exit 0 means foreground supervision returned normally, not that the job passed.

GRIM-S011 remains open. No candidate was integrated, committed, pushed, deployed or substituted for primary code. Manual/native acceptance and broader runner readiness remain separate.

## Declared comparison

The deterministic baseline was measured and sealed before candidate admission, using the same source commit and the same six host-owned checks with the adapter absent. It is an unimplemented-feature baseline, not a competing-model benchmark.

| Measure | Declared baseline | Live candidate |
| --- | --- | --- |
| Contract tests | 3 passed, 3 failed | 6 passed, 0 failed |
| Provider execution | None | One implementation turn and one independent-review turn |
| Direct paid API spend | $0 | $0; subscription-only route |
| Host elapsed | 1.097960 seconds | 134.4345769 seconds |
| Implementation attempt span | Not applicable | 85.538 seconds, including supervised attempt overhead |
| Independent review | Not applicable | Failed; verified output-bound defect |
| Accepted product outcome | No | No |
| Human/external-agent interventions within the declared window | 0 / 0 | 0 / 0 |

The host span is wall time, not active human time or measured model inference time. Codex subscription tokens and cost remain unknown. The two unresolved reservations retain ceilings totaling 49,152 tokens and 420 seconds; those are holds, not consumption. No itemized subscription bill or efficiency/savings result is inferred from $0 direct API spend. Earlier pilot holds remain unchanged.

## Bounds, recovery and observer coverage

The admitted policy allowed one job attempt, one implementation turn (240 seconds), one independent-review turn (180 seconds), a 600-second engine window, concurrency one, zero paid API budget and zero authentication/network retries. Only two adapter/test files were eligible for candidate changes. Trusted checks used synthetic presets and a fake helper; they made no model call.

Preparation had two external-agent recovery actions: a sandbox metadata-probe cleanup failure recovered by narrow elevation, and correction of a Windows-default decoding mistake to explicit UTF-8. The latter produced a separately preserved pre-admission v15 refusal with zero jobs/reservations/turns; the corrected request used a new ID, fresh source copy and fresh state. Copy/line-ending verification steps belonged to that same preparation recovery. There was one pre-admission refusal, one subsequently admitted attempt, and no automatic or post-admission retry. No review repair round was started. These are runner attempt/turn counts; internal subscription HTTP request/retry counts are not observed.

The private observation log opens before foreground admission and closes after process exit. It captures all human/external-agent actions affecting this attempt through the owning task; planned observation/checks and autonomous provider tool actions are excluded from interventions. No intervention occurred inside that closed window. Preparation recoveries are counted separately. This scope does not assert observation of unrelated machine activity, and it cannot establish historical v14 counts. Logs have consecutive sequence numbers, unique intervention IDs and sealed hashes matching the host receipt.

All 51 prior pilot database hashes match their preflight values. The primary GP-Grimoire HEAD, clean status and issue hash are unchanged. Candidate changes are confined to the declared adapter/test paths. The admitted request, baseline, binaries, source/lockfile map, trusted checks, observer log, intervention log, review/check receipts and final measurements are hash-bound. Raw prompts, responses, tool arguments, secrets and campaign data are excluded from this published report.

## Reproduction and retained evidence

Private evidence root: `C:/Projects/Agents/private/fabrica/FAB-S013/pilot-v15r1/`.

- `declared-baseline.json` and `baseline-checks.txt`: before-admission baseline and six-check result.
- `admission-binding.json`, `request-normalized.json`, `build-source-binding.json`: immutable request/source/runtime/check bindings.
- `observer.jsonl`, `interventions.jsonl`, `host-receipt.json`: closed observation window and scoped counts.
- `measurement.json`, `check-and-review-evidence.json`, `final-seal.json`: full metadata reconciliation, failed review and unknown holds.
- `summarize.py`: read-only database/source reconciliation; writes metadata only.
- `../pilot-v15/`: preserved pre-admission refusal and diagnosis.

Re-read stored evidence for verification; do not rerun `run_once.py`. Its exclusive marker prevents duplicate invocation. No new inference, reset, hold release or overnight expansion is authorized by story closure. The failed adapter candidate needs a separately selected repair before product integration.
