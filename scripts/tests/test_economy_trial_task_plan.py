import unittest

from scripts.economy_trial_task_plan import (
    TrialTaskPlanError,
    complete_acceptance,
    correct_implementation_task,
    parse_issue_requirements,
    validate_supervisor_prompt,
)


MULTIFILE_ISSUE = """---
schema_version: 1
id: QWENPILOT-S003
title: Synthetic multifile edit
type: task
status: ready
parent: []
depends_on: []
---
# Synthetic multi-file edit

Add a shared `slugify(text)` helper in text_utils.py and use it from `catalog.slug_for_title`. Slugs are lowercase ASCII alphanumeric words joined by hyphens; treat punctuation and whitespace as separators, normalize Unicode with NFKD, then discard remaining non-ASCII characters. Keep the public catalog function.

Acceptance:
- `text_utils.slugify` provides the shared slug behavior.
- `catalog.slug_for_title` delegates to the shared helper and keeps its public function name.
- Unicode normalization, em dash punctuation, digits, and ordinary punctuation follow the specification.
- Change only text_utils.py and catalog.py.
"""

SMALL_ISSUE = """# Synthetic small bug fix

Fix `normalize_name` so it trims surrounding whitespace and collapses internal whitespace to one space. Preserve case.

Acceptance:
- Leading and trailing whitespace is removed.
- Any run of internal whitespace becomes one ASCII space.
- Letter case is unchanged.
"""


def implementation_job(files):
    snapshot = MULTIFILE_ISSUE
    return {
        "issue_id": "QWENPILOT-S003",
        "source_fingerprint": "0123456789abcdef",
        "issue_snapshot": snapshot,
        "plan": {
            "tasks": [
                {
                    "task_id": "multifile_edit-implementation",
                    "kind": "implementation",
                    "files": files,
                    "acceptance": ["Implement the behavior specified by synthetic issue QWENPILOT-S003."],
                    "limits": {"max_attempts": 1},
                },
                {
                    "task_id": "multifile_edit-review",
                    "kind": "reviewer",
                    "files": files,
                    "acceptance": ["Syntax check only."],
                },
            ]
        },
    }


class EconomyTrialTaskPlanTests(unittest.TestCase):
    def test_multifile_description_includes_normative_unicode_rule(self):
        description, bullets = parse_issue_requirements(MULTIFILE_ISSUE)
        self.assertIn("normalize Unicode with NFKD, then discard remaining non-ASCII characters", description)
        self.assertEqual(complete_acceptance(MULTIFILE_ISSUE)[1], [description, *bullets])

    def test_shared_store_utf8_bom_does_not_hide_issue_frontmatter(self):
        self.assertEqual(parse_issue_requirements("\ufeff" + MULTIFILE_ISSUE), parse_issue_requirements(MULTIFILE_ISSUE))

    def test_builder_places_full_requirements_in_actual_task_and_prompt(self):
        files = ["text_utils.py", "catalog.py"]
        job = implementation_job(files)
        corrected, prompt = correct_implementation_task(job, MULTIFILE_ISSUE, files)
        task = corrected["plan"]["tasks"][0]
        description, bullets = parse_issue_requirements(MULTIFILE_ISSUE)
        for requirement in [description, *bullets]:
            self.assertIn(requirement, task["acceptance"])
            self.assertIn("- " + requirement, prompt)
        self.assertIn("Authoritative issue requirements (stored snapshot):\n" + job["issue_snapshot"], prompt)
        self.assertEqual(job["plan"]["tasks"][0]["acceptance"], ["Implement the behavior specified by synthetic issue QWENPILOT-S003."])
        self.assertEqual(corrected["plan"]["tasks"][1]["acceptance"], ["Syntax check only."])
        self.assertIn("Plan task count: 2", prompt)
        self.assertIn("Issue snapshot fingerprint: 0123456789abcdef", prompt)
        validate_supervisor_prompt(corrected, prompt)
        with self.assertRaises(TrialTaskPlanError):
            validate_supervisor_prompt(corrected, prompt.replace("Authoritative issue requirements", "Issue requirements"))

    def test_builder_rejects_issue_text_not_bound_to_persisted_snapshot(self):
        job = implementation_job(["text_utils.py", "catalog.py"])
        with self.assertRaises(TrialTaskPlanError):
            correct_implementation_task(job, SMALL_ISSUE, ["text_utils.py", "catalog.py"])

    def test_exact_fixture_allowlist_is_required(self):
        with self.assertRaises(TrialTaskPlanError):
            correct_implementation_task(implementation_job(["text_utils.py", "catalog.py", "other.py"]), MULTIFILE_ISSUE, ["text_utils.py", "catalog.py"])
        with self.assertRaises(TrialTaskPlanError):
            correct_implementation_task(implementation_job(["text_utils.py", "../catalog.py"]), MULTIFILE_ISSUE, ["text_utils.py", "catalog.py"])
        with self.assertRaises(TrialTaskPlanError):
            correct_implementation_task(implementation_job(["C:/outside.py"]), MULTIFILE_ISSUE, ["C:/outside.py"])

    def test_missing_or_ambiguous_acceptance_is_rejected(self):
        with self.assertRaises(TrialTaskPlanError):
            parse_issue_requirements("# Title\nDescription without an acceptance section")
        with self.assertRaises(TrialTaskPlanError):
            parse_issue_requirements("# Title\nDescription\n\nAcceptance:\n- one\nnot a bullet\n")
        with self.assertRaises(TrialTaskPlanError):
            complete_acceptance("# Title\n\nAcceptance:\n- one\n")

    def test_small_bugfix_preserves_case_requirement_in_full_acceptance(self):
        description, criteria = complete_acceptance(SMALL_ISSUE)
        self.assertEqual(criteria[0], description)
        self.assertIn("Preserve case.", criteria[0])
        self.assertIn("Letter case is unchanged.", criteria)


if __name__ == "__main__":
    unittest.main()
