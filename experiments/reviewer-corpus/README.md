# Verified reviewer corpus

This offline research harness extracts historical Rust source from this repository, compiles executable reproductions, verifies scoped clean controls, and compares two reviewer policies. It does not alter the application, enable a reviewer policy, or publish changes.

## Corpus and ground truth

The corpus contains four pairs from three historical fixes:

| Pair | Historical fix | Required behavior |
| --- | --- | --- |
| SVG literal references | `bc2bbbe` | Reject the specified spaced external `href` and CSS URL; preserve the two safe specimens. |
| Encoded SVG references | `bc2bbbe` | Reject the specified entity-encoded and aliased external references; preserve a safe path and an internal aliased reference. |
| Nested icon discovery | `56a372f` | Resolve the named frontend and project-mark fixture paths. Other supported locations remain permitted. |
| Oversized icon fallback | `ce37e47` | With only the three named files present, select the small store image over the stale web image. Preserve README-logo precedence. |

The encoded-reference pair is held out for this final comparison. Nested discovery was moved into the target/regression set after calibration exposed ambiguous wording. These are eight cases, **not eight independent bug families**. The two SVG pairs deliberately share source but have different specimen contracts.

“Clean” means all executable requirements of that case's explicit contract pass. It is not a claim that the whole historical module is defect-free. Original test modules are excluded from source artifacts. The SVG fixtures include only `safe_svg` and its supporting functions; lookup fixtures retain the production module, with unrelated SVG behavior explicitly outside their review contracts.

PNG fixtures use a PNG signature and distinct marker payloads because this adapter inspects signatures and returns encoded bytes. They do not establish image decoding or browser-rendering acceptance. Oversized fixtures retain the recognized PNG signature so size rejection cannot accidentally be explained by an invalid signature.

## Build and verify

Run from a checkout containing the referenced history. Choose a new private directory **outside this repository**; do not put state, prompts, model responses, logs or compiled fixtures in tracked files.

```powershell
python experiments/reviewer-corpus/corpus.py --repo . --state C:/Private/reviewer-corpus/run-001
python experiments/reviewer-corpus/corpus.py --repo . --state C:/Private/reviewer-corpus/run-001 --verify-only
python experiments/reviewer-corpus/check_integrity.py --repo . --state C:/Private/reviewer-corpus/run-001
```

Requirements: Python 3.11+, Git history, Rust/Cargo, and cached `base64`, `quick-xml`, and `tempfile` dependencies. Dependency resolution is offline. Cargo.lock, source artifacts, Rust harness, and test fixture hashes are frozen before compilation; subsequent builds use `--locked`. Rust compilation happens in the private state's standalone Cargo project, avoiding Tauri packaging and user settings.

Every source must pass its preservation checks. A buggy case must produce exactly its declared assertion failures and a clean case must pass every contract test. Compilation errors, zero matched tests, changed source, timeouts, or mismatched counts make the corpus **unverified**. Frozen inputs and existing manifests are never overwritten. Reverification uses the same private harness rather than silently regenerating it from edited tests.

## Reviewer comparison

```powershell
python experiments/reviewer-corpus/compare.py --repo . --state C:/Private/reviewer-corpus/run-001 --prepare-only
python experiments/reviewer-corpus/compare.py --repo . --state C:/Private/reviewer-corpus/run-001
```

The second command performs **32 Codex inference calls**: eight cases, two arms, two repetitions. Each uses GPT-6 Luna at medium reasoning, a fresh ephemeral read-only session, the production-equivalent JSON output schema, a 120-second wall timeout, and a common soft 600-word response limit. At most two calls run concurrently. This is not an enforced equal-token budget; report measured input, cache and output tokens separately.

Both arms share a baseline adapted from the current production prompt. Live Git/workspace obligations are replaced with a whole-artifact contract review; code, line numbers and exact requirements are supplied directly. The structured arm adds the frozen claim → evidence → verification instruction. Labels, pairing, revisions, test outcomes and held-out membership are withheld from reviewer prompts. This is an **adapted source-only prompt comparison**, not acceptance of the production reviewer tool loop. Reviewers may not use tools. Corpus labels are verified by the separate executable harness.

`plan.json` freezes the prompt hashes, model settings, task order and rubric before inference. Existing receipts are reused only for the same plan; an ambiguous partial attempt is refused rather than replayed. Raw outputs and metadata stay private. Empty, malformed, tool-using, fingerprint-mismatched or otherwise incomplete responses remain failed observations.

## Scoring

`metrics.json` initially reports disposition counts only. A human must validate each finding against the frozen source and executable contract before calling it a confirmed detection. Record supported defects, unsupported or withdrawn findings, clean-control false positives, missed defects and protocol validity separately. A rejected buggy case is not automatically a successful detection. A clean case blocked by a “withdrawn” finding is still a gate false positive.

Save human assessments privately with one entry per trial ID, the answer SHA-256, supported and unsupported finding indices, covered contract-test names, and adjudication notes. `score.py` verifies that every finding is classified and that the notes address the exact frozen response:

```powershell
python experiments/reviewer-corpus/score.py --state C:/Private/reviewer-corpus/run-001 --assessments C:/Private/reviewer-corpus/run-001/manual-assessments.json
```

This aggregates supplied human judgments; it cannot prove the judgments are correct. Preserve whether adjudication was independent/blinded and retain the source/test references supporting each decision. See [the dated results](results-2026-10-18.md) for the completed trial and its limitations.

No active workflow promotion follows automatically. The existing `scripts/workflow_evidence.py cycle` helper owns experiment snapshots and adoption decisions; these scripts only build fixtures and collect observations. Record any calibration exclusions and replace an exposed held-out case before a new comparison. Do not claim independent or statistically conclusive evidence from repeated runs of this small corpus.
