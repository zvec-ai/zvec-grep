from __future__ import annotations

import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from zg_bench import runner
from zg_bench.cli import build_parser, main

_FIXTURE_SUITE_NAME = "external-suite-fixture"
_FIXTURE_TASK = "fixture/task-one"
_FIXTURE_SUITE_YAML = f"""\
name: {_FIXTURE_SUITE_NAME}
dataset: fixture/external-dataset@1
tiers:
  smoke:
    tasks:
      - {_FIXTURE_TASK}
  full:
    all: true
"""


def _install_external_suite_fixture(test_case: unittest.TestCase) -> str:
    temp_dir = tempfile.TemporaryDirectory()
    test_case.addCleanup(temp_dir.cleanup)
    suites_dir = Path(temp_dir.name) / "suites"
    suites_dir.mkdir()
    (suites_dir / f"{_FIXTURE_SUITE_NAME}.yaml").write_text(
        _FIXTURE_SUITE_YAML,
        encoding="utf-8",
    )
    suites_patch = patch.object(runner, "SUITES_DIR", suites_dir)
    suites_patch.start()
    test_case.addCleanup(suites_patch.stop)
    return _FIXTURE_SUITE_NAME


class CliTests(unittest.TestCase):
    def setUp(self) -> None:
        self.suite_name = _install_external_suite_fixture(self)

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

    def test_doctor_forwards_embedding_endpoint(self) -> None:
        endpoint = "https://embedding.example/v1/embeddings"

        with patch("zg_bench.cli.run_doctor", return_value=0) as run_doctor:
            return_code = main(
                [
                    "doctor",
                    "--agent",
                    "claude-code",
                    "--model",
                    "claude-opus-5",
                    "--profile",
                    "zvec-grep",
                    "--embedding-endpoint",
                    endpoint,
                ]
            )

        self.assertEqual(return_code, 0)
        self.assertEqual(run_doctor.call_args.kwargs["embedding_endpoint"], endpoint)

    def test_lists_smoke_tasks(self) -> None:
        output = io.StringIO()

        with contextlib.redirect_stdout(output):
            return_code = main(["list", "tasks", self.suite_name, "--tier", "smoke"])

        self.assertEqual(return_code, 0)
        self.assertIn(_FIXTURE_TASK, output.getvalue())

    def test_lists_registered_suites(self) -> None:
        output = io.StringIO()

        with contextlib.redirect_stdout(output):
            return_code = main(["list", "suites"])

        self.assertEqual(return_code, 0)
        self.assertEqual(output.getvalue().splitlines(), [self.suite_name])

    def test_lists_supported_agent_models(self) -> None:
        output = io.StringIO()

        with contextlib.redirect_stdout(output):
            return_code = main(["list", "agent-models"])

        self.assertEqual(return_code, 0)
        listing = output.getvalue()
        self.assertNotIn("qwen-coder", listing)
        self.assertIn("qwen3.7-max", listing)
        self.assertIn("opencode", listing)
        self.assertIn("aliyun-glm-5.2", listing)
        self.assertIn("claude-code", listing)
        self.assertIn("claude-opus-5", listing)

    def test_rejects_unsupported_agent_model_during_parsing(self) -> None:
        stderr = io.StringIO()

        with contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit) as error:
            build_parser().parse_args(
                [
                    "run",
                    self.suite_name,
                    "--agent",
                    "opencode",
                    "--model",
                    "qwen3.8",
                ]
            )

        self.assertEqual(error.exception.code, 2)
        self.assertIn(
            "supported models: aliyun-glm-5.2, custom-openai/glm-5.2, qwen3.7-max",
            stderr.getvalue(),
        )

    def test_accepts_opencode_qwen_combination(self) -> None:
        args = build_parser().parse_args(
            [
                "run",
                self.suite_name,
                "--agent",
                "opencode",
                "--model",
                "qwen3.7-max",
            ]
        )

        self.assertEqual(args.agent, "opencode")
        self.assertEqual(args.model, "qwen3.7-max")

    def test_accepts_published_claude_code_configuration(self) -> None:
        args = build_parser().parse_args(
            [
                "run",
                self.suite_name,
                "--agent",
                "claude-code",
                "--model",
                "claude-opus-5",
            ]
        )

        self.assertEqual(args.agent, "claude-code")
        self.assertEqual(args.model, "claude-opus-5")

    def test_accepts_independent_trial_count(self) -> None:
        args = build_parser().parse_args(
            [
                "run",
                self.suite_name,
                "--agent",
                "opencode",
                "--model",
                "custom-openai/glm-5.2",
                "--n-attempts",
                "3",
            ]
        )

        self.assertEqual(args.n_attempts, 3)

    def test_legacy_package_fails_before_harbor(self) -> None:
        with self.assertRaisesRegex(SystemExit, "does not support"):
            main(
                [
                    "run",
                    self.suite_name,
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
                patch("zg_bench.cli.execute", side_effect=fake_execute),
                contextlib.redirect_stderr(stderr),
                contextlib.redirect_stdout(stdout),
            ):
                return_code = main(
                    [
                        "run",
                        self.suite_name,
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

    def test_run_keeps_embedding_endpoint_out_of_setup_cache(self) -> None:
        endpoint = "https://embedding.example/v1/embeddings"
        with tempfile.TemporaryDirectory() as temp_dir:
            with (
                patch("zg_bench.cli.collect_checks", return_value=[]),
                patch("zg_bench.cli.print_report", return_value=0),
                patch("zg_bench.cli.prepare_setup_cache", return_value=None) as cache,
                patch(
                    "zg_bench.cli.build_harbor_command",
                    return_value=["harbor"],
                ) as build,
                patch("zg_bench.cli.execute", return_value=0),
                patch("zg_bench.cli.job_has_exceptions", return_value=False),
            ):
                return_code = main(
                    [
                        "run",
                        self.suite_name,
                        "--agent",
                        "claude-code",
                        "--model",
                        "claude-opus-5",
                        "--profile",
                        "zvec-grep",
                        "--embedding-endpoint",
                        endpoint,
                        "--jobs-dir",
                        temp_dir,
                        "--job-name",
                        "endpoint-forwarding",
                    ]
                )

        self.assertEqual(return_code, 0)
        cache_kwargs = cache.call_args.kwargs
        self.assertNotIn("embedding_endpoint", cache_kwargs)
        self.assertEqual(cache_kwargs["embedding_model"], runner.ZVEC_GREP_EMBEDDING)
        self.assertEqual(len(build.call_args_list), 2)
        for build_call in build.call_args_list:
            self.assertEqual(build_call.kwargs["embedding_endpoint"], endpoint)


if __name__ == "__main__":
    unittest.main()
