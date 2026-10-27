# Shared agent model workflows

Fabrica owns the reusable helper and configurable model routes. Agent skill instructions are maintained separately by the caller. User configuration and runtime data belong outside this public checkout. No key is stored in these files. This helper performs text inference only; coding jobs retain the existing Rust runner, credential broker and paid gateway.

## Codex coding executor

Use **GPT-6 Luna** (`gpt-6-luna`) with **medium** reasoning for all local coding tasks. This is the user's coding executor preference; apply it through the host chat or Codex runtime's supported model controls. If the current executor setting cannot be inspected or changed, report that limitation instead of claiming the preference is active.

This preference is separate from the optional OpenRouter and Ollama text-helper routes below. Keep their configured models and provider settings unchanged. Do not add an optional helper review solely for extra confidence on a simple coding task.

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

## Explicit Markdown prompt presets (v1)

Select one file; the helper never scans directories or automatically runs discovered presets. Synthetic examples live in `config/prompt-presets/`: summarize, extract-actions and draft-local. Keep personal presets outside this public checkout, for example `C:/Projects/Agents/private/fabrica/prompt-presets/`. Presets are trusted instructions: review files before selecting them.

```powershell
python scripts/model_helper.py inspect --preset config/prompt-presets/summarize.md
'Synthetic meeting notes: Ada will review the draft Friday.' | python scripts/model_helper.py run --preset config/prompt-presets/extract-actions.md
'Synthetic draft for review.' | python scripts/model_helper.py run --preset config/prompt-presets/draft-local.md --private
```

A preset is UTF-8 (optional BOM), at most 16 KiB, with an opening and closing `---` line, followed by a nonempty Markdown instruction body. The frontmatter subset is deliberately smaller than YAML: one `key: value` per line, no quotes, comments, blank lines, nesting, lists, aliases or duplicate keys. Required keys are `schema_version: 1`, `id` (stable identity) and `task` (existing route name). IDs and tasks match `[a-z][a-z0-9-]{0,63}`. The only optional key is `private: true` or `private: false` (default false). Unknown versions, unknown keys/routes, malformed values and conflicting `--task` refuse before inference.

Route policy remains authoritative for provider, model, input/output bounds and timeout. Presets cannot override those settings, URLs, credentials, budgets or executable commands. `--task` may only agree with the preset task; CLI `--private` and preset `private: true` combine with logical OR, so neither can weaken the other. Privacy refusal happens during inspection too. `inspect` requires no private settings, credentials, stdin, network or ledger and shows only identity, effective task/privacy and allowlisted route settings; it omits instructions and file paths. `route --preset` also resolves without inference. `usage` rejects presets. Existing `run --task ...` raw-stdin behavior is unchanged.

With a preset, stdin is document data, serialized as one JSON string after the instructions. No placeholders, includes, environment expansion, code execution or automatic file reads occur inside the body or document. Source stdin remains bounded to 64 KiB; the complete rendered prompt, including instructions, JSON escaping and the existing margin, must fit the route input envelope. Empty documents refuse. This structural boundary helps distinguish data but cannot guarantee resistance to model prompt injection; treat model output as untrusted and review it before use. The helper has no tool-execution capability.

Presets use the same admission, credential, reservation, settlement and unknown-usage behavior as raw prompts. The ledger records the existing task route, never preset identity/path/body, rendered prompt, document or response. Output still goes only to stdout; keep caller transcript capture private. Offline tests cover preset loading, precedence, inspection, refusal, payload boundaries and retained cloud holds without paid requests.

## Select document context

For structured context, explicitly supply `--document-mode full|selection|none` and either `--instruction` or `--preset`. Stdin supplies the document; the instruction is separate. Without these new options, raw-stdin and preset invocation retain their existing behavior. Use non-sensitive CLI instruction text because command arguments may be visible to other local processes; private instructions can live in a private preset file.

```powershell
'Left Selected Right' | python scripts/model_helper.py context --task draft-local --instruction 'Summarize.' --document-mode selection --start 5 --end 13
'Left Selected Right' | python scripts/model_helper.py run --task draft-local --instruction 'Summarize.' --document-mode selection --start 5 --end 13 --private
'Synthetic document' | python scripts/model_helper.py context --preset config/prompt-presets/summarize.md --document-mode full
'' | python scripts/model_helper.py context --task draft-local --instruction 'Draft a synthetic greeting.' --document-mode none
```

`full` includes the supplied document, including an explicitly empty document. `selection` requires both offsets with `0 <= start < end <= document length`; invalid, missing or empty ranges refuse, never fall back to full. Offsets are zero-based Unicode code-point indexes with an exclusive end, after UTF-8 decoding and removal of an optional initial BOM. Emoji usually count as one code point; combining marks and multi-code-point emoji count separately. These are not UTF-16 offsets, byte offsets or grapheme indexes. Line endings are not normalized: CRLF counts as two code points. Offsets are rejected in other modes. `none` sends only the instruction and excludes all document content; an empty stdin is sufficient. The 64 KiB source-stdin cap applies to every mode, even when part or all is excluded.

The `context` command performs local inspection only: no inference, credential resolution, ledger access, provider discovery or network. It reports selected character/UTF-8 byte counts, excluded half-open ranges, effective route/timeout, composed UTF-8 size, the 512-byte framing margin, conservative input estimate, input envelope and reserved output-token limit. Default output omits instruction and document content. `--preview` is accepted only with `context` and adds the selected document transiently to stdout (null for none); it is never persisted by this helper. Keep any caller terminal capture private.

Inspection exits 0 when the request fits the configured envelope, or 2 on invalid options, privacy refusal or oversize. An oversized valid request reports `fits_input_envelope: false` and a fixed refusal reason, without truncation or content. A byte-plus-margin estimate is not a tokenizer measurement or proof of model-advertised capacity. For current Ollama routes, the legacy input envelope also configures `num_ctx`; structured requests additionally reserve `max_output_tokens` within that configured capacity. Cloud requests reserve output separately, and actual execution still checks the fresh catalog context capacity using the existing input-envelope-plus-output rule. Offline inspection cannot certify catalog capacity, credentials, budget availability or provider acceptance. Separate local context configuration belongs to FAB-S017.

Structured `run` uses the same composed request as inspection and refuses a failed fit before inference; normal privacy, credential, quote and reservation controls still apply. Preset privacy and CLI privacy combine as before. Selected text is JSON-quoted data; none mode has no document framing. Instructions in document text never enable tools or change route policy. No wikilink expansion, includes, recursive file access, supplemental context or URL retrieval is performed. There is no automatic omission or truncation. Prompt injection remains a model limitation; review returned text. Excluded content is absent from provider payloads, and neither selection text nor context reports are stored in the usage ledger.
