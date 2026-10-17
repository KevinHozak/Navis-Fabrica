# Runner job ledger

`runner_core::jobs` is the durable, provider-neutral state store for issue work. It is separate from the Tauri `workflow_runs` history: old run records remain historical display data and are not silently imported as executable jobs. The caller selects the SQLite path, normally under application data, and supplies the authoritative Markdown fingerprint and immutable issue snapshot. The ledger never reads credentials, writes issue Markdown, launches a provider, or interprets a process exit as completion.

## Lifecycle and safety rules

Submission is idempotent by caller-provided key and exact request JSON. A conflicting reuse fails. Batch submission is transactional, rejects dependency cycles, and records every prerequisite as a linked job, explicit closed external evidence, or unresolved. Unresolved prerequisites remain blocked until a caller records evidence; missing issues do not imply closed issues. A linked job becomes eligible only after its prerequisite's completion gate is verified.

Claims serialize through an immediate SQLite transaction. Each claim increments a fencing token and creates an attempt; heartbeat and worker mutations must present the current worker and token before lease expiry. An expired lease moves to `reconciliation_required`. The job cannot be claimed again until a caller supplies both explicit process-stopped and effects-reconciled evidence (or chooses needs-input/failed). Cancellation of ambiguous work is rejected until recovery is resolved. This module records process references and cancellation requests but does not terminate processes itself.

An attempt may propose completion, but the job stays `awaiting_verification` until non-empty passing evidence matches the exact source fingerprint. A changed source invalidates non-active work as stale; active or ambiguous jobs must be reconciled first. Epic progress is computed from current direct child snapshots and their verified gates; it is an aggregate report, not an automatic epic completion.

## Plans and operation

Plans are bounded to 32 tasks, one implementation root, and direct worker/reviewer children only (no recursive delegation). Every task needs acceptance criteria; file paths must be relative and cannot traverse upward. Per-task attempts, token, cost, time, and concurrency are checked against the job/root caps, including aggregate child budgets. Provider/model assignment is metadata only; execution and credential lookup belong to the typed service layer.

`JobsStore::open` creates schema version 1 and rejects a database with a newer schema. SQLite foreign keys, WAL, and a busy timeout are enabled. Event rows retain lifecycle decisions; attempt rows retain leases, worker/process references, outcomes, and assignment. Checkpoints are upserted by name, including verified-completion evidence. Callers should keep the database in application data, back it up with the rest of durable runner state, and surface migration or I/O errors rather than silently resetting it.

The synthetic unit tests exercise idempotency, transactional ordering, unresolved external prerequisites, fencing, expiry reconciliation, source invalidation, bounded plans, cycles, concurrent claims, future-schema rejection, and the GP-109 through GP-112 chain with GP-108 explicitly modeled as closed external evidence. They do not contact providers or prove Tauri wiring or process supervision.
