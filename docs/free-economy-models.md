# Free Economy coding models

## Current checkpoint — 2026-10-27

FAB-S001 remains open. The authorized nine-case trial matrix is complete, and none of the three candidates passed all cases. The Economy shortlist is empty. Catalog presence, a model's own claims, and passing offline policy tests do not establish coding quality or runtime compatibility.

Authenticated OpenRouter model and user-model metadata verified these bounded evaluation candidates with zero prompt/completion prices. Aider 0.86.2 is installed. Exact serving endpoint metadata was checked separately; none of these free endpoints was present in the authenticated ZDR inventory at observation time.

| Candidate ID | Serving endpoint | Catalog price | Trial status |
| --- | --- | --- | --- |
| poolside/laguna-s-2.1:free | poolside/fp4 | Zero input/output | Failed: 2/3 cases passed |
| nvidia/nemotron-3-ultra-550b-a55b:free | nvidia | Zero input/output | Failed: 0/3 cases passed |
| nvidia/nemotron-3.5-lightning:free | nvidia/nvfp4 | Zero input/output | Failed: 0/3 cases passed |

These IDs are a dated evaluation set, not a permanent or automatically approved catalog. Re-fetch authenticated user availability, model/endpoint pricing, capabilities and policy before admission. Missing, stale or changed evidence must refuse automatic selection. No production profile or running job was changed during metadata inspection.

## Runtime results — 2026-10-27

The actual Fabrica runner used Aider 0.86.2 and newly created synthetic Python fixtures. Each candidate received a small bug fix, an implementation task and a multi-file edit. Independent agent source review preceded isolated host checks; this was not human approval.

| Exact model ID | Task | Runtime | Host check / review | Requests | Total tokens | Latency (ms) |
| --- | --- | --- | --- | ---: | ---: | ---: |
| poolside/laguna-s-2.1:free | small_bugfix | completed | passed / passed | 1 | 2037 | 29131 |
| poolside/laguna-s-2.1:free | implementation | completed | passed / passed | 1 | 2227 | 38310 |
| poolside/laguna-s-2.1:free | multifile_edit | completed | failed / failed | 1 | 2318 | 43533 |
| nvidia/nemotron-3-ultra-550b-a55b:free | small_bugfix | failed | failed / failed | 1 | 2209 | 39256 |
| nvidia/nemotron-3-ultra-550b-a55b:free | implementation | failed | failed / failed | 1 | 2726 | 52520 |
| nvidia/nemotron-3-ultra-550b-a55b:free | multifile_edit | failed | failed / failed | 1 | 2519 | 40704 |
| nvidia/nemotron-3.5-lightning:free | small_bugfix | failed | failed / failed | 1 | 4348 | 58570 |
| nvidia/nemotron-3.5-lightning:free | implementation | failed | failed / failed | 1 | 4492 | 64703 |
| nvidia/nemotron-3.5-lightning:free | multifile_edit | failed | failed / failed | 1 | 9376 | 108657 |

Laguna's multi-file implementation removed a Unicode em dash before treating it as a word separator, producing the wrong slug. Each NVIDIA case returned one response with the selected model identity, then Aider exited with code 1 and left the seed files unchanged. The deeper NVIDIA failure cause is unknown because post-dispatch stderr was deliberately omitted from retained diagnostics. These results establish failure of this bounded runtime evaluation, not a general claim about model capability.

An earlier Laguna bug-fix admission stopped before process start and made zero inference requests because an inherited `.env` guard rejected the workspace. Its failed record was retained. The user separately authorized one fresh bug-fix trial; the table uses that fresh result. No other case was retried or switched to another model. A host-verifier destination collision on Laguna implementation was refused before execution; a separately logged fresh host check supplied its result without repeating inference.

All observed response identities matched the exact selected free IDs. The gateway applied zero-price ceilings and a $0 API budget; provider-reported dollar usage was unavailable, so actual billed dollars remain unknown. Total tokens include input and output and are separate from the 8,192-output-token cap. Original provider settings were byte-identical after the trials.

The sanitized matrix and per-case source/check/review hashes are retained with the reviewed evidence bundle. Re-evaluation requires an explicit bounded selection and fresh authenticated metadata/privacy checks. No failed model may become eligible by changing a display flag or a manual override.

## Privacy and trial authorization

The official provider policy table observed on 2026-10-27 lists Poolside as not training on prompts but retaining them for an unknown period, and NVIDIA as potentially training and retaining prompts for an unknown period. Endpoint responses omit separate training/retention/logging policy fields. ZDR membership is not inferred from a model family or a paid endpoint with a similar name.

The user explicitly authorized an exception only for newly created synthetic-code trials on these three endpoints despite unknown retention and possible NVIDIA training. Bounds: three candidates × three tasks; at most three inference requests per task, 8,192 output tokens per request, 120 seconds per task; zero paid API budget; no automatic retries or fallback. This exception does not authorize private repository content, production use, or changed privacy settings for existing jobs. Trial output and logs remain private and are reviewed as untrusted data. No key belongs in prompts, command arguments, settings or persisted telemetry.

## Production admission contract — 2026-10-28 repair

Explicit provider refresh stores normalized catalog and exact endpoint observations. Read-only preview and recommendation rehydrate eligibility from that snapshot, the current runtime identity and core-owned reviewed evidence; they do not probe providers or accept a frontend eligibility flag. Missing or stale observations require another explicit refresh.

The runner commits the selected model, serving endpoint and semantic evidence digest. Dispatch rebuilds admission from authenticated metadata and compares the pin before reserving or starting the provider process. The capped gateway repeats identity, zero-price, context/capability, endpoint and ZDR checks for each call. A caller cannot deserialize the opaque admission or reuse the synthetic training exception as production permission.

Reviewed endpoint policy records live in [the core policy registry](../runner-core/config/economy-endpoint-policies.json). Unknown training, retention or content-logging policy blocks production admission; the current three candidates have no compatible policy proof and all failed the full coding matrix. Local-only repository policy continues to exclude OpenRouter. Manual selectors preserve saved choices while blocking free, zero-priced or router identities that lack current eligibility.

Free requests use a separate call ledger and retain request, token and time caps. Enforced zero-price budget settlement is policy-derived; it is distinct from measured provider token usage and an invoice dollar receipt. Unknown usage keeps its reservation unresolved. No new live evaluation is part of this repair.

ZDR membership is checked through the [documented exact endpoint inventory](https://openrouter.ai/docs/guides/features/zdr). It does not substitute for reviewed endpoint policy. The current documentation also describes broker screening/legal-retention exceptions, so this feature does not promise absolute OpenRouter broker non-retention.

## Provider controls and limits

Pin the selected exact model and serving provider at admission and every call; reject random or automatic routers, fallback models and mismatched response identity. The gateway must enforce zero price ceilings, request/token/time caps and provider-only routing. Ordinary eligible strict-policy requests require compatible policy evidence and enforced no-collection/ZDR restrictions. The synthetic-only policy exception must remain distinct and auditable.

OpenRouter's dated published free-model limits are 20 requests per minute and either 50 or 1,000 per day, based on lifetime credit purchases; the documentation describes a rounding allowance near the purchase threshold. These are provider rules, not a measured remaining allowance. The deprecated key rate_limit object is not execution eligibility or quota evidence. Remaining quota stays unknown when current counters are absent. Rate limits, removed models, changed pricing, policy changes and unavailable evidence must stop or pause with an actionable reason, never silently move to paid inference or another model.

## Sources

- [Provider routing, fixed endpoints, fallback and maximum prices](https://openrouter.ai/docs/guides/routing/provider-selection)
- [Endpoint metadata schema](https://openrouter.ai/docs/api/api-reference/endpoints/list-endpoints)
- [Provider logging, retention and training policies](https://openrouter.ai/docs/guides/privacy/logging)
- [Zero data retention enforcement and endpoint inventory](https://openrouter.ai/docs/guides/features/zdr)
- [Free-model limits and key metadata](https://openrouter.ai/docs/api/reference/limits)

Original observation receipts and operational logs remain in caller-private evidence storage. Sanitized trial artifacts may be published after source and privacy review. Passing offline checks alone does not certify a model or complete FAB-S001.

## Low-cost paid candidate preparation — 2026-10-28

FAB-S001 now includes low-cost paid coding models. Selection should compare cost per successfully verified task, including failed attempts, token use, latency and review effort. Qwen3-Coder-Next, DeepSeek V4.1 Flash and GLM 5.3 Flash are research leads; none inherits qualification from a free variant or advertised benchmark.

The optional paid endpoint pin keeps ordinary paid billing and durable reservations. It binds a selected model, authenticated canonical identity and exact endpoint through the reviewed route commitment. Admission and each call recheck current endpoint identity, capability, context, prices, ZDR membership and dated reviewed policy. Requests restrict the endpoint, disable fallback and require the approved price and privacy ceilings. A paid pin cannot supply a free Economy admission proof or turn a paid receipt into zero-cost usage. Existing paid requests without a pin retain their prior behavior.

The Qwen pilot uses only synthetic fixtures and the approved aggregate USD ceiling. Public and authenticated metadata identify `qwen/qwen3-coder-next` at `parasail/bf16`; this is metadata and policy evidence, not tested coding readiness. Parasail's serverless terms limit content retention to generation and delivery, while allowing operational metadata. OpenRouter's ZDR exceptions still apply. No zero-logging claim covers operational metadata.

Optional endpoint fields remain distinguishable from observations. An omitted request fee is not recorded as an observed zero: a pinned paid request requires an enforced zero per-request-fee ceiling and rejects an explicit positive or malformed fee. A missing separate prompt limit stays unknown; the local input/output envelope must fit the observed total context, and any explicit prompt/output limits must also fit. These controls do not promise that an endpoint will accept every request.

Sources: [provider price ceilings](https://openrouter.ai/docs/guides/routing/provider-selection), [ZDR](https://openrouter.ai/docs/guides/features/zdr), [Parasail security and retention](https://docs.parasail.io/parasail-docs/security-and-account-management/overview), and [serverless terms](https://www.parasail.io/legal/terms-of-service).

## Qwen pilot attempt — 2026-10-28

The user approved an aggregate USD 0.10 ceiling for three synthetic tasks, with at most three Qwen calls per task. Three tasks were admitted, each with one attempt. All three stopped before Aider started because the batch passed the selected Codex reasoning effort to the Qwen runtime, which correctly rejected an effort not configured for that provider/model. The bounded source repair keeps the effort attached to its selected provider/model; it does not retry these tasks.

The durable paid-call ledger records zero calls, zero tokens, zero cost, and zero unresolved usage; reservations were released before process start. Qwen was **not evaluated**: these results are neither a 0/3 score nor a model failure. This stopped attempt does not qualify any model. A fresh pilot requires new explicit authorization because the approved pilot prohibited task retries. The original admission-staleness refusal remains separately preserved in private evidence.

## Qwen pilot second attempt — 2026-10-28

A fresh approved run made one Qwen call for the small-bugfix task: 2,322 tokens and $0.000457 accounted cost. Its two host checks failed because the output changed case and did not collapse repeated spaces. The implementation prompt contained only generic acceptance text and did not include the issue requirements, so this result is inconclusive about Qwen's coding capability and is not a valid model-quality score. No verified coding task succeeded; cost per verified success is undefined.

The other two tasks remained at `reconciliation_required` and were blocked before inference because the dispatch guard could not verify source-audited Aider 0.86.2. A later metadata-only probe observed Aider 0.86.2 (exit 0, 7.249 seconds), but the earlier guard did not retain enough detail to identify why verification failed. All three authorized attempts are consumed; no whole-task retries are authorized. The settled ledger has no unresolved usage, and the original attempt evidence remains preserved. No model is qualified, and further trials require new explicit authorization.

## Qwen pilot corrected run — 2026-10-28

The corrected run passed the full stored issue snapshot and complete task acceptance requirements to Aider. The version guard now allows up to 30 seconds for isolated startup, capped by the same task's remaining 120-second deadline, and still requires exact Aider 0.86.2 output. All three Aider starts passed the guard; no fallback, retry, or output repair was used.

| Synthetic task | Calls | Tokens | Accounted API cost | Runner-accounted execution (not API latency) | Behavioral result |
| --- | ---: | ---: | ---: | ---: | --- |
| Small bugfix | 1 | 2,265 | $0.000315 | 93 s | Passed both host checks |
| Implementation | 1 | 2,448 | $0.000426 | 23 s | Failed: `parse_positive_limit("0")` returned `0` instead of raising `ValueError` |
| Multifile edit | 1 | 2,651 | $0.000450 | 25 s | Failed: em-dash handling produced `42cafe` instead of `42-cafe` |
| **Total** | **3** | **7,364** | **$0.001191** | **141 s** | **1 of 3 tasks passed** |

The batch wall duration was 196.861 seconds. The table reports runner-accounted task execution time; API latency was not retained.

The corrected run's accounted API cost per verified task was $0.001191. Including the earlier inconclusive $0.000457 call, cumulative accounted API cost per verified task is $0.001648. Subscription review cost was not measured. The earlier call is excluded from model-quality scoring because its prompt omitted issue requirements.

The corrected source change was published as `0e3bdbf` (version 1.0.24); all seven declared checks passed, including 345 unit tests, 7 integration tests, 7 helper tests, and the exact-version source probe. Qwen has one valid verified task out of three and is **not qualified**. The pilot is complete; no further live calls, retries, fallbacks, or output repairs are authorized by this run.

## DeepSeek V4.1 Flash pilot — 2026-10-28

The user selected `deepseek/deepseek-v4.1-flash` next under the same cumulative USD 0.10 cap. The corrected three-task pilot pinned canonical model `deepseek/deepseek-v4.1-flash-20260910` and `parasail/fp8`, with at most two calls per task, 16,384 input and 8,192 output tokens per call, and 120 seconds of provider execution per task. Full issue snapshots and exact acceptance requirements were present. No fallback or whole-task retry ran.

The runner now separates authenticated model-catalog pricing from the selected endpoint quote. Parasail's reviewed rate was USD 0.30/M input and USD 1.20/M output; reservations and request price ceilings use that endpoint rate. The cheaper catalog minimum remains separately bound and cannot authorize another endpoint. A catalog-price change stopped the first metadata-only preparation before any request, batch or inference; its evidence was preserved, and a fresh preview reused the unchanged synthetic fixtures. No omitted request fee is treated as observed zero: `max_price.request=0` remains an enforced ceiling.

| Synthetic task | Calls | Tokens | Accounted API cost | Runner-accounted execution | Outcome |
| --- | ---: | ---: | ---: | ---: | --- |
| Small bugfix | 1 | 2,575 | $0.000993 | 15 s | Provider process failed before edits |
| Implementation | 1 | 3,511 | $0.002070 | 42 s | Provider process failed before edits |
| Multifile edit | 1 | 5,915 | $0.004458 | 77 s | Provider process failed before edits |
| **Total** | **3** | **12,001** | **$0.007521** | **134 s** | **Inconclusive for coding quality** |

All three attempts remain `reconciliation_required`, and the batch is `waiting`; the foreground host returned after 160.737 seconds. Retained stderr tails show Aider/Rich crashing with a Windows cp1252 `UnicodeEncodeError` while printing U+25BA (`►`). Earlier response/error output was not fully retained, so another preceding error cannot be ruled out. The four seed source files remained unchanged in each worktree. No candidate behavior checks were executed because no implementation was applied; this is not a valid 0/3 model-quality score.

All three calls and parent reservations settled, with zero unresolved billing or retained budget holds. Including previous Qwen usage, cumulative accounted API usage is seven calls, 21,687 tokens and **USD 0.009169**, leaving USD 0.090831 under the cap. Cost per verified successful DeepSeek task is undefined. API latency and subscription review dollars were not measured. No model is qualified by this run.

Endpoint quote support was delivered in `8076444` (v1.0.25). Verification passed: 349 active unit tests, seven integration tests, seven prompt-helper tests, native check, CLI/worker build, release source contract and whitespace. Independent source audit found no actionable issues. One new test fixture initially used the wrong catalog output price; its failed run is preserved, and the corrected suite passed. No app installer was built. Live desktop settings and all six historical preservation hashes remain unchanged.

FAB-S001 remains open. The next runtime repair is to make the isolated Aider output path Unicode-safe and verify it locally before any separately bounded fresh pilot. These consumed attempts were not recovered, edited or retried.
