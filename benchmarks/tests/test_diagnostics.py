from __future__ import annotations

import json
import os
import tempfile
import unittest
from pathlib import Path

from zg_bench.diagnostics import (
    format_job_diagnostics,
    job_has_exceptions,
    latest_job,
)


class DiagnosticsTests(unittest.TestCase):
    def test_reports_setup_error_and_exception_tail(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            job_dir = Path(temp_dir) / "job"
            trial_dir = job_dir / "trial-one"
            agent_dir = trial_dir / "agent"
            agent_dir.mkdir(parents=True)
            (job_dir / "result.json").write_text(
                json.dumps(
                    {
                        "stats": {
                            "evals": {
                                "eval": {
                                    "exception_stats": {
                                        "RuntimeError": ["trial-one"]
                                    }
                                }
                            }
                        }
                    }
                ),
                encoding="utf-8",
            )
            (agent_dir / "zvec-grep-setup.json").write_text(
                json.dumps(
                    {
                        "status": "failed",
                        "error_type": "RuntimeError",
                        "error": "index was not ready",
                    }
                ),
                encoding="utf-8",
            )
            (trial_dir / "exception.txt").write_text(
                "Traceback line\nRuntimeError: index was not ready\n",
                encoding="utf-8",
            )

            report = format_job_diagnostics(job_dir)

            self.assertIn("Exceptions: RuntimeError=1", report)
            self.assertIn("setup=failed, RuntimeError", report)
            self.assertIn("RuntimeError: index was not ready", report)

    def test_finds_latest_job(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            jobs_dir = Path(temp_dir)
            first = jobs_dir / "first"
            second = jobs_dir / "second"
            first.mkdir()
            second.mkdir()
            os.utime(first, (1, 1))
            os.utime(second, (2, 2))

            self.assertEqual(latest_job(jobs_dir), second)

    def test_reads_exception_from_trial_result_without_exception_file(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            job_dir = Path(temp_dir) / "job"
            trial_dir = job_dir / "trial-one"
            trial_dir.mkdir(parents=True)
            (job_dir / "result.json").write_text(
                json.dumps(
                    {
                        "stats": {
                            "evals": {
                                "eval": {
                                    "exception_stats": {
                                        "NonZeroAgentExitCodeError": ["trial-one"]
                                    }
                                }
                            }
                        }
                    }
                ),
                encoding="utf-8",
            )
            (trial_dir / "result.json").write_text(
                json.dumps(
                    {
                        "exception_info": {
                            "exception_type": "NonZeroAgentExitCodeError",
                            "exception_message": "qwen exited with status 1",
                            "exception_traceback": "traceback",
                        }
                    }
                ),
                encoding="utf-8",
            )

            self.assertTrue(job_has_exceptions(job_dir))
            report = format_job_diagnostics(job_dir)
            self.assertIn("Exception: NonZeroAgentExitCodeError", report)
            self.assertIn("qwen exited with status 1", report)


if __name__ == "__main__":
    unittest.main()
