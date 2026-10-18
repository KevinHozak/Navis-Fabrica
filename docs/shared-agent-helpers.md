# Shared agent helpers

Fabrica supplies reusable executable interfaces. Skill instructions remain in the caller's skill repository. No personal skill package, provider reference, user record, ledger or credential is bundled here.

| Helper | Interface and scope |
| --- | --- |
| `scripts/workflow_evidence.py` | `--help`; offline preflight, evidence checkpoints, failure capsules and bounded cycle snapshots. No workflow authorization or dispatch. |
| `scripts/create_project_harness.py` | `--target PATH --project-name NAME`; creates missing files from `templates/project-workflow-harness/`. Overwriting requires `--force`. Generated contract links resolve to this checkout. |
| `scripts/skill_regression.py` | `--root PATH [--json PRIVATE_REPORT]`; offline structural/link/credential-marker checks for caller-owned skills. |
| `scripts/model_usage_stats.py` | `--codex-home PATH --fabrica-db PATH --output PRIVATE_DIR [--pricing PATH]`; offline metadata reporting. Defaults use user-local state or `FABRICA_USAGE_DATA`; pricing overrides use `FABRICA_USAGE_PRICING`. Codex estimates are not subscription invoices. Output inside this checkout is refused. |
| `scripts/usage_metrics.py` | Importable cost-provenance and timestamp activity functions. |
| `scripts/backup-codex-skills.ps1` | `-BackupRoot PRIVATE_DIR -SharedSkillRoot CALLER_SKILLS`; optional `-SkillRoot`, `-DiscoveryRoot`, `-ProjectSkillRoot`, `-HelperRoot`. Copies and hashes files, excludes generated data, and records trusted skill/helper links. Unknown links refuse. |
| `scripts/dev-version/` | `set_version.py` and `bump_version.py`; use `--help`, explicit repository and normal task authorization. |
| `scripts/dev-build/` | `build_tauri.py` and `prepare_main.py`; caller authorizes build/Git effects. Existing canonical-main/origin-main assumptions are preserved. |
| `scripts/dev-issue/create-epic.ps1` | Retired publisher entry point; refuses without remote effects. Use the shared runner issue store. |

The [delivery contract](issue-delivery-contract.md), [harness principles](harness-engineering.md), [improvement interface](workflow-improvements.md) and [model workflows](agent-model-workflows.md) describe the boundaries. Callers may retain thin legacy entry points or helper-directory junctions. Keep one executable implementation here; configure local checkout and state paths externally. Historical private records do not belong in this repository.

## Validation

Run the helper suites from `scripts/` using `python -m unittest test_workflow_evidence test_workflow_failure_capsules test_project_harness test_skill_regression test_model_usage_stats`, then `python -m unittest discover -s scripts/tests` from the checkout root. The caller compatibility test requires `FABRICA_COMPAT_ROOT`; otherwise that test skips. Windows backup/junction verification requires PowerShell and skips elsewhere. `WORKFLOW_TEST_TMP` selects an external fixture directory.

On October 17, 2026, migration validation passed 47 helper tests and 21 additional model-helper/backup/CLI tests with caller compatibility enabled. This is migration/interface evidence, not a live release build or remote GitHub acceptance result. No paid inference was needed for the migration.
