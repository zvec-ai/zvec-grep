from __future__ import annotations

import contextlib
import io
import unittest

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

    def test_opencode_default_package_fails_before_harbor(self) -> None:
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
                    "--dry-run",
                ]
            )


if __name__ == "__main__":
    unittest.main()
