# Local issues and runner batches in Fabrica

## Browse and edit Markdown issues

Open **Local issues**. Select the local project folder and its `runner.json`, then choose **Open / refresh**. A GitHub remote is not required. Search by issue ID/title, filter by status or priority, or choose an epic/parent. Lists render 100 rows per page and fetch full Markdown only for a selected issue.

Edit the title, status, priority, relationships or Markdown body, then choose **Save Markdown**. Unknown frontmatter remains preserved. Saves compare the revision read from disk; an external edit produces a conflict and retains your draft. Discard and refresh to inspect the external change. Drafts remain when switching Fabrica sections. **Preview dependencies** reads saved Markdown and does not start work. Generated HTML is an optional viewer, not another editable backlog.

## Review a plan and assign models

Open **Runner batches**. The runner state folder defaults to Fabrica's app-data folder. Use exactly the same folder with the CLI to share job history and controls. Choose a local plan prepared by Codex and select **Open plan preview**. Preview performs no provider execution.

Inspect the project, selected issues, task breakdown, declared checks, provider/model assignments, dependency evidence, concurrency, time window, token/API ceilings and permitted effects. Expand an issue to change task assignments and the reviewer reasoning effort advertised by its selected model. Reassigning to a model that does not support the old effort clears it to the model default. Any assignment or effort change disables Start until **Preview assignments again** succeeds. A changed source/settings snapshot requires a new valid preview; the original fingerprint cannot authorize a different plan. Start submits the exact reviewed request and returns while detached workers continue when the OS host permits breakaway. A typed worker-launch failure may leave admitted state; the UI reads that state back and disables duplicate Start. It never attaches an unrelated batch after a general ID conflict. For a host that forbids detached launch, use the explicit CLI hosted mode described in the CLI guide.

The desktop bridge resolves its packaged worker. The UI cannot choose an arbitrary worker executable or access a raw billing credential. Paid OpenRouter tasks require the reviewed capped-gateway policy and supported Aider 0.86.2 runtime. Actual-adapter synthetic evidence and live acceptance are separate; consult the current readiness report before dispatch.

## Reconnect and control

Enter a batch ID and the same state folder, then choose **Reconnect**. Fabrica remembers the connection fields locally and reconnects on reopening without submitting work. The CLI can inspect and control that same batch. Polling reads status and bounded event pages; it does not start new jobs.

The view shows reported costs/tokens, unresolved usage reservations, job states, attempts, completion gates, bounded check/checkpoint details and recent batch events. Unresolved usage stays reserved; a displayed measured total is not permission to spend the remainder independently.

**Pause** stops new claims after current work reaches its checkpoint. **Stop after current work** is graceful. **Cancel active work** requests cancellation of the recorded owned process. **Reconcile** inspects ambiguity and does not retry it automatically. A needs-input state requires resolving the stated evidence, credentials or manual acceptance requirement; repeated Resume is not evidence of success.

The controls use native labels, keyboard-operable inputs/buttons and the application's reduced-motion styling. Current browser verification uses synthetic mock IPC; native WebView/IPC and live-provider acceptance remain separate checks. See [CLI instructions](runner-cli.md) for setup, exact commands and current limitations.
