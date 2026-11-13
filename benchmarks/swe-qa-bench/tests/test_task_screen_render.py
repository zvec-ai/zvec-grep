"""The report exposes all three populations and auditable task-screen reasons."""
from __future__ import annotations

import copy
import unittest
from unittest.mock import patch

from swe_qa_fixtures import _judged_task_report, _set_report_trial_judgements
from zg_bench.metrics.summary import aggregate_cases
from zg_bench.reports.render import render_report


def report_with_score_dispersion():
    report = _judged_task_report("task:0")
    cases = []
    for index, width in enumerate((1, 2, 3, 4, 5, 35)):
        source = _judged_task_report(f"task:{index}")
        _set_report_trial_judgements(
            source,
            baseline=[5] + [49, 51] * 4 + [100],
            zvec=[5] + [50 - width, 50 + width] * 4 + [100],
        )
        cases.extend(source["cases"])
    report["cases"] = cases
    report["judge"]["usage"]["calls"] = 120
    report["gate"].update(expected_tasks=[c["task_id"] for c in cases],
                          valid_pairs=6, successful_judgements=120)
    return report


class TaskScreenRenderTests(unittest.TestCase):
    def test_three_stages_counts_common_coverage_and_exclusion_evidence(self):
        report = report_with_score_dispersion()
        before = copy.deepcopy(report)
        markdown = render_report(report)
        self.assertEqual(report, before)
        self.assertIn("| Raw |", markdown)
        self.assertIn("| After score trim, before task screening |", markdown)
        self.assertIn("| **After MAD task screening** |", markdown)
        self.assertIn("| baseline | 6 | 60 | 48 | 40 |", markdown)
        self.assertIn("| zvec-grep | 6 | 60 | 48 | 40 |", markdown)
        self.assertIn("**5/6 completed tasks**", markdown)
        self.assertIn("| Task | Source profile | Metric | Max dispersion | Median | MAD | Modified Z | Upper threshold | Reason |", markdown)
        self.assertIn("| task:5 | zvec-grep | judge |", markdown)
        self.assertIn("modified Z > 3.5; excluded from every profile", markdown)
        per_task = markdown.split("### Per-task results: After score trimming", 1)[1]
        for index in range(6):
            self.assertIn(f"| task:{index} |", per_task)
        row = next(line for line in per_task.splitlines() if line.startswith("| task:5 |"))
        self.assertTrue(row.endswith("| excluded |"))
        self.assertIn("MAD-excluded tasks", markdown)
        self.assertEqual(report["judge"]["usage"]["calls"], 120)

    def test_unavailable_references_and_current_profiles_are_explicit(self):
        markdown = render_report(report_with_score_dispersion())
        self.assertIn("| agent_wall_seconds | unavailable_zero_mad |", markdown)
        self.assertIn("| input_tokens | unavailable_zero_mad |", markdown)
        self.assertIn("Reference profiles: `baseline`, `zvec-grep`", markdown)
        self.assertIn("historical six-group comparison are not silently added", markdown)
        self.assertIn("not a p-value", markdown)
        self.assertIn("https://www.itl.nist.gov/div898/handbook/eda/section3/eda35h.htm", markdown)

    def test_small_scope_and_missing_trial_evidence_are_not_silent(self):
        report = _judged_task_report("task:0")
        markdown = render_report(report)
        self.assertIn("unavailable_insufficient_tasks", markdown)
        self.assertIn("No tasks were excluded by MAD screening", markdown)
        self.assertIn("engineering safeguard, not an official statistical minimum", markdown)
        report = report_with_score_dispersion()
        del report["cases"][0]["profiles"]["baseline"]["trials"]
        markdown = render_report(report)
        self.assertIn("unavailable_missing_dispersion", markdown)
        self.assertIn("Unscreened legacy evidence (baseline)", markdown)
        self.assertIn("N/A (unscreened)", markdown)
        self.assertIn("**6/6 completed tasks**", markdown)

    def test_empty_final_aggregate_formats_missing_means_as_na(self):
        report = report_with_score_dispersion()
        aggregate = aggregate_cases(report["cases"])
        for profile in aggregate["profiles"].values():
            for key in ("judge", "input_tokens", "tool_calls", "agent_wall_seconds"):
                profile[key] = None
        aggregate["comparison"] = {key: None for key in aggregate["comparison"]}
        with patch("zg_bench.reports.render.aggregate_cases", return_value=aggregate):
            markdown = render_report(report)
        self.assertIn("| **After MAD task screening** | N/A / N/A / N/A | N/A | N/A | N/A |", markdown)


if __name__ == "__main__":
    unittest.main()
