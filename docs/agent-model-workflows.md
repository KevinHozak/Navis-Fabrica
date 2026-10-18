# Shared agent model workflows

Fabrica owns the reusable helper and configurable model routes. Agent skill instructions are maintained separately by the caller. User configuration and runtime data belong outside this public checkout. No key is stored in these files. This helper performs text inference only; coding jobs retain the existing Rust runner, credential broker and paid gateway.

## Run a bounded task

Requires Python 3.10+ and the standard library. Install Ollama separately for local inference. Point `FABRICA_AGENT_CONFIG` at a private copy of `config/agent-settings.example.json`; set its provider profile ID and settings path. The profile's secretRef resolves through Windows Credential Manager using the same contract as the Rust broker. On other platforms supply `OPENROUTER_API_KEY` through your secret manager/process environment. Never put a key in a CLI argument, tracked settings or prompt.

```powershell
python scripts/model_helper.py route --task coding-advice
'Explain this synthetic function: def add(a,b): return a+b' | python scripts/model_helper.py run --task coding-advice
'Reword: The draft is ready for review.' | python scripts/model_helper.py run --task draft-local --private
python scripts/model_helper.py usage
```

Routes are editable JSON, not compiled model rankings. Defaults use Qwen3 Coder Next for coding advice/extraction, Gemini 3.5 Flash Lite for documents, Sonnet 4.6 for premium review, Gemma3 4b for local drafts and experimental Qwen2.5 Coder 7b for small local code questions. Haiku/Sonnet 5.5 are not silently substituted. Private cloud routes refuse; choose a suitable local task explicitly. Local requests have 2K context and 256 output-token limits; large prompts refuse rather than truncate. Deterministic facts and whole-repository coding jobs are outside this helper's purpose.

Each cloud call refreshes public model pricing, reserves a conservative input envelope plus maximum output in a transactional SQLite ledger, disables redirects/proxies/retries and pins model/provider price ceilings. Requests contain one text message, no tools, attachments, plugins or cache-control directives; modality/search prices in catalog metadata do not enable those services. Unsupported fees/tiered pricing refuse. The byte-sized prompt plus margin must fit the input envelope. `data_collection=deny`, `require_parameters=true` and `allow_fallbacks=false` may make a route unavailable; report this rather than silently relaxing it.

The ledger stores only call ID, local calendar day, provider/model/task label, timestamp, tokens, cost, latency, state and reservation. It stores no prompt, response, key, credential target or repository content. Responses go to stdout for the invoking agent. Keep terminal/transcript capture private. A crash, timeout or missing usage leaves a reservation; do not count it as zero. Overruns stop further dispatch pending operator review. Do not edit unresolved rows to free budget without provider evidence. Known costs round up to microdollars for conservative admission; returned provider cost remains unrounded in the call result. At 80% committed/reserved budget the result carries a warning. Exceeding daily or per-call ceiling refuses before inference. A user-approved increase is a private setting change for that task/day; restore the ordinary limit afterward. The day uses the host's local timezone; share one ledger across concurrent helpers on that host.

The helper ledger is a distinct scope from `runner-core`'s existing coding-job paid-call ledger. Do not claim its daily limit caps Aider/runner jobs or unrelated API clients. Runner jobs must retain their own enforced budgets. A future unified admission/reporting integration should reconcile both stores before making an app-wide daily-cap claim. Codex subscription allowance and local energy/hardware cost are separate, unmeasured quantities here. Local API cost is zero; unreported local tokens remain unknown.

## Skill discovery and sharing

Local skills should call the shared CLI rather than copy its implementation. Keep instruction packages in the caller-owned skill repository and register them using the agent's supported discovery mechanism. Configure paths outside the public checkout; preserve existing validation and side-effect boundaries. A skill is instructions, not an executable. Fabrica does not distribute personal skill instructions or private settings. Reusable code from such skills can live in `scripts/` with a documented CLI; caller-owned skills reference that interface.

## Verification

Run `python -m unittest discover -s scripts/tests -p "test_model_helper.py"`. Tests use temporary external ledgers, mocked providers and synthetic prompts; they spend no money. Live provider smoke tests are separately recorded in private state and do not establish agent coding readiness. The 2026-10-17 comparison established only two small text cases per cloud model; Sonnet's strict expression-field mismatch was a formatting difference, not a wrong repair.

On 2026-10-17, all 18 isolated tests passed, skill frontmatter validation passed, and live smoke requests completed through Qwen extraction, Gemini document summary, Sonnet review and private Gemma drafting. No coding-job or full-project acceptance claim follows from those checks. Credential-value and whitespace checks passed. Detailed usage measurements and receipts remain in caller-private state; no prompts, responses, credential references or usage databases are distributed here.

API contracts: [OpenRouter provider selection and price ceilings](https://openrouter.ai/docs/guides/routing/provider-selection), [OpenRouter API](https://openrouter.ai/docs/api/reference/overview), and [Ollama generate](https://docs.ollama.com/api/generate). Provider metadata is refreshed on cloud admission rather than treating this dated evidence as a permanent price or quality guarantee.
