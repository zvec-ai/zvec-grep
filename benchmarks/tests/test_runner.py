from __future__ import annotations

import hashlib
import io
import json
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, patch

from zg_bench import runner


def _write_uv_archive(path: Path) -> None:
    with tarfile.open(path, "w:gz") as archive:
        for name in ("uv", "uvx"):
            contents = f"{name} binary".encode()
            info = tarfile.TarInfo(f"uv-x86_64-unknown-linux-gnu/{name}")
            info.size = len(contents)
            info.mode = 0o755
            archive.addfile(info, io.BytesIO(contents))


class UvArchiveTests(unittest.TestCase):
    def test_patches_swebench_dockerfile_to_install_local_uv_archive(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            task_dir = Path(temp_dir) / "task"
            environment_dir = task_dir / "environment"
            environment_dir.mkdir(parents=True)
            dockerfile = environment_dir / "Dockerfile"
            dockerfile.write_text(
                "FROM example\n"
                "WORKDIR /testbed\n"
                "RUN curl -LsSf https://astral.sh/uv/0.7.13/install.sh | sh\n"
                "RUN mkdir -p /logs\n"
            )
            archive = Path(temp_dir) / "uv.tar.gz"
            _write_uv_archive(archive)

            patched = runner.patch_task_uv_install(task_dir, archive)

            self.assertTrue(patched)
            contents = dockerfile.read_text()
            self.assertNotIn("curl -LsSf https://astral.sh/uv", contents)
            self.assertIn("COPY zg-bench-uv.tar.gz", contents)
            self.assertIn("--no-same-owner", contents)
            self.assertIn("/usr/local/bin/uv", contents)
            self.assertEqual(
                (environment_dir / "zg-bench-uv.tar.gz").read_bytes(),
                archive.read_bytes(),
            )

    def test_rejects_archive_without_uvx(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            archive = Path(temp_dir) / "uv.tar.gz"
            with tarfile.open(archive, "w:gz") as contents:
                data = b"uv binary"
                info = tarfile.TarInfo("uv-x86_64-unknown-linux-gnu/uv")
                info.size = len(data)
                contents.addfile(info, io.BytesIO(data))

            with self.assertRaisesRegex(ValueError, "uvx"):
                runner.validate_uv_archive(archive)

    def test_prepares_isolated_local_dataset_without_modifying_harbor_cache(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            harbor_task = root / "harbor-cache" / "digest"
            environment_dir = harbor_task / "environment"
            environment_dir.mkdir(parents=True)
            original = (
                "FROM example\n"
                "RUN curl -LsSf https://astral.sh/uv/0.7.13/install.sh | sh\n"
            )
            (environment_dir / "Dockerfile").write_text(original)
            archive = root / "uv.tar.gz"
            _write_uv_archive(archive)
            suite = runner.BenchmarkSuite(
                name="suite",
                dataset="swe-bench/suite@2",
                tier="smoke",
                tasks=("swe-bench/example",),
            )

            with (
                patch.object(runner, "LOCAL_UV_TASKS_DIR", root / "local-tasks"),
                patch.object(
                    runner,
                    "_download_suite_task_paths",
                    new=AsyncMock(
                        return_value=(("swe-bench/example", harbor_task),)
                    ),
                ),
            ):
                prepared = runner.prepare_suite_uv_archive(suite, archive)

            self.assertEqual(
                (environment_dir / "Dockerfile").read_text(),
                original,
            )
            local_dockerfile = (
                prepared.dataset_path / "example" / "environment" / "Dockerfile"
            )
            self.assertIn("COPY zg-bench-uv.tar.gz", local_dockerfile.read_text())
            self.assertEqual(prepared.task_count, 1)

    def test_harbor_command_uses_prepared_local_dataset(self) -> None:
        suite = runner.BenchmarkSuite(
            name="suite",
            dataset="swe-bench/suite@2",
            tier="smoke",
            tasks=("swe-bench/example",),
        )

        command = runner.build_harbor_command(
            suite,
            profile="baseline",
            agent="opencode",
            model="qwen3.7-max",
            job_name="local-uv-test",
            task_dataset_path=Path("/tmp/local-swebench"),
        )

        self.assertIn("--path", command)
        self.assertIn("/tmp/local-swebench", command)
        self.assertNotIn("--dataset", command)
        self.assertNotIn("--include-task-name", command)


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
            ):
                prepared = runner.prepare_setup_cache(
                    "opencode",
                    "zvec-grep",
                    zvec_grep_package=str(source_dir),
                )

            overlay = json.loads(prepared.compose_path.read_text())
            self.assertEqual(
                overlay["services"]["main"]["environment"]["PIP_INDEX_URL"],
                runner.PIP_INDEX_URL,
            )
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
            self.assertEqual(
                prepared.zvec_grep_package,
                runner.LOCAL_ZVEC_GREP_PACKAGE_TARGET,
            )
            self.assertEqual(prepared.zvec_grep_package_sha256, digest)

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
        self.assertEqual(command[agent_index + 1], runner.OPENCODE_ACP_IMPORT_PATH)
        self.assertEqual(command[model_index + 1], "dashscope/qwen3.7-max")

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

    def test_django_focused_full_tier_selects_curated_tasks(self) -> None:
        suite = runner.load_suite("django-focused", tier="full")

        self.assertEqual(
            suite.tasks,
            (
                "swe-bench/django__django-11734",
                "swe-bench/django__django-11885",
                "swe-bench/django__django-14631",
                "swe-bench/django__django-16560",
                "swe-bench/django__django-13344",
            ),
        )

    def test_unconfigured_ci_tier_has_actionable_error(self) -> None:
        with self.assertRaisesRegex(runner.SuiteConfigError, "available: smoke, full"):
            runner.load_suite("swebench-verified", tier="ci")


if __name__ == "__main__":
    unittest.main()
