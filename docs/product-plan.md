# Navis-Fabrica product plan

Planning date: 2026-10-13. All features below are proposals. Initial delivery is structure only, per the user's clarified scope.

## Purpose

Give Kevin a fast, calm cockpit for every code project under a configured root, initially `C:\Projects`. See repository state, choose work, launch a familiar workflow, and follow its progress without managing multiple terminals by hand.

## Experience

Use Navis-Protocol as a visual reference: charcoal canvas, subtle glass panels, sky-blue accents, compact readable status indicators, and clear buttons. Reuse design language rather than importing its backend or personal Vault data. Keep version information in the window title. Use local/system fonts, keyboard navigation, visible focus, semantic controls, sufficient contrast, and reduced-motion support.

Proposed navigation: Projects, Runs, Providers, Settings. The Projects view contains search/filter controls and repo cards or a compact list. Selecting a project opens details for branch, dirty files, latest commit, upstream divergence, linked GitHub activity, and available actions. Distinguish local freshness from remote freshness. No fabricated counts.

The run drawer shows queued/running/needs-input/completed/failed/cancelled/interrupted states, streamed output, changed files, relevant checks, and artifact links. Buttons remain understandable while work runs; scanning and process output never block rendering.

## Workflow buttons

| Action | Inputs | Expected outcome |
| --- | --- | --- |
| Work on issue / dev-work | Project, issue URL/number, agent, model, pace | Scoped implementation, checks, commit/push/PR when authorized by workflow |
| Push / dev-push | Project, selected changes, agent or declared script | Intentional staging, checks, commit, push, PR |
| Build / dev-build | Project, detected stack/build profile | Release artifact by default, verified version and output path |
| Audit (later) | Project or PR/issue scope | Evidence-backed findings and verification limits |

Skills are instructions, not executable programs. Registry entries declare whether an action invokes a compatible agent with a skill or an equivalent versioned script. Build actions are stack-specific: Tauri, mobile, and other repositories must not all invoke the Tauri skill. Unsupported actions show a useful reason. Execution scope includes allowed writes and remote actions; explicit button intent and saved policy avoid repetitive approvals. Destructive scope expansion always stops for review.

## AI and pace controls

Separate provider, model, reasoning effort, and agent runtime. Codex and Antigravity supply coding-agent runtimes; Ollama supplies inference and needs a compatible tool-execution harness before it can edit code or run commands. The provider screen shows installation, auth/connectivity state, capabilities, available configured models, and tested versions. Never promise every provider supports every setting.

Manual selection ships first. Later, Auto recommends or chooses among explicitly enabled models using task complexity, required tools/context, observed latency, and fresh usage signals. Modes: Fast (latency preference), Balanced, Thorough (quality preference), and Economy (conserve paid usage). A separate desired completion time is a preference, never a guarantee; quality and required checks remain visible.

Routing flow: determine requirements -> reject ineligible models -> honor manual override -> apply pace/quality/budget rules -> show selection reason -> start at a safe boundary. Use deterministic configurable rules initially, rather than another paid classifier. Usage data includes source, window, units, timestamp, and reset time. Unknown/stale quota is unknown, not zero or unlimited. Subscription allowance, API billing, context limits, and local GPU capacity are separate signals. Live quota access across providers remains an integration question.

Do not silently switch a running task between providers. A supported fallback resumes at a checkpoint with a bounded handoff (goal, scope, branch, diff, checks, decisions, pending actions), preserving configured local-only/privacy policy. Never repeat commit/push/PR side effects. Show why a switch was proposed and retain audit history.

## Milestones

1. Read-only cockpit: foundation UX, saved root, reliable discovery/Git facts, optional GitHub summaries.
2. Useful manual workflows: safe run manager, declared action registry, tested Codex adapter, manual provider/model selection, verified artifacts.
3. Provider breadth: Antigravity capability validation, Ollama with an explicit agent harness, portable handoff.
4. Explainable automation: measured usage/latency, pace policies, deterministic routing, safe fallback.
5. Windows release: installer, accessibility, responsive UI under large roots, recovery, regression coverage.

All roadmap items start in Backlog. Foundation-P1 is the suggested first implementation task; nothing is moved to Ready without a prioritization request.

## Decisions and open questions

- Standalone Tauri 2/Rust application, React/TypeScript with Vite; no always-running Python or Next server is required.
- Local application settings/history use app-specific storage; credentials use provider-managed auth or Windows credential storage.
- Treat GitHub as optional: offline/local projects remain useful.
- Validate installed Antigravity entry points before selecting a CLI/SDK integration; discovery in this session found Codex and Ollama on PATH, but not Antigravity.
- Select an Ollama-compatible coding harness during its phase; raw model inference is insufficient.
- Calibrate complexity/latency against the user's own tasks; do not hard-code specific model names or pretend tokens determine difficulty.
- Windows first; portability can follow after process management and paths have verified behavior.

## Sources and reference evidence

Inspected `C:\Projects\Navis-Protocol\frontend\src\app\globals.css`, frontend package metadata, and Tauri configuration. These establish the visual reference and Tauri precedent, not reusable runtime behavior.

- [Tauri project setup](https://v2.tauri.app/start/create-project/) and [capabilities](https://tauri.app/security/capabilities/).
- [Codex non-interactive mode](https://learn.chatgpt.com/docs/non-interactive-mode) and [Codex SDK](https://learn.chatgpt.com/docs/codex-sdk): candidate integration surfaces; exact supported behavior must be verified against the installed version.
- [Antigravity CLI reference](https://www.antigravity.google/docs/cli/reference/): candidate adapter surface, not locally verified availability.
- [Ollama model list API](https://docs.ollama.com/api/tags): local discovery surface; coding tool execution is a separate design responsibility.
