from __future__ import annotations

import hashlib
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from zg_bench import runner


class LocalPackageTests(unittest.TestCase):
    def test_packs_current_checkout_and_names_artifact_by_digest(self) -> None:
        package_contents = b"local zvec-grep package"

        with tempfile.TemporaryDirectory() as temp_dir:
            cache_dir = Path(temp_dir) / "cache"
            source_dir = Path(temp_dir) / "source"
            (source_dir / "node_modules" / ".bin").mkdir(parents=True)
            (source_dir / "package.json").write_text("{}")
            (source_dir / "node_modules" / ".bin" / "tsc").write_text("")

            def fake_run(
                command: list[str], **kwargs: object
            ) -> subprocess.CompletedProcess[str]:
                self.assertEqual(command[:2], ["npm", "pack"])
                self.assertIn("npm_config_cache", kwargs["env"])
                pack_dir = Path(command[-1])
                (pack_dir / "zvec-zvec-grep-0.1.5.tgz").write_bytes(package_contents)
                return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

            with (
                patch.object(runner, "LOCAL_PACKAGE_DIR", cache_dir),
                patch.object(runner, "LOCAL_NPM_CACHE_DIR", cache_dir / "npm-cache"),
                patch.object(runner.shutil, "which", return_value="/usr/bin/npm"),
                patch.object(runner.subprocess, "run", side_effect=fake_run),
            ):
                package, digest = runner.prepare_local_zvec_grep_package(source_dir)

            expected_digest = hashlib.sha256(package_contents).hexdigest()
            self.assertEqual(digest, expected_digest)
            self.assertEqual(
                package,
                (cache_dir / f"zvec-grep-{expected_digest[:16]}.tgz").resolve(),
            )
            self.assertEqual(package.read_bytes(), package_contents)

    def test_zvec_profile_mounts_local_package_and_keys_volume_by_hash(self) -> None:
        digest = "b" * 64

        with tempfile.TemporaryDirectory() as temp_dir:
            cache_dir = Path(temp_dir) / "agent-setup"
            source_dir = Path(temp_dir) / "source"
            source_dir.mkdir()
            package = Path(temp_dir) / "zvec-grep.tgz"
            package.write_bytes(b"package")
            inspected = subprocess.CompletedProcess([], 0, stdout="", stderr="")

            with (
                patch.object(runner, "SETUP_CACHE_DIR", cache_dir),
                patch.object(
                    runner,
                    "prepare_local_zvec_grep_package",
                    return_value=(package, digest),
                ),
                patch.object(runner.subprocess, "run", return_value=inspected),
                patch.dict(
                    runner.os.environ,
                    {runner.ZVEC_GREP_INDEX_SEED_ENV: ""},
                ),
            ):
                prepared = runner.prepare_setup_cache(
                    "opencode",
                    "zvec-grep",
                    zvec_grep_package=str(source_dir),
                )

            overlay = json.loads(prepared.compose_path.read_text())
            service_volumes = overlay["services"]["main"]["volumes"]
            self.assertEqual(
                service_volumes[1],
                {
                    "type": "bind",
                    "source": str(package),
                    "target": runner.LOCAL_ZVEC_GREP_PACKAGE_TARGET,
                    "read_only": True,
                },
            )
            self.assertIn(
                f"local-{digest[:16]}",
                overlay["volumes"]["agent-setup-cache"]["name"],
            )
            self.assertIn(
                "local-potion-code-16m-v2",
                runner.setup_cache_volume_name(
                    "opencode",
                    "zvec-grep",
                    zvec_grep_package_sha256=digest,
                    embedding_model="local/potion-code-16m-v2",
                ),
            )
            self.assertEqual(
                prepared.zvec_grep_package,
                runner.LOCAL_ZVEC_GREP_PACKAGE_TARGET,
            )
            self.assertEqual(prepared.zvec_grep_package_sha256, digest)

    def test_index_seed_mount_is_disabled_for_baseline_and_remote_embedding(
        self,
    ) -> None:
        inspected = subprocess.CompletedProcess([], 0, stdout="", stderr="")

        with tempfile.TemporaryDirectory() as temp_dir:
            cache_dir = Path(temp_dir) / "agent-setup"
            seed_dir = Path(temp_dir) / "index-seed"

            with (
                patch.object(runner, "SETUP_CACHE_DIR", cache_dir),
                patch.object(runner.subprocess, "run", return_value=inspected),
                patch.dict(
                    runner.os.environ,
                    {runner.ZVEC_GREP_INDEX_SEED_ENV: str(seed_dir)},
                ),
            ):
                baseline = runner.prepare_setup_cache(
                    "opencode",
                    "baseline",
                    embedding_model="local/potion-code-16m-v2",
                )
                baseline_overlay = json.loads(baseline.compose_path.read_text())
                remote = runner.prepare_setup_cache(
                    "opencode",
                    "zvec-grep",
                    embedding_model="qwen/text-embedding-v4",
                )
                remote_overlay = json.loads(remote.compose_path.read_text())

            for overlay in (baseline_overlay, remote_overlay):
                self.assertNotIn(str(seed_dir.resolve()), json.dumps(overlay))
            self.assertFalse(seed_dir.exists())

    def test_local_zvec_profile_creates_host_seed_without_mounting_it(
        self,
    ) -> None:
        inspected = subprocess.CompletedProcess([], 0, stdout="", stderr="")

        with tempfile.TemporaryDirectory() as temp_dir:
            cache_dir = Path(temp_dir) / "agent-setup"
            seed_dir = Path(temp_dir) / "nested" / "index-seed"

            with (
                patch.object(runner, "SETUP_CACHE_DIR", cache_dir),
                patch.object(runner.subprocess, "run", return_value=inspected),
                patch.dict(
                    runner.os.environ,
                    {runner.ZVEC_GREP_INDEX_SEED_ENV: str(seed_dir)},
                ),
            ):
                prepared = runner.prepare_setup_cache(
                    "opencode",
                    "zvec-grep",
                    embedding_model="local/potion-code-16m-v2",
                )

            overlay = json.loads(prepared.compose_path.read_text())
            self.assertTrue(seed_dir.is_dir())
            self.assertEqual(
                overlay["services"]["main"]["volumes"],
                [
                    {
                        "type": "volume",
                        "source": "agent-setup-cache",
                        "target": runner._SETUP_CACHE_TARGET,
                    }
                ],
            )
            self.assertNotIn(str(seed_dir.resolve()), json.dumps(overlay))

    def test_index_seed_rejects_broad_host_directories(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            github_workspace = Path(temp_dir) / "github-workspace"
            github_workspace.mkdir()

            with patch.dict(
                runner.os.environ,
                {"GITHUB_WORKSPACE": str(github_workspace)},
            ):
                forbidden = (
                    Path("/"),
                    Path.home(),
                    Path.cwd(),
                    github_workspace,
                )
                for path in forbidden:
                    with self.subTest(path=path), self.assertRaisesRegex(
                        ValueError,
                        "dedicated cache directory",
                    ):
                        runner.resolve_zvec_grep_index_seed_dir(str(path))

    def test_version_shorthand_selects_published_npm_package(self) -> None:
        prepared = runner.prepare_zvec_grep_package("0.1.5")

        self.assertEqual(prepared.install_spec, "@zvec/zvec-grep@0.1.5")
        self.assertIsNone(prepared.bind_source)
        self.assertIsNone(prepared.sha256)

    def test_harbor_command_installs_mounted_package_and_records_hash(self) -> None:
        digest = "c" * 64
        suite = runner.BenchmarkSuite(
            name="swebench-verified",
            dataset="swe-bench/swe-bench-verified@2",
            tier="smoke",
            tasks=("swe-bench/pallets__flask-5014",),
        )

        command = runner.build_harbor_command(
            suite,
            profile="zvec-grep",
            agent="opencode",
            model="aliyun-glm-5.2",
            job_name="local-package-test",
            zvec_grep_package=runner.LOCAL_ZVEC_GREP_PACKAGE_TARGET,
            zvec_grep_package_sha256=digest,
        )

        self.assertIn(
            f"zvec_grep_package={runner.LOCAL_ZVEC_GREP_PACKAGE_TARGET}", command
        )
        self.assertIn(f"zvec_grep_package_sha256={digest}", command)


class RunValidationTests(unittest.TestCase):
    def test_harbor_command_forwards_independent_trial_count(self) -> None:
        suite = runner.load_suite("swe-qa-bench-manual", tier="ci")

        command = runner.build_harbor_command(
            suite,
            profile="baseline",
            agent="opencode",
            model="custom-openai/glm-5.2",
            job_name="three-trials",
            n_attempts=3,
        )

        self.assertEqual(command[command.index("--n-attempts") + 1], "3")

    def test_harbor_command_rejects_non_positive_trial_count(self) -> None:
        suite = runner.load_suite("swe-qa-bench-manual", tier="ci")

        with self.assertRaisesRegex(ValueError, "positive integer"):
            runner.build_harbor_command(
                suite,
                profile="baseline",
                agent="opencode",
                model="custom-openai/glm-5.2",
                job_name="invalid-trials",
                n_attempts=0,
            )

    def test_custom_glm_uses_openai_compatible_provider(self) -> None:
        suite = runner.load_suite("swebench-verified", tier="smoke")

        command = runner.build_harbor_command(
            suite,
            profile="zvec-grep",
            agent="opencode",
            model="custom-openai/glm-5.2",
            embedding_model="local/potion-code-16m-v2",
            job_name="custom-glm-test",
        )

        self.assertEqual(
            command[command.index("--model") + 1],
            "custom-openai/glm-5.2",
        )
        self.assertIn(
            "embedding_model=local/potion-code-16m-v2",
            command,
        )
        config_argument = next(
            value for value in command if value.startswith("opencode_config=")
        )
        config = json.loads(config_argument.removeprefix("opencode_config="))
        provider = config["provider"]["custom-openai"]
        self.assertEqual(
            provider["options"]["apiKey"],
            "{env:OPENAI_API_KEY}",
        )
        self.assertNotIn("GLM_API_KEY", json.dumps(config))
        self.assertIn("mcp", config)

    def test_custom_glm_environment_normalizes_and_scrubs_source_key(self) -> None:
        with patch.dict(
            runner.os.environ,
            {"GLM_API_KEY": "glm-secret", "UNRELATED": "kept"},
            clear=True,
        ):
            environment = runner.execution_environment(
                agent="opencode",
                model="custom-openai/glm-5.2",
            )

        self.assertEqual(environment["OPENAI_API_KEY"], "glm-secret")
        self.assertNotIn("GLM_API_KEY", environment)
        self.assertEqual(environment["UNRELATED"], "kept")

    def test_local_embedding_does_not_require_embedding_key(self) -> None:
        with patch.dict(
            runner.os.environ,
            {"GLM_API_KEY": "glm-secret"},
            clear=True,
        ):
            runner.validate_profile_credentials(
                ("baseline", "zvec-grep"),
                agent="opencode",
                model="custom-openai/glm-5.2",
                embedding_model="local/potion-code-16m-v2",
            )

    def test_qwen_code_model_is_supported(self) -> None:
        support = runner.resolve_agent_model("qwen-coder", "qwen3.7-max")

        self.assertEqual(support.agent, "qwen-coder")
        self.assertEqual(support.model, "qwen3.7-max")

    def test_qwen_code_rejects_unconfigured_model(self) -> None:
        with self.assertRaisesRegex(ValueError, "supported models: qwen3.7-max"):
            runner.resolve_agent_model("qwen-coder", "qwen3.8")

    def test_qwen_code_uses_harbor_agent_name(self) -> None:
        suite = runner.load_suite("swebench-verified", tier="smoke")

        command = runner.build_harbor_command(
            suite,
            profile="baseline",
            agent="qwen-coder",
            model="qwen3.7-max",
            job_name="qwen-code-test",
        )

        agent_index = command.index("--agent")
        model_index = command.index("--model")
        self.assertEqual(command[agent_index + 1], "qwen-coder")
        self.assertEqual(command[model_index + 1], "qwen3.7-max")
        self.assertIn(
            f"base_url={runner.QWEN_CODE_DASHSCOPE_BASE_URL}", command
        )

    def test_qwen_code_zvec_profile_uses_custom_adapter(self) -> None:
        suite = runner.load_suite("swebench-verified", tier="smoke")

        command = runner.build_harbor_command(
            suite,
            profile="zvec-grep",
            agent="qwen-coder",
            model="qwen3.7-max",
            job_name="qwen-code-zvec-test",
        )

        agent_index = command.index("--agent")
        self.assertEqual(
            command[agent_index + 1], runner.ZVEC_QWEN_CODE_IMPORT_PATH
        )
        self.assertIn("--skill", command)

    def test_qwen_code_dashscope_key_is_forwarded_as_openai_key(self) -> None:
        with patch.dict(
            runner.os.environ,
            {"DASHSCOPE_API_KEY": "qwen-secret"},
            clear=True,
        ):
            environment = runner.execution_environment(
                agent="qwen-coder",
                model="qwen3.7-max",
            )

        self.assertEqual(environment["OPENAI_API_KEY"], "qwen-secret")

    def test_opencode_qwen_uses_dashscope_provider(self) -> None:
        suite = runner.load_suite("swebench-verified", tier="smoke")

        command = runner.build_harbor_command(
            suite,
            profile="baseline",
            agent="opencode",
            model="qwen3.7-max",
            job_name="opencode-qwen-test",
        )

        agent_index = command.index("--agent")
        model_index = command.index("--model")
        self.assertEqual(command[agent_index + 1], "opencode")
        self.assertEqual(command[model_index + 1], "dashscope/qwen3.7-max")
        self.assertIn(f"version={runner.OPENCODE_VERSION}", command)

        config_argument = next(
            value for value in command if value.startswith("opencode_config=")
        )
        config = json.loads(config_argument.removeprefix("opencode_config="))
        qwen = config["provider"]["dashscope"]["models"]["qwen3.7-max"]
        self.assertEqual(qwen["options"]["enable_thinking"], False)
        self.assertIn("OPENAI_API_KEY=${OPENAI_API_KEY}", command)

        with patch.dict(
            runner.os.environ,
            {"DASHSCOPE_API_KEY": "opencode-qwen-secret"},
            clear=True,
        ):
            environment = runner.execution_environment(
                agent="opencode",
                model="qwen3.7-max",
            )
        self.assertEqual(
            environment["OPENAI_API_KEY"], "opencode-qwen-secret"
        )
        self.assertEqual(
            environment["OPENAI_BASE_URL"], runner.OPENCODE_DASHSCOPE_BASE_URL
        )

    def test_opencode_zvec_profile_keeps_mcp_in_native_config(self) -> None:
        suite = runner.load_suite("swebench-verified", tier="smoke")

        command = runner.build_harbor_command(
            suite,
            profile="zvec-grep",
            agent="opencode",
            model="aliyun-glm-5.2",
            job_name="opencode-native-zvec-test",
        )

        agent_index = command.index("--agent")
        self.assertEqual(
            command[agent_index + 1], runner.ZVEC_OPENCODE_IMPORT_PATH
        )
        config_argument = next(
            value for value in command if value.startswith("opencode_config=")
        )
        config = json.loads(config_argument.removeprefix("opencode_config="))
        self.assertEqual(
            config["mcp"]["zvec_grep"],
            {
                "type": "remote",
                "url": "http://127.0.0.1:7999/mcp",
                "enabled": True,
                "timeout": 600_000,
                "oauth": False,
            },
        )

    def test_remote_auth_rejects_published_package_without_workspace_grants(
        self,
    ) -> None:
        for agent in ("codex", "opencode"):
            with self.subTest(agent=agent), self.assertRaisesRegex(
                ValueError, "Workspace Remote Embedding authorization"
            ):
                runner.validate_zvec_grep_package_compatibility(
                    ("zvec-grep",),
                    agent=agent,
                    zvec_grep_package="0.1.5",
                )

    def test_opencode_accepts_local_package_source(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            runner.validate_zvec_grep_package_compatibility(
                ("zvec-grep",),
                agent="opencode",
                zvec_grep_package=temp_dir,
            )

    def test_missing_local_package_is_rejected_before_docker(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            missing = Path(temp_dir) / "missing"
            with self.assertRaisesRegex(ValueError, "does not exist"):
                runner.validate_zvec_grep_package_compatibility(
                    ("zvec-grep",),
                    agent="opencode",
                    zvec_grep_package=str(missing),
                )

    def test_existing_job_directory_is_rejected_before_run(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            jobs_dir = Path(temp_dir)
            (jobs_dir / "existing").mkdir()
            with self.assertRaisesRegex(ValueError, "job output already exists"):
                runner.validate_job_destinations(
                    jobs_dir, (("baseline", "existing"),)
                )


class SuiteTierTests(unittest.TestCase):
    def test_local_swe_qa_suite_uses_harbor_path(self) -> None:
        suite = runner.load_suite("swe-qa-bench-manual", tier="ci")

        command = runner.build_harbor_command(
            suite,
            profile="baseline",
            agent="opencode",
            model="custom-openai/glm-5.2",
            job_name="local-suite-test",
        )

        self.assertIsNone(suite.dataset)
        self.assertIsNotNone(suite.path)
        selection_path = (
            Path(__file__).resolve().parents[1]
            / "zg_bench"
            / "swe_qa"
            / "data"
            / "selection.json"
        )
        selected_slugs = [
            task["task_slug"]
            for task in json.loads(selection_path.read_text())["tasks"]
        ]
        self.assertEqual(list(suite.tasks or ()), selected_slugs)
        self.assertIn("--path", command)
        self.assertNotIn("--dataset", command)

    def test_full_tier_runs_all_dataset_tasks(self) -> None:
        suite = runner.load_suite("swebench-verified", tier="full")

        command = runner.build_harbor_command(
            suite,
            profile="baseline",
            agent="opencode",
            model="aliyun-glm-5.2",
            job_name="full-test",
        )

        self.assertIsNone(suite.tasks)
        self.assertNotIn("--include-task-name", command)

    def test_task_overrides_are_forwarded_as_repeatable_filters(self) -> None:
        suite = runner.load_suite(
            "swebench-verified",
            tier="smoke",
            task_overrides=("org/task-one", "org/task-two"),
        )

        command = runner.build_harbor_command(
            suite,
            profile="baseline",
            agent="opencode",
            model="aliyun-glm-5.2",
            job_name="tasks-test",
        )

        filters = [
            command[index + 1]
            for index, value in enumerate(command)
            if value == "--include-task-name"
        ]
        self.assertEqual(filters, ["org/task-one", "org/task-two"])

    def test_unconfigured_ci_tier_has_actionable_error(self) -> None:
        with self.assertRaisesRegex(runner.SuiteConfigError, "available: smoke, full"):
            runner.load_suite("swebench-verified", tier="ci")


if __name__ == "__main__":
    unittest.main()
