# Model usage dashboard

The Home page has a collapsible Model usage section. Opening Home refreshes metadata; Refresh usage imports changed sources again. Apply filters queries retained metadata without rereading logs. No provider request, inference, voice hook or background monitor is involved. Python 3.10+ on PATH is required; missing runtime/storage is an actionable error.

## Sources and retention

The native typed commands get_usage_dashboard, get_usage_dashboard_config and save_usage_dashboard_config use dashboard schema 1. Native IPC validates UTC intervals (at most 366 days), bounded filters and response sizes. Source configuration lives in the configured app storage as usage-dashboard-config.json, with schema_version, codex_root and up to eight additional fabrica_databases. Custom locations must be absolute and verified before saving. An empty Codex root disables discovery. Default native paths are resolved from Fabrica's actual configured data location, including workflow-runs.sqlite3, runner/jobs.sqlite3 and runner/budgets.sqlite3. Budget child-call settlements live in budgets.sqlite3; jobs.sqlite3 alone does not provide them.

The collector and its existing usage/cost helpers are bundled into the desktop binary and materialized in private usage-dashboard/tools storage. No executable or shell command comes from the frontend. Imports read only whitelisted Codex counters/timestamps/model metadata and native workflow/call/task metadata. They never persist prompts, responses, tool arguments, credential values or source content. Skill-file loading is not invocation/cost attribution. File-cache keys are hashed paths. Dashboard metadata is stored separately in usage-dashboard/dashboard.sqlite3 and is not a billing/admission ledger.

Imports are serialized by the native boundary and a transactional SQLite writer. Stable session IDs and native record IDs replace snapshots rather than accumulating them. Shorter/stale copies do not overwrite more complete retained observations. Imported history survives upstream pruning. Known fork-family token snapshots are deduplicated; inherited first counters without matching parent evidence are unpriced/unattributed. Fork activity timelines are unattributed to avoid counting copied activity twice. Missing parent metadata still limits fork correlation. Codex and Fabrica coverage stays separate where no reliable correlation exists.

Each pass bounds inventory to 10,000 files, imports to 400 changed sessions and 18 seconds of session traversal, session files to 64 MiB and events to 2 MiB. Native rows are bounded to 20,000 per source table; oversized records and limit exhaustion are visible coverage errors. Detail output is capped at 2,000 rows and native process output at 2 MiB/55 seconds. Repeat manual refresh to continue an initial partial Codex import; unchanged files are skipped. Missing or unreadable sources retain earlier data and display missing/partial/error states. Collection timestamps older than 15 minutes show stale. No source is silently treated as zero usage.

## Costs and token accounting

Settled native paid-call child rows supply actual USD and their settlement provenance, including legitimate zero. Missing/unsettled actual costs remain unknown; parent reservations are excluded from actual costs. Codex subscription logs do not report an itemized bill, so actual USD stays unknown. Codex API-equivalent estimates use exact-model rates from the bundled dated config/model-pricing.json snapshot; no aliases or automatic price refresh are applied. Unpriced coverage and pricing source/date are shown.

Cached reads/writes are subsets of input and reasoning is a subset of output; subsets are not added again. Missing cache-write counts use zero only for reconstruction and remain visibly unknown. Short/long-context scenarios exclude fees and service-tier uplifts and are not invoice bounds. Native paid-call token totals have no input/output/cache split; their estimates are broad min/max token-only ranges. Actuals and estimates have different coverage and are never summed. Native task durations are reported separately from workflow spans. No dashboard total is an app-wide budget or subscription quota claim.

## Date and time semantics

Date boundaries are explicit UTC midnights: [From, Before). Token-event costs are attributed to the token counter timestamp; paid-call costs to dispatch time. Costs are not prorated across a date window. Untimed usage is excluded and counted separately; timezone-less timestamps are not interpreted in the host timezone. File modification times are discovery/cache hints only and never determine report eligibility.

Elapsed spans and original adjacent activity intervals are clipped to the selected date range. Activity counts only gaps no longer than five minutes; gap classification happens before clipping, so clipping an idle gap never makes it active. Model-specific Codex intervals use the model at their left endpoint. Missing timelines remain unknown. Unfinished workflow spans end at their last observed event, not the current time. Codex spans are labeled observed/partial. Concurrent session spans overlap; adding them does not produce unique wall-clock, CPU or human work time. Record counts are scoped to each model/source group; they must not be added as unique chats across groups.

## Verification

Run python -m unittest discover -s scripts/tests -p test_usage_dashboard.py and the existing scripts/test_model_usage_stats.py suite. Fixtures cover costs/zero/unknown, exact pricing/cache/reasoning, idle clipping, UTC boundaries, duplicate/fork imports, concurrency, partial/missing timelines, retention and metadata privacy. Native source/path/range tests live in src-tauri/src/usage_dashboard.rs. Native UI acceptance additionally verifies command permissions, rendering, keyboard controls, empty/error states and refresh; fixture passes alone do not establish that acceptance.

The shared version profile explicitly includes the embedded collector scripts and pricing snapshot as app inputs. Changes to those files participate in synchronized app version preparation as well as Rust/frontend changes.
