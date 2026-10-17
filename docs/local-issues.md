# Local issue records

Runner-P1 stores one issue per Markdown file with schema-versioned frontmatter. The Rust `issues` module is independent of Tauri so the later shared runner and native UI can call the same record and validation API.

## Record format

The supported record fields are `schema_version` (currently `1`), stable project-scoped `id`, non-empty `title`, `type`, `status`, optional string-or-null `priority`, `parent` and `depends_on` lists, optional `acceptance` and `completion_evidence`, and optional GitHub provenance such as `github_url` or `github_number`. Unknown frontmatter keys and the Markdown body are retained. An explicit `priority: null` remains null.

Frontmatter currently accepts JSON values, which are a strict, interoperable subset of YAML. Unsupported YAML syntax is rejected with the offending line or key; it is never silently discarded. This covers the imported GP-Grimoire format in GP-105 through GP-112. A GP-L001-style ID was not present in the imported snapshot; the tests use a synthetic local ID of that form. Missing dependency records are reported in list/show as unresolved, never as complete.

## Persistence and validation

`IssueStore` exposes validated `list`, `show`, `create`, `update`, and `delete` operations. Writes use UTF-8 BOM Markdown and a same-directory temporary file for atomic publication. A SQLite high-water journal is advanced in an immediate transaction before publication. New epic records (`type: "epic"`) receive `<SCOPE>-ENNN`; every other type receives `<SCOPE>-SNNN` for a story, either standalone or linked to an epic through `parent`. For example, FAB-E001 and FAB-S001 have independent sequences. Each namespace starts above its own highest existing ID and preserves its high-water mark across deletion and restart. Crashes may leave gaps but cannot reuse deleted IDs. Existing numeric imports and L IDs remain readable and unchanged, and legacy allocator rows remain intact without advancing the new E/S sequences. Updates require the revision returned from the file the caller read; a stale revision reports a conflict. SQLite serializes cooperating store clients. An unrelated external editor does not participate in that lock, so an external write racing between the revision check and rename is not prevented by this phase.

Duplicate IDs, unsafe IDs, malformed records, newer schema versions, dependency cycles, and invalid status transitions are rejected. Missing dependencies remain explicit unresolved references. Parent epics are records in their own right; completion is not a second executable task. Child acceptance is evaluated from direct child records using their acceptance checklists; status labels alone do not count.

Epic completion is derived from direct child acceptance checklists and requires at least one child; an empty or incomplete child set cannot mark an epic complete. The bounded `issues-page` summaries include optional epic progress (direct-child total, accepted count, and incomplete IDs), computed from those same checklists. Status labels alone, including `done` or `closed`, do not count as acceptance. Non-epic summaries omit the optional field, and clients can ignore it when absent. The local issue panel and versioned CLI are available; richer migration tooling and full YAML 1.2 support remain later work.
