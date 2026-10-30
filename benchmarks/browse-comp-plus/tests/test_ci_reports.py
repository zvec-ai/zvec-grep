from __future__ import annotations

import contextlib
import io
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from zg_bench.artifacts import write_json
from zg_bench.ci_reports import compact_report, main, render_retry_summary, retry_counts


class RetrySummaryTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.artifacts = self.root / "artifacts"
        self.run = self.artifacts / "runs/current"
        write_json(self.run / "run.json", {"query_ids": ["628"], "trials_per_case": 3})

    def trial(self, profile, index, statuses, *, selected=True):
        trial = self.run / "cases/628" / profile / "trials" / f"trial-{index:03d}"
        for number, status in enumerate(statuses, 1):
            path = trial / "attempts" / f"attempt-{number:03d}"
            path.mkdir(parents=True)
            if status is not None:
                write_json(path / "result.json", {"status": status, "error": "SECRET"})
        if selected and statuses:
            write_json(
                trial / "result.json", {"status": statuses[-1], "error": "SECRET"}
            )
        return trial

    def test_selected_trials_and_raw_retry_costs_are_counted_separately(self):
        self.trial("baseline", 1, ["infrastructure_failed", "completed"])
        self.trial("baseline", 2, ["completed"])
        self.trial("baseline", 3, ["failed"])
        self.trial("zvec-grep", 1, ["completed"])
        self.trial("zvec-grep", 2, ["infrastructure_failed"] * 3)
        write_json(
            self.run / "cases/628/pair.json",
            {
                "trials": [
                    {"trial_index": 1, "eligible": True},
                    {"trial_index": 2, "eligible": False},
                ]
            },
        )
        counts = retry_counts(self.artifacts)
        self.assertEqual(
            counts["baseline"],
            {
                "planned": 3,
                "successful": 2,
                "paired": 1,
                "retries": 1,
                "failed_attempts": 2,
                "final_failures": 1,
                "unfinished": 0,
            },
        )
        self.assertEqual(
            counts["zvec-grep"],
            {
                "planned": 3,
                "successful": 1,
                "paired": 1,
                "retries": 2,
                "failed_attempts": 3,
                "final_failures": 1,
                "unfinished": 1,
            },
        )
        markdown = render_retry_summary(self.artifacts)
        self.assertNotIn("SECRET", markdown)
        self.assertIn("| baseline | 2/3 | 1 | 1 | 2 | 1 | 0 |", markdown)
        self.assertIn("exclude failed-attempt overhead", markdown)

    def test_interrupted_or_unselected_attempts_do_not_become_final_results(self):
        self.trial("baseline", 1, ["infrastructure_failed", None], selected=False)
        self.trial("baseline", 2, ["completed"], selected=False)
        counts = retry_counts(self.artifacts)["baseline"]
        self.assertEqual(counts["successful"], 0)
        self.assertEqual(counts["final_failures"], 0)
        self.assertEqual(counts["failed_attempts"], 1)
        self.assertEqual(counts["retries"], 1)
        self.assertEqual(counts["unfinished"], 3)

    def test_only_latest_worker_run_is_counted_and_aggregate_is_excluded(self):
        self.trial("baseline", 1, ["completed"])
        write_json(
            self.artifacts / "runs/old/run.json",
            {
                "query_ids": ["628", "700"],
                "trials_per_case": 3,
            },
        )
        os.utime(self.artifacts / "runs/old/run.json", ns=(1, 1))
        write_json(
            self.artifacts / "runs/aggregate/run.json",
            {
                "aggregate_only": True,
                "query_ids": ["628"] * 20,
                "trials_per_case": 3,
            },
        )
        counts = retry_counts(self.artifacts)["baseline"]
        self.assertEqual(counts["planned"], 3)
        self.assertEqual(counts["successful"], 1)

    def test_partial_invalid_diagnostics_and_missing_runs_are_safe(self):
        trial = self.trial("baseline", 1, ["completed"])
        (trial / "result.json").write_text('{"error":"SECRET",')
        write_json(trial / "attempts/attempt-001/result.json", {"status": ["SECRET"]})
        write_json(self.run / "cases/628/pair.json", {"trials": [None, "SECRET"]})
        counts = retry_counts(self.artifacts)["baseline"]
        self.assertEqual(counts["successful"], 0)
        self.assertEqual(counts["failed_attempts"], 0)
        self.assertNotIn("SECRET", render_retry_summary(self.artifacts))
        self.assertEqual(retry_counts(self.root / "missing")["baseline"]["planned"], 0)

    def test_cli_appends_to_step_summary_and_defaults_to_artifacts(self):
        self.trial("baseline", 1, ["completed"])
        summary = self.root / "summary.md"
        summary.write_text("Previous summary\n")
        with (
            patch("zg_bench.ci_reports.DEFAULT_ARTIFACTS_DIR", self.artifacts),
            patch.dict(os.environ, GITHUB_STEP_SUMMARY=str(summary)),
            contextlib.redirect_stdout(io.StringIO()) as output,
        ):
            self.assertEqual(main(["retries"]), 0)
        self.assertEqual(output.getvalue(), "")
        self.assertTrue(
            summary.read_text().startswith("Previous summary\n## Trial retries")
        )
        self.assertIn("| baseline | 1/3", summary.read_text())


class CompactReportTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.evidence = self.root / "evidence"
        self.evidence.mkdir()
        self.output = self.root / "compact"

    def write(self, relative, value="public"):
        path = self.evidence / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(value)
        return path

    def test_compact_copies_reports_and_identity_without_raw_or_runtime_payloads(self):
        retained = [
            "benchmark.toml",
            "candidate-manifest.json",
            "ci.json",
            "plan.json",
            "runs/worker/run.json",
            "runs/worker/report/summary.md",
            "runs/worker/report/summary.json",
            "runs/worker/report/trials.csv",
            "runs/aggregate/run.json",
            "runs/aggregate/report/ci-completeness.json",
        ]
        excluded = [
            "runs/worker/cases/628/baseline/events.jsonl",
            "runs/worker/cases/628/baseline/attempts/attempt-001/result.json",
            "runs/worker/evaluation/judge/events.jsonl",
            "runs/worker/profiles/auth.json",
            "models/potion/model.safetensors",
            "logs/index.stdout.log",
            "source/queries.jsonl",
        ]
        for relative in retained + excluded:
            self.write(relative, relative)
        compact_report(self.evidence, self.output)
        self.assertEqual(
            sorted(
                str(path.relative_to(self.output))
                for path in self.output.rglob("*")
                if path.is_file()
            ),
            sorted(retained),
        )
        for relative in retained:
            self.assertEqual((self.output / relative).read_text(), relative)

    def test_missing_reports_and_optional_metadata_do_not_hide_failed_run(self):
        self.write("runs/failed/run.json", '{"status":"failed"}')
        compact_report(self.evidence, self.output)
        self.assertTrue((self.output / "runs/failed/run.json").is_file())
        self.assertFalse((self.output / "runs/failed/report").exists())

    def test_symlinks_in_retained_report_are_rejected_and_partial_output_removed(self):
        self.write("benchmark.toml")
        report = self.write("runs/worker/report/summary.md")
        report.unlink()
        report.symlink_to(self.write("private"))
        with self.assertRaisesRegex(ValueError, "symlinks"):
            compact_report(self.evidence, self.output)
        self.assertFalse(self.output.exists())

    def test_excluded_symlink_is_not_traversed(self):
        self.write("runs/worker/run.json")
        (self.evidence / "runs/worker/profiles").symlink_to(self.root)
        compact_report(self.evidence, self.output)
        self.assertFalse((self.output / "runs/worker/profiles").exists())

    def test_existing_or_overlapping_destination_is_preserved(self):
        self.output.mkdir()
        marker = self.output / "keep"
        marker.write_text("unchanged")
        for output in (self.output, self.evidence / "nested", self.root):
            with self.assertRaises(ValueError):
                compact_report(self.evidence, output)
        self.assertEqual(marker.read_text(), "unchanged")
        self.assertFalse((self.evidence / "nested").exists())

    def test_cli_error_does_not_echo_sensitive_paths(self):
        with contextlib.redirect_stderr(io.StringIO()) as error:
            self.assertEqual(
                main(
                    [
                        "compact",
                        "--evidence",
                        str(self.root / "SECRET"),
                        "--output",
                        str(self.output),
                    ]
                ),
                1,
            )
        self.assertNotIn("SECRET", error.getvalue())


if __name__ == "__main__":
    unittest.main()
