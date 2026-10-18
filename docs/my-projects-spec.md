# My Projects experience

Selected implementation phase: FAB-S002. Authorized 2026-10-18.

Home opens maximized to a roomy, searchable collection of explicitly selected projects. Discovery provides candidates only; new clones never join automatically. Settings manages selection, showing names and full paths, with an explicit save and read-back. Existing favorites are reused as the saved collection; hidden/view preferences no longer decide home membership. Empty collections guide setup. Missing saved projects remain selected and visible in Settings for removal.

Verified candidate directories under C:\Projects include DreamCatcher, GP-Grimoire, GP-Dev, Mimir, Navis-Fabrica, Navis-Protocol, Navis-Recon. These are reviewable suggestions, not proof of ownership or automatic selections. Other discovered projects remain unchecked unless the user selects them.

Cards show name, branch, changed-file count, local issue total and workflow activity or needs-attention state. Unknown/loading/error states remain explicit; never fabricate zero. Changed filenames come from read-only Git porcelain status, including untracked files and renames. Cached scans without file data show unknown until refreshed. Local counts use the project's runner.json and shared issues-page API; missing/invalid configuration reports unavailable without creating files in the project.

Clicking a card opens its changes, local issue editor, and workflow actions. GitHub issue counts, pickers and maintenance are absent from this experience. Workflows and local batch execution remain available through Activity, while providers/models and discovery settings are under Settings. Keep advanced workflow capabilities intact, preserving unsaved issue edits while navigating.

Verification: TypeScript/frontend production build; native Rust checks and scanner tests for changes including special filenames; persisted collection read-back; empty/error states; no remote issue actions; synchronized existing versions with no version bump. Browser preview cannot execute native scans, issue reads or workflows and must say so. No commit, push or release requested.

## Folder refresh clarification

Folder discovery is manual in Settings. Never rescan the root on startup, timers, or content events. On startup re-inspect saved selected entries only. Content watcher ignores access and metadata-only notifications, coalesces writes for 350 ms with a two-second maximum batch window, and refreshes only affected saved selected entries. No overlap: one active scan and a bounded deduplicated follow-up queue. New clones remain outside the catalog until explicit rediscovery. Saving discovery settings marks data stale and requires rediscovery. Existing issue edits remain mounted while navigating.

Candidate discovery and its cache retain local Git, other-host Git, and detected non-Git coding projects. A GitHub remote is not an ownership gate. Content notifications compare cached size/modified-time/directory stamps before re-inspecting a project, suppressing duplicate notifications as well as read/access events. Activity includes connected local batch state; unknown batch-to-project association is never guessed.

## Verification and delivery

Frontend production build and scripts/test-project-collection.mjs passed. All 74 native library tests passed serially; scanner coverage includes actual Git facts, candidate discovery/cache, special filenames/renames, settings persistence, and read/duplicate watcher suppression. An initial parallel performance test exceeded its timing target under cold-compile contention; its serial rerun passed without loosening bounds. Real read-only CLI calls confirmed GP-Grimoire and Fabrica issue totals, and explicit unavailable errors for missing stores/configs in the other named examples.

Live browser/native visual acceptance remains unconfirmed: local preview access is blocked by this environment's socket permissions. User subsequently authorized scoped commit/push/PR, main integration and a canonical release build. The delivery workflow prepares the required patch version, 0.26.1, before final checks; release packaging will not change it again. No GitHub issue records are created or maintained.
