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
