# Project Workflow Harness

Reusable scaffolding for projects that need durable planning, bounded execution, human gates, and evidence-based handoffs.

## Install into a project

From your Navis-Fabrica checkout:

```powershell
python scripts/create_project_harness.py --target <target-project> --project-name ExampleProject
```

The command is conservative. It creates missing files and does not overwrite existing project instructions or planning files unless `--force` is explicitly provided.

## Generated structure

```text
AGENTS.md                 durable repository guidance, only created if absent
GOALS.md                  outcomes, scope, non-goals, and success conditions
PLANS.md                  roadmap and phase dependencies
PROMPTS.md                reusable prompts for each workflow stage
harness/
  build/
    phase-template.md     copy for each approved implementation phase
  context/
    README.md             rules for phase-specific evidence
  code_review/
    README.md             review evidence convention
  build-log.md            append-only phase and validation log
```

## Operating rules

- Read repository instructions and planning files before making changes.
- Do a read-only preflight before proposing implementation.
- Record scope, files, risks, acceptance criteria and authorization before repository writes. A planning-only request stops before writes.
- Keep phases bounded. Continue to another phase only when the enclosing request includes it.
- Record observed commands and results, including failures.
- Use the selected workflow's authorization. `dev-work` and `dev-push` include scoped commit, push and PR delivery through the [shared delivery contract]([FABRICA ROOT]/docs/issue-delivery-contract.md). Merge, deployment, credentials and unrelated external writes need their own authorization. Local `dev-issue` authorizes local issue persistence/readback only.
- Treat `AGENTS.md` as durable guidance, not a dumping ground for temporary progress.

Replace `[FABRICA ROOT]` with the actual installed shared checkout path. For another machine, resolve an installed equivalent explicitly; a missing contract is a dependency to report, not permission to invent publication rules. Existing repositories are preserved by the installer; this update does not migrate their guidance automatically.
