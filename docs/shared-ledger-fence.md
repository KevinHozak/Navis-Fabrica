# Shared helper writer fence — S017

The current Windows helper requires a durable per-user accounting authority before it opens any shared helper ledger. The fixed authority directory is obtained from the OS UserProfile known folder and ends in `NavisFabrica-Portal/accounting-authority`. The former Local AppData consumer root is detection-only: any existing root, even empty, conflicting or inaccessible, blocks default-namespace use rather than selecting an independent authority. It is not selected by `--config`, `LOCALAPPDATA`, `FABRICA_AGENT_CONFIG` or an authority-path environment override. Missing, invalid, paused, mismatched or replaced authority/ledger fails closed. No real authority is initialized by installation/import or the helper itself.

**Compatibility:** existing unmanaged helper calls (including local inference and `usage`) now refuse until a separately reviewed authority exists. No fallback to an unguarded ledger, automatic initialization or empty replacement database is allowed. Codex execution, Aider's separate accounting and Tauri app defaults are not changed by this standalone-helper feature. Windows ownership/DACL checks remain mandatory for authority, ledger, parent and existing SQLite sidecars.

`scripts/ledger_fence.py` exposes trusted backend/operator APIs `Fence.initialize`, `pause`, `select_snapshot` and `resume`. These are not renderer/HTTP operations and have no automatic CLI startup hook. Operator APIs may accept explicit roots for generated fixtures; production `Ledger` always uses the OS-known-folder root. Do not expose root selection to an untrusted profile. Initialization requires an existing protected database with accounting schema and starts paused, never active.

A short exclusive control lock linearizes admissions and state revisions. Shared writer leases span each database connection and its request/settlement. Pause persists immediately after winning the control lock; already admitted work may finish or remain charged/held. New reservations are denied, including repeated/raw inserts through the guarded connection (statement caching is disabled). The Portal adapter holds admission only over its reservation transaction, not a network call, so pause can persist while an in-flight request drains. Connection/process death releases OS leases without changing accounting or implicitly resuming.

A snapshot selection requires paused state, the exact integer revision and exclusive drain of all gated leases. It verifies complete schema/data equality of pre-existing protected source/candidate databases, then atomically writes the new path/file identity and epoch while retaining retired paths. It copies no accounting and stays paused. Resume separately requires the current revision, unchanged file identity and drained leases. Ledger connections use existing-only SQLite `mode=rw`; missing files cannot create a fresh history. Retired paths, hardlink aliases/different paths, replaced files, stale epochs and stale revisions refuse. Snapshot comparison is bounded; an oversized/unexpected ledger requires review rather than truncation.

State uses fsynced temporary content and Windows write-through replacement. Missing/corrupt state refuses instead of recovering from an older backup. Failed/interrupted transitions stay paused or retain the previously committed state; no operation auto-resumes. A commit reported as interrupted must be read back before retry. Do not restore old authority files, clear retirement history, replay unknown requests, invent new operation/run IDs, reset caps or reactivate an old ledger after new accounting writes.

The fence covers updated trusted direct/helper and Portal-mirror entry points. It cannot retrofit older already-running imports, old script/binary copies, arbitrary SQLite writers, privileged file/manifest rollback or malicious modified code. All such writers must be inventoried, quiesced and excluded before a real cutover. Parent replacement protections, source provenance, full backup/recovery tooling, config/profile changes and operator maintenance authorization remain separate gates. Changing a Portal profile's ledger path changes its profile hash; no existing run binding is rewritten or bypassed automatically.

No production ledger was opened/migrated, canonical authority created, credentials resolved, route enabled or paid call made for offline qualification. Tests seed existing generated databases and inject roots only in test code, never through production environment/config. Source changes require fresh qualification; the Portal S017 evidence binds exact versions/hashes. No Tauri/native/frontend code changed, so no desktop version bump is allocated.


## S018 profile namespace policy — 2026-11-02

The operator approved engineering/offline qualification of the fixed UserProfile location only. Folder lookup uses Windows APIs, not USERPROFILE/LOCALAPPDATA or config overrides. Existing default-root sessions recheck the namespace at admissions/control operations; appearance of old AppData state blocks new admissions. Already admitted settlement remains governed by its existing lease/accounting rules. Legacy state is not deleted, relocated, reset or merged automatically. File/reparse conflicts and unreadable roots refuse.

Production operator work must use the default namespace guard, not an explicit fixture root to bypass it. Tests create only disposable private roots. No production directories, accounting authority, active config or migration are provisioned by this change, and no helper resumes or provider route becomes enabled. Any real deployment requires the separate reviewed S018 maintenance/cutover authorization.


## S018 atomic bootstrap retirement — 2026-11-02

`Fence.initialize(ledger, retired_paths=(), verifier=None)` validates every initial
retirement before creating state. The first atomic manifest is paused, revision
one/epoch one, and already contains the reviewed original-source retirement.
Duplicate/selected-path/invalid retirements refuse. Existing non-relocation
callers retain the empty default; a cutover coordinator must supply its source.
Supported snapshot selection refuses a retired source immediately and after
restart. Do not add retirement by hand-editing a manifest after initialization.

The optional verifier is a trusted backend callable only, never renderer/config
input. It executes after the first commit while control and exclusive writer
locks remain held; competing pause/resume/selection refuses until verification
finishes. A verifier/interruption failure leaves any committed state paused with
retirement intact, and performs no automatic retry, cleanup, rollback or resume.
Portal's offline operator coordinator also retains source/config and destination
pins across this complete window. These APIs were exercised only with generated
roots; no real authority was initialized by the engineering change.
