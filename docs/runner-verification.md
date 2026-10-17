# Runner verification and delivery gates

Verification is an observed gate, separate from implementation completion. A worker may propose completion, but the runner accepts it only after declared checks pass against the exact source snapshot, an independent reviewer passes, and any declared manual acceptance is recorded separately.

`runner_core::verification::VerificationService` accepts check and review executor adapters. Its built-in `ProcessCheckExecutor` runs an argv-based command with an explicit repository-relative working directory, a 30-minute maximum timeout, and bounded output capture. Shell interpolation is not used. Captured command text is discarded after execution; durable receipts keep only exit status, timeout/truncation flags, and byte count. The service snapshots Git HEAD, tracked diff bytes (`--no-ext-diff --no-textconv`), untracked paths, and bounded untracked contents before and after each check. Untracked paths are resolved under the canonical repository root; reads stop at the 64 MiB source limit. Fingerprint fields are length-prefixed so arbitrary file bytes, including NUL, cannot confuse field boundaries. A change before or during a check makes its receipt stale. Ordinary checks pass only on observed zero exit without timeout, cancellation, or incomplete output. `VerificationEnvelope` carries separate exact issue-Markdown revision, worker-task, and code-checkout identities. Production callers use `verify_job_with_issue_source_and_gate_guard`, which rereads bounded authoritative Markdown before checks and immediately before the final gate, validates its raw-byte `issues::revision` and parsed issue ID, rejects changes during review as stale, and runs a final supervisor lease-ownership guard before setting Verified. `verify_job` remains the lower-level trusted adapter API for synthetic Rust fixtures. The service runs every declared check and the independent reviewer, then creates `VerificationEvidence` internally; `JobsStore::verify_gate_with_code_snapshot` gates on the original issue fingerprint while preserving code HEAD/fingerprint in evidence. Callers cannot supply a `passed=true` assertion in place of execution.

Review requests include the issue fingerprint, same code snapshot and check receipts, acceptance criteria, and distinct worker/reviewer task IDs. The supervisor's `RuntimeReviewExecutor` calls the shared configured runtime with the chosen reviewer profile/model and a read-only review request; missing profile/model, input requests, and malformed JSON fail closed. Each check and review is checkpointed as started before dispatch and completed with its observed result; full checkpoint transitions are retained in the append-only job event log. Review responses and criteria have bounded counts and sizes. Review findings fail the gate. Explicit manual acceptance is durably recorded by `JobsStore` as `NeedsInput`; it cannot mark a job verified. Repair progress is checkpointed per round and limited to two rounds, with separate pre-repair and resulting source fingerprints. Requeue requires an already persisted `Started` round and only succeeds while the exact issue snapshot awaits verification; duplicate requeue is rejected. `JobsStore` supports verifier checkpoints after the worker lease closes, interrupted-repair checkpointing during process/effect reconciliation, and terminal verification failure. An interrupted `started` round remains visible for recovery; it cannot be interpreted as a pass.

The effect ledger protocol separates local edits and checks from commit, push, pull-request creation, merge, deployment, or publication. Each effect gets a stable idempotency key and a durable intent before dispatch. Results and reconciliation append transition records. Every transition keeps the original source snapshot and check IDs bound to the key. Confirmed and reconciled effects are idempotent. An ambiguous result cannot be retried until an adapter reconciles external state; this prevents duplicate publication when a request may have succeeded despite a lost response. Durable storage is supplied by the application through `EffectLedger`/`SqliteEffectLedger`.

The portable runner has no implicit dependency on `C:\Projects\Agents`. A caller may explicitly configure the optional `agents_delivery_contract` check adapter to consume an existing Issue Delivery Contract run record with the read-only `workflow_evidence.py report` command. Its helper and record paths are explicit absolute paths, and the record must already exist outside the checked worktree. The adapter invokes exactly the declared command once; it does not initialize or update the record, run checkpoints, or perform publication. The helper's bounded stdout JSON must report `ready: true`, `status: "ready"`, the canonical checked repository path, the current Git HEAD, and a non-empty helper fingerprint. The runner's own before/after source snapshot remains authoritative. A draft/not-ready report, missing dependency, unavailable interpreter/helper/record, malformed JSON, or mismatched repository/HEAD returns NeedsInput even when the helper exits zero. Raw report contents are consumed in memory and are not written to job checkpoints or receipts.

Example opt-in check configuration (paths must be replaced with existing local files):

```json
{
  "id": "issue-delivery-contract",
  "program": "C:/Python/python.exe",
  "args": [
    "C:/Projects/Agents/scripts/workflow_evidence.py",
    "report",
    "--repo",
    ".",
    "--record",
    "C:/Workflow/runs/FAB-L008.json"
  ],
  "cwd": ".",
  "timeout_ms": 30000,
  "output_limit_bytes": 262144,
  "adapter": "agents_delivery_contract"
}
```

This explicit check consumes the readiness gate when a caller configures it; it does not create or update a PR or provide remote review/publication evidence. The local-only GP109 plan omits this adapter. Hosted reviews, remote publication, deployment, and human acceptance require explicit adapters/evidence and are never inferred from a worker exit code.

Batch workers use `JobsStore::claim_ids` and `reconcile_expired_ids` with their persisted batch job ID set. Shared job databases therefore do not let one worker claim another batch's queued job or mark its expired lease for reconciliation. The unscoped methods remain available to standalone coordinators.

The fingerprint uses Git's configured object hash over deterministic snapshot material and includes the exact Git HEAD. It intentionally excludes ignored files and Git metadata outside the described source view. Checks should therefore run with the repository's declared check commands and should not rely on ignored build output as source evidence.
