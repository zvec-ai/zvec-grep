from __future__ import annotations

import json
import os
import subprocess
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

from zg_bench.ci import (
    _contains_secret,
    configure_auth,
    export_evidence,
    prepare_sources,
    secret_values,
    verify_model,
)
from zg_bench.config import load_config
from zg_bench.models import TraceSummary, Usage
from zg_bench.runner import load_suite


class CIIntegrationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.home = self.root / "opencode-home"
        self.artifacts = self.root / "artifacts"
        self.output = self.root / "evidence"
        self.manifest = self.root / "candidate.json"
        self.manifest.write_text('{"commit":"candidate-sha"}')
        environment = patch.dict(
            os.environ, {"PATH": os.environ.get("PATH", "")}, clear=True
        )
        environment.start()
        self.addCleanup(environment.stop)

    def write(self, relative, content):
        path = self.artifacts / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        return path

    def export(self):
        export_evidence(self.artifacts, self.output, self.home, self.manifest)

    def configure(self):
        os.environ.update(
            OPENAI_API_KEY="model-api-secret",
            BROWSE_COMP_MODEL_BASE_URL="https://model.example.test/v1",
        )

    def test_source_preparation_can_be_cached_before_any_index_build(self):
        for ready in (None, self.artifacts / "state/dataset.json"):
            with (
                self.subTest(ready=ready),
                patch("zg_bench.dataset.prepared_dataset", return_value=ready),
                patch("zg_bench.dataset.fetch") as fetch,
                patch("zg_bench.index.build_index") as build,
                patch("zg_bench.ci.DEFAULT_ARTIFACTS_DIR", self.artifacts),
            ):
                prepare_sources()
            self.assertEqual(fetch.call_count, int(ready is None))
            build.assert_not_called()

    @patch("zg_bench.ci.prepare_judge_home")
    @patch("zg_bench.ci.subprocess.run")
    def test_credentials_stay_in_environment_and_home_is_private(self, run, prepare):
        self.configure()
        run.return_value.stdout = "--format --model --agent --dir --pure"
        run.return_value.stderr = ""
        configure_auth(self.home)
        self.assertEqual(self.home.stat().st_mode & 0o777, 0o700)
        self.assertFalse((self.home / "auth.json").exists())
        self.assertEqual(run.call_count, 1)
        self.assertEqual(run.call_args.args[0], ["opencode", "run", "--help"])
        self.assertEqual(
            run.call_args.kwargs["env"]["OPENAI_API_KEY"], "model-api-secret"
        )
        self.assertNotIn("model-api-secret", run.call_args.args[0])
        self.assertEqual(prepare.call_args.args[2], self.home)

    def test_missing_credentials_or_endpoints_fail_before_commands(self):
        self.configure()
        for name in (
            "OPENAI_API_KEY",
            "BROWSE_COMP_MODEL_BASE_URL",
        ):
            with (
                self.subTest(name=name),
                patch.dict(os.environ, {name: ""}),
                patch("zg_bench.ci.subprocess.run") as run,
            ):
                with self.assertRaises(ValueError):
                    configure_auth(self.home)
                run.assert_not_called()

    def test_remote_embedding_still_requires_its_own_credentials(self):
        config = load_config()
        remote = replace(
            config,
            zvec_grep=replace(
                config.zvec_grep, embedding="qwen/qwen3.7-text-embedding"
            ),
        )
        self.configure()
        for values, error in (
            ({}, "QWEN_EMBEDDING_API_KEY"),
            ({"ZVEC_GREP_API_KEY": "embedding-secret"}, "QWEN_EMBEDDING_ENDPOINT"),
        ):
            with (
                self.subTest(values=values),
                patch.dict(os.environ, values),
                patch("zg_bench.ci.load_config", return_value=remote),
                patch("zg_bench.ci.subprocess.run") as run,
            ):
                with self.assertRaisesRegex(ValueError, error):
                    configure_auth(self.home)
                run.assert_not_called()

    @patch("zg_bench.ci.prepare_judge_home")
    @patch("zg_bench.ci.subprocess.run")
    def test_incompatible_opencode_fails_before_corpus_preparation(self, run, prepare):
        self.configure()
        run.return_value.stdout = "--model"
        run.return_value.stderr = ""
        with self.assertRaisesRegex(ValueError, "--format"):
            configure_auth(self.home)
        prepare.assert_not_called()

    @unittest.skipUnless(
        os.environ.get("OPENCODE_BENCHMARK_TEST_BINARY"),
        "set OPENCODE_BENCHMARK_TEST_BINARY to validate the installed CLI offline",
    )
    def test_pinned_opencode_supports_required_run_flags(self):
        # setUp isolates credentials, so capture the explicitly selected binary on the class.
        result = subprocess.run(
            [self.opencode_binary, "run", "--help"],
            capture_output=True,
            text=True,
            check=True,
            timeout=30,
        )
        for flag in ("--format", "--model", "--agent", "--dir", "--pure"):
            self.assertIn(flag, result.stdout + result.stderr)

    opencode_binary = os.environ.get("OPENCODE_BENCHMARK_TEST_BINARY", "opencode")

    def test_export_keeps_partial_evidence_and_excludes_auth_and_corpus(self):
        self.write("runs/run-1/run.json", '{"status":"partial"}')
        self.write("runs/run-1/report/summary.md", "# Partial report\n")
        self.write("runs/run-1/cases/628/baseline/result.json", '{"status":"failed"}')
        self.write("runs/run-1/evaluation/summary.json", '{"status":"partial"}')
        self.write("logs/index.stderr.log", "index diagnostic")
        self.write("runtime/zvec-home/config.json", "credential")
        self.write("source/data.jsonl", "gold data")
        self.write("workspaces/a/doc.md", "corpus")
        credential = self.root / "auth.json"
        credential.write_text('{"openai":{"key":"credential"}}')
        profile = self.artifacts / "runs/run-1/profiles/baseline/opencode-home"
        profile.mkdir(parents=True)
        (profile / "auth.json").symlink_to(credential)
        summary = self.root / "step-summary.md"
        os.environ.update(
            GITHUB_STEP_SUMMARY=str(summary), BROWSE_CANDIDATE_COMMIT="candidate-sha"
        )
        self.export()
        self.assertEqual(
            (self.output / "runs/run-1/report/summary.md").read_text(),
            "# Partial report\n",
        )
        self.assertTrue((self.output / "runs/run-1/evaluation/summary.json").is_file())
        self.assertTrue((self.output / "logs/index.stderr.log").is_file())
        for name in ("profiles", "auth.json", "runtime", "source", "workspaces"):
            self.assertFalse(any(path.name == name for path in self.output.rglob("*")))
        self.assertIn("candidate-sha", summary.read_text())
        self.assertIn("Partial report", summary.read_text())

    def test_secret_leak_fails_closed_and_does_not_publish_summary(self):
        os.environ["ZVEC_GREP_API_KEY"] = "secret-from-provider"
        summary = self.root / "summary.md"
        os.environ["GITHUB_STEP_SUMMARY"] = str(summary)
        self.write("runs/run-1/cases/628/events.jsonl", "secret-from-provider")
        with self.assertRaisesRegex(ValueError, "credential material"):
            self.export()
        self.assertFalse(self.output.exists())
        self.assertFalse(summary.exists())

    def test_model_key_and_json_escaped_credentials_are_scanned(self):
        os.environ["OPENAI_API_KEY"] = 'model-secret-with-"quote'
        escaped = json.dumps(os.environ["OPENAI_API_KEY"])[1:-1]
        self.assertIn(escaped.encode(), secret_values(self.home))
        self.write("runs/run-1/cases/628/events.jsonl", escaped)
        with self.assertRaisesRegex(ValueError, "credential material"):
            self.export()

    def test_secret_matching_crosses_read_boundaries(self):
        path = self.write("large.log", "x" * (1024 * 1024 - 3) + "my-secret")
        self.assertTrue(_contains_secret(path, {b"my-secret"}))
        self.assertFalse(_contains_secret(path, {b"absent"}))

    @patch("zg_bench.ci.resolve_executable", return_value=Path("/test/opencode"))
    @patch("zg_bench.ci.prepare_judge_home")
    @patch("zg_bench.opencode.run_prompt")
    def test_model_preflight_preserves_configured_model(self, run, prepare, resolve):
        prepare.side_effect = lambda config, artifacts, home: home
        run.return_value = self.preflight_result()
        verify_model()
        self.assertEqual(run.call_args.kwargs["model"], load_config().run.model)
        self.assertEqual(run.call_args.kwargs["idle_timeout_seconds"], 180)
        self.assertEqual(run.call_args.kwargs["timeout_seconds"], 180)
        self.assertEqual(run.call_args.args[0], resolve.return_value)
        self.assertNotEqual(run.call_args.kwargs["home"], self.home)
        self.assertFalse(run.call_args.kwargs["home"].exists())

    @staticmethod
    def preflight_result(*, errors=()):
        return {
            "exit_code": 0,
            "idle_killed": False,
            "trace": TraceSummary(
                "test-session", "OK", "OK", True, Usage(), (), (), errors
            ),
        }

    @patch("zg_bench.ci.resolve_executable", return_value=Path("/test/opencode"))
    @patch("zg_bench.ci.prepare_judge_home")
    @patch("zg_bench.opencode.run_prompt")
    def test_model_preflight_failure_does_not_echo_provider_output(
        self, run, prepare, resolve
    ):
        run.return_value = self.preflight_result(
            errors=("unexpected credential in provider error",)
        )
        with self.assertRaisesRegex(ValueError, "verify credential access") as error:
            verify_model()
        self.assertNotIn("unexpected credential", str(error.exception))

    def test_symlinked_evidence_is_rejected(self):
        path = self.write("runs/run-1/cases/628/response.md", "answer")
        path.unlink()
        path.symlink_to(self.manifest)
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.export()
        self.assertFalse(self.output.exists())

    def test_auth_file_in_an_allowed_directory_is_rejected(self):
        self.write("runs/run-1/cases/628/auth.json", "unknown credentials")
        with self.assertRaisesRegex(ValueError, "credential material"):
            self.export()

    def test_prepare_failure_still_exports_diagnostics(self):
        self.write("logs/index.stderr.log", "failed to build index")
        summary = self.root / "summary.md"
        os.environ["GITHUB_STEP_SUMMARY"] = str(summary)
        self.export()
        self.assertIn("No paired report", summary.read_text())
        self.assertTrue((self.output / "logs/index.stderr.log").is_file())

    def test_ci_and_smoke_preserve_the_existing_protocol(self):
        config = load_config()
        self.assertEqual(config.run.trials_per_case, 3)
        queries = [
            {"query_id": value} for value in ("628", "700", "966", "827", "241", "770")
        ]
        self.assertEqual(
            load_suite("ci", queries).query_ids, ("628", "700", "966", "827", "241")
        )
        self.assertEqual(load_suite("smoke", queries).query_ids, ("770",))


if __name__ == "__main__":
    unittest.main()
