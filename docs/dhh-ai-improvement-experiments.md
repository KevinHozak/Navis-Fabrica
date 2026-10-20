# DHH interview: Fabrica improvement experiments

Reviewed 2026-10-18. Proposals only; no implementation phase selected. No API, schema, runtime, provider or issue-state changes.

## Interview observations

The conversation describes a focused mission for Omarchy, opinionated defaults and small personal tools. It also discusses independent review, requesting simpler solutions and asynchronous task delivery. Sources: [Omarchy](https://lexfridman.com/dhh-2-transcript/#building-omarchy-linux-distro), [harnesses](https://lexfridman.com/dhh-2-transcript/#best-ai-coding-harnesses), [simplification and voice](https://lexfridman.com/dhh-2-transcript/#voice-prompting-vs-typing). These are practitioner observations, not measured forecasts for Fabrica. The publisher warns of transcription errors. Historical model/product claims are not verified current capabilities.

## Our interpretation

Reduce the effort of finding decisions and evaluating outcomes before increasing agent concurrency. Evaluate existing controls first; additional functionality requires observed friction and a separately selected issue.

## Existing coverage and proposed experiments

| Idea | Existing coverage verified in source/docs | Proposed experiment |
| --- | --- | --- |
| Attention queue | `src/BatchRunnerPanel.tsx` computes attention state, shows reasons/checkpoints and provides explicit recovery controls; see [runner UI](runner-ui.md). | Use five synthetic jobs spanning running, blocked, needs-input, failed and verified. Measure decision/evidence lookup time. Test a manual digest first; propose grouping only if it reduces lookup effort without hiding recovery requirements. |
| Outcome receipt | Batch UI shows completion gates, checkpoints, reported cost/tokens and unresolved usage. [Verification](runner-verification.md) binds checks/reviews to snapshots and keeps manual acceptance explicit. | Assemble three manual summaries: outcome, criteria, checks, manual gaps, interventions, elapsed time and usage provenance. Require every claim to trace to evidence and preserve unknowns. Assess readability before adding a persisted format. |
| Simplification review | Independent review and bounded repair already exist in [verification](runner-verification.md); workflow evaluation already has [improvement records](workflow-improvements.md). | Apply a simplification question to three scoped completed candidates. Retain it if at least two become easier to maintain with requirements/checks preserved. Do not automatically add another paid review or weaken existing gates. |
| Curated defaults | [Product plan](product-plan.md) defines project-aware workflow recipes and useful unsupported-action explanations. This does not prove every stack/action is supported. | Walk through work, push and build on representative projects. Record choice points and prerequisite/scope confusion. Change presentation only where observed friction warrants it, preserving effect scope and capability checks. |
| Responsiveness | `src/App.tsx` records first-project and full-scan duration. [Architecture](architecture.md) has provisional performance targets. | Record five cold and five warm observations on a documented Windows setup from project selection to first useful result. Keep this distinct from scan timing. Report samples and median/range; identify the dominant delay before choosing an optimization. |

Inspection establishes source coverage, not fresh native UI or performance acceptance. [Issue status](../issues/status.md) retains FAB-S013.4 intervention accounting as partial and GP-109 manual acceptance as open. Extend that evidence rather than duplicate its work. [FAB-S001](../issues/FAB-S001.md) already requests runtime-bound model evaluation, latency and usage evidence; this proposal adds no shortlist.

## Evaluation and reusable prompt

Record ID/date, goal, input/source revision, runtime/model/settings if relevant, baseline comparability, preparation minutes, review/correction minutes, total elapsed time, interventions, errors, usefulness (1–5), checks, reported cost with source/time, unknown usage, and keep/revise/drop decision. Keep private run records outside Git. No savings claim without a comparable baseline.

> Review this result against its requirements. Identify unnecessary steps, abstractions, controls or duplication. Propose the smallest simplification, state what it preserves and identify verification needed. Do not expand scope, remove required checks or claim unexecuted checks passed.

Start with manual evaluations and synthetic/read-only evidence. No trials have run under this proposal. No schedules, paid calls, extra agents or remote actions are created. Any feature implementation requires a separately selected issue.
