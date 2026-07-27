from __future__ import annotations

import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from zg_bench.cli import build_parser, main


class CliTests(unittest.TestCase):
    def test_doctor_accepts_run_specific_context(self) -> None:
        args = build_parser().parse_args(
            [
                "doctor",
                "--agent",
                "opencode",
                "--model",
                "aliyun-glm-5.2",
                "--profile",
                "zvec-grep",
                "--zvec-grep-package",
                "..",
            ]
        )

        self.assertEqual(args.agent, "opencode")
        self.assertEqual(args.profile, "zvec-grep")
        self.assertEqual(args.zvec_grep_package, "..")

    def test_lists_smoke_tasks(self) -> None:
        output = io.StringIO()

        with contextlib.redirect_stdout(output):
            return_code = main(
                ["list", "tasks", "swebench-verified", "--tier", "smoke"]
            )

        self.assertEqual(return_code, 0)
        self.assertIn("swe-bench/pallets__flask-5014", output.getvalue())

    def test_lists_django_focused_tasks(self) -> None:
        output = io.StringIO()

        with contextlib.redirect_stdout(output):
            return_code = main(
                ["list", "tasks", "django-focused", "--tier", "full"]
            )

        self.assertEqual(return_code, 0)
        listing = output.getvalue()
        self.assertIn("Tasks: 5", listing)
        self.assertIn("swe-bench/django__django-11734", listing)
        self.assertIn("swe-bench/django__django-13344", listing)

    def test_lists_supported_agent_models(self) -> None:
        output = io.StringIO()

        with contextlib.redirect_stdout(output):
            return_code = main(["list", "agent-models"])

        self.assertEqual(return_code, 0)
        listing = output.getvalue()
        self.assertIn("qwen-coder", listing)
        self.assertIn("qwen3.7-max", listing)
        self.assertIn("opencode", listing)
        self.assertIn("aliyun-glm-5.2", listing)

    def test_rejects_unsupported_agent_model_during_parsing(self) -> None:
        stderr = io.StringIO()

        with contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit) as error:
            build_parser().parse_args(
                [
                    "run",
                    "swebench-verified",
                    "--agent",
                    "opencode",
                    "--model",
                    "qwen3.8",
                ]
            )

        self.assertEqual(error.exception.code, 2)
        self.assertIn(
            "supported models: aliyun-glm-5.2, qwen3.7-max",
            stderr.getvalue(),
        )

    def test_accepts_opencode_qwen_combination(self) -> None:
        args = build_parser().parse_args(
            [
                "run",
                "swebench-verified",
                "--agent",
                "opencode",
                "--model",
                "qwen3.7-max",
            ]
        )

        self.assertEqual(args.agent, "opencode")
        self.assertEqual(args.model, "qwen3.7-max")

    def test_accepts_pre_downloaded_uv_archive(self) -> None:
        args = build_parser().parse_args(
            [
                "run",
                "swebench-verified",
                "--agent",
                "opencode",
                "--model",
                "qwen3.7-max",
                "--uv-archive",
                "/tmp/uv.tar.gz",
            ]
        )

        self.assertEqual(args.uv_archive, Path("/tmp/uv.tar.gz"))

    def test_enables_github_proxy_by_default(self) -> None:
        args = build_parser().parse_args(
            [
                "run",
                "swebench-verified",
                "--agent",
                "opencode",
                "--model",
                "qwen3.7-max",
            ]
        )

        self.assertTrue(args.github_proxy)

    def test_accepts_github_proxy_opt_out(self) -> None:
        args = build_parser().parse_args(
            [
                "run",
                "swebench-verified",
                "--agent",
                "opencode",
                "--model",
                "qwen3.7-max",
                "--no-github-proxy",
            ]
        )

        self.assertFalse(args.github_proxy)

    def test_enables_zvec_index_cache_by_default(self) -> None:
        args = build_parser().parse_args(
            [
                "run",
                "swebench-verified",
                "--agent",
                "opencode",
                "--model",
                "qwen3.7-max",
            ]
        )

        self.assertTrue(args.zvec_index_cache)
        self.assertEqual(
            args.zvec_index_cache_dir,
            Path(__file__).parents[1] / ".cache" / "zvec-grep-indexes",
        )

    def test_accepts_zvec_index_cache_opt_out_and_directory(self) -> None:
        args = build_parser().parse_args(
            [
                "run",
                "swebench-verified",
                "--agent",
                "opencode",
                "--model",
                "qwen3.7-max",
                "--no-zvec-index-cache",
                "--zvec-index-cache-dir",
                "/tmp/zg-index-cache",
            ]
        )

        self.assertFalse(args.zvec_index_cache)
        self.assertEqual(
            args.zvec_index_cache_dir,
            Path("/tmp/zg-index-cache"),
        )

    def test_legacy_package_fails_before_harbor(self) -> None:
        with self.assertRaisesRegex(SystemExit, "does not support"):
            main(
                [
                    "run",
                    "swebench-verified",
                    "--agent",
                    "opencode",
                    "--model",
                    "aliyun-glm-5.2",
                    "--profile",
                    "zvec-grep",
                    "--zvec-grep-package",
                    "0.1.5",
                    "--dry-run",
                ]
            )

    def test_trial_exception_is_printed_when_harbor_exits_zero(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            jobs_dir = Path(temp_dir)

            def fake_execute(*args: object, **kwargs: object) -> int:
                job_dir = jobs_dir / "failed-job"
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
                            }
                        }
                    ),
                    encoding="utf-8",
                )
                return 0

            stderr = io.StringIO()
            stdout = io.StringIO()
            with (
                patch("zg_bench.cli.collect_checks", return_value=[]),
                patch("zg_bench.cli.print_report", return_value=0),
                patch("zg_bench.cli.prepare_setup_cache", return_value=None),
                patch(
                    "zg_bench.cli.prepare_suite_task_overrides",
                    return_value=SimpleNamespace(
                        dataset_path=jobs_dir / "prepared-tasks",
                        task_count=1,
                    ),
                ),
                patch("zg_bench.cli.execute", side_effect=fake_execute),
                contextlib.redirect_stderr(stderr),
                contextlib.redirect_stdout(stdout),
            ):
                return_code = main(
                    [
                        "run",
                        "swebench-verified",
                        "--agent",
                        "opencode",
                        "--model",
                        "qwen3.7-max",
                        "--profile",
                        "baseline",
                        "--jobs-dir",
                        str(jobs_dir),
                        "--job-name",
                        "failed-job",
                    ]
                )

            self.assertEqual(return_code, 1)
            self.assertIn("Exception: NonZeroAgentExitCodeError", stderr.getvalue())
            self.assertIn("qwen exited with status 1", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
