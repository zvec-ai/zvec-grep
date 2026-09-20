from __future__ import annotations

import hashlib
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from harbor.agents.installed.claude_code import ClaudeCode

from zg_bench import doctor, runner
from zg_bench.agents.zvec_claude_code import ZvecClaudeCode
from zg_bench.agents.zvec_grep import ZvecGrepMixin

_FIXTURE_SUITE_NAME = "external-suite-fixture"
_FIXTURE_DATASET = "fixture/external-dataset@1"
_FIXTURE_TASK = "fixture/task-one"
_FIXTURE_SUITE_YAML = f"""\
name: {_FIXTURE_SUITE_NAME}
dataset: {_FIXTURE_DATASET}
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
            name=_FIXTURE_SUITE_NAME,
            dataset=_FIXTURE_DATASET,
            tier="smoke",
            tasks=(_FIXTURE_TASK,),
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

    def test_claude_code_local_package_is_bound_into_zvec_profile(self) -> None:
        digest = "d" * 64
        suite = runner.BenchmarkSuite(
            name=_FIXTURE_SUITE_NAME,
            dataset=_FIXTURE_DATASET,
            tier="smoke",
            tasks=(_FIXTURE_TASK,),
        )

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
                    "claude-code",
                    "zvec-grep",
                    zvec_grep_package=str(source_dir),
                )
                command = runner.build_harbor_command(
                    suite,
                    profile="zvec-grep",
                    agent="claude-code",
                    model="claude-opus-5",
                    job_name="claude-local-package",
                    zvec_grep_package=prepared.zvec_grep_package or "",
                    zvec_grep_package_sha256=prepared.zvec_grep_package_sha256,
                )

            overlay = json.loads(prepared.compose_path.read_text())
            package_mount = next(
                mount
                for mount in overlay["services"]["main"]["volumes"]
                if mount.get("target") == runner.LOCAL_ZVEC_GREP_PACKAGE_TARGET
            )
            self.assertEqual(
                package_mount,
                {
                    "type": "bind",
                    "source": str(package),
                    "target": runner.LOCAL_ZVEC_GREP_PACKAGE_TARGET,
                    "read_only": True,
                },
            )
            self.assertIn(
                f"zvec_grep_package={runner.LOCAL_ZVEC_GREP_PACKAGE_TARGET}",
                command,
            )
            self.assertIn(f"zvec_grep_package_sha256={digest}", command)
            self.assertIn("--extra-docker-compose", command)
            self.assertIn("claude-code-2.1.212", json.dumps(overlay))

    def test_claude_profiles_cache_nvm_and_local_install_separately(self) -> None:
        inspected = subprocess.CompletedProcess([], 0, stdout="", stderr="")

        with tempfile.TemporaryDirectory() as temp_dir:
            cache_dir = Path(temp_dir) / "agent-setup"
            overlays: dict[str, dict[str, object]] = {}
            with (
                patch.object(runner, "SETUP_CACHE_DIR", cache_dir),
                patch.object(runner.subprocess, "run", return_value=inspected),
            ):
                for profile in runner.PROFILES:
                    prepared = runner.prepare_setup_cache(
                        "claude-code",
                        profile,
                    )
                    overlays[profile] = json.loads(prepared.compose_path.read_text())

        local_volume_names: list[str] = []
        for profile, overlay in overlays.items():
            service_volumes = overlay["services"]["main"]["volumes"]
            self.assertIn(
                {
                    "type": "volume",
                    "source": "agent-setup-cache",
                    "target": "/root/.nvm",
                },
                service_volumes,
            )
            self.assertIn(
                {
                    "type": "volume",
                    "source": runner._CLAUDE_CODE_INSTALL_CACHE_SOURCE,
                    "target": "/root/.local",
                },
                service_volumes,
            )
            local_volume = overlay["volumes"][
                runner._CLAUDE_CODE_INSTALL_CACHE_SOURCE
            ]
            self.assertTrue(local_volume["external"])
            self.assertEqual(
                local_volume["name"],
                runner.claude_code_install_cache_volume_name(profile),
            )
            local_volume_names.append(local_volume["name"])

        self.assertEqual(len(set(local_volume_names)), len(runner.PROFILES))

    def test_non_claude_setup_cache_does_not_mount_local_install_volume(self) -> None:
        inspected = subprocess.CompletedProcess([], 0, stdout="", stderr="")

        with tempfile.TemporaryDirectory() as temp_dir:
            with (
                patch.object(runner, "SETUP_CACHE_DIR", Path(temp_dir)),
                patch.object(runner.subprocess, "run", return_value=inspected),
            ):
                prepared = runner.prepare_setup_cache("opencode", "baseline")

            overlay = json.loads(prepared.compose_path.read_text())

        self.assertEqual(
            overlay["services"]["main"]["volumes"],
            [
                {
                    "type": "volume",
                    "source": "agent-setup-cache",
                    "target": "/root/.nvm",
                }
            ],
        )
        self.assertNotIn(
            runner._CLAUDE_CODE_INSTALL_CACHE_SOURCE,
            overlay["volumes"],
        )

    def test_claude_install_cache_creation_error_names_failed_volume(self) -> None:
        profile = "baseline"
        nvm_volume = runner.setup_cache_volume_name("claude-code", profile)
        local_volume = runner.claude_code_install_cache_volume_name(profile)

        def fake_run(
            command: list[str], **kwargs: object
        ) -> subprocess.CompletedProcess[str]:
            if command == ["docker", "volume", "inspect", nvm_volume]:
                return subprocess.CompletedProcess(command, 0, stdout="", stderr="")
            if command == ["docker", "volume", "inspect", local_volume]:
                return subprocess.CompletedProcess(
                    command,
                    1,
                    stdout="",
                    stderr="no such volume",
                )
            if command == ["docker", "volume", "create", local_volume]:
                return subprocess.CompletedProcess(
                    command,
                    1,
                    stdout="",
                    stderr="permission denied",
                )
            raise AssertionError(f"unexpected Docker command: {command}")

        with tempfile.TemporaryDirectory() as temp_dir, patch.object(
            runner,
            "SETUP_CACHE_DIR",
            Path(temp_dir),
        ), patch.object(runner.subprocess, "run", side_effect=fake_run):
            with self.assertRaisesRegex(
                RuntimeError,
                "Claude Code install cache Docker volume",
            ) as error:
                runner.prepare_setup_cache("claude-code", profile)

        self.assertIn(local_volume, str(error.exception))
        self.assertIn("no such volume", str(error.exception))
        self.assertIn("permission denied", str(error.exception))


class ClaudeMcpInstallTests(unittest.IsolatedAsyncioTestCase):
    async def test_zg_install_uses_harbor_claude_config_directory(self) -> None:
        agent = object.__new__(ZvecClaudeCode)
        agent._mcp_target = "claude-code"
        calls: list[tuple[str, dict[str, object]]] = []

        async def fake_exec(
            environment: object,
            command: str,
            **kwargs: object,
        ) -> subprocess.CompletedProcess[str]:
            calls.append((command, kwargs))
            return subprocess.CompletedProcess([], 0, stdout="", stderr="")

        agent.exec_as_agent = fake_exec  # type: ignore[method-assign]
        await agent._setup_mcp(
            object(),
            "/workspace",
            {},
            supports_ready_check=True,
        )

        install_command, install_kwargs = calls[0]
        self.assertEqual(
            install_command,
            "zg --install --target claude-code --yes",
        )
        self.assertEqual(install_kwargs["cwd"], "/workspace")
        self.assertEqual(
            install_kwargs["env"],
            {"CLAUDE_CONFIG_DIR": "/logs/agent/sessions"},
        )


class RunValidationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.suite_name = _install_external_suite_fixture(self)

    def test_harbor_command_forwards_independent_trial_count(self) -> None:
        suite = runner.load_suite(self.suite_name, tier="smoke")

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
        suite = runner.load_suite(self.suite_name, tier="smoke")

        with self.assertRaisesRegex(ValueError, "positive integer"):
            runner.build_harbor_command(
                suite,
                profile="baseline",
                agent="opencode",
                model="custom-openai/glm-5.2",
                job_name="invalid-trials",
                n_attempts=0,
            )

    def test_claude_code_baseline_uses_published_configuration(self) -> None:
        suite = runner.load_suite(self.suite_name, tier="smoke")

        with patch.dict(
            runner.os.environ,
            {"ANTHROPIC_API_KEY": "anthropic-secret"},
            clear=True,
        ):
            command = runner.build_harbor_command(
                suite,
                profile="baseline",
                agent="claude-code",
                model="claude-opus-5",
                job_name="claude-code-baseline",
            )

        self.assertEqual(command[command.index("--agent") + 1], "claude-code")
        self.assertEqual(command[command.index("--model") + 1], "claude-opus-5")
        self.assertIn("version=2.1.212", command)
        self.assertIn("reasoning_effort=high", command)
        self.assertIn("max_budget_usd=4.0", command)
        self.assertIn("ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY}", command)
        self.assertNotIn("mcp_target=claude-code", command)

    def test_claude_code_zvec_profile_registers_mcp_and_qwen_embedding(
        self,
    ) -> None:
        suite = runner.load_suite(self.suite_name, tier="smoke")

        with patch.dict(
            runner.os.environ,
            {"CLAUDE_CODE_OAUTH_TOKEN": "oauth-secret"},
            clear=True,
        ):
            endpoint = "https://embedding.example/v1/embeddings"
            command = runner.build_harbor_command(
                suite,
                profile="zvec-grep",
                agent="claude-code",
                model="claude-opus-5",
                job_name="claude-code-zvec",
                embedding_endpoint=endpoint,
            )

        self.assertEqual(
            command[command.index("--agent") + 1],
            runner.ZVEC_CLAUDE_CODE_IMPORT_PATH,
        )
        self.assertIn("version=2.1.212", command)
        self.assertIn("reasoning_effort=high", command)
        self.assertIn("max_budget_usd=4.0", command)
        self.assertIn("mcp_target=claude-code", command)
        self.assertIn(
            "embedding_model=qwen/qwen3.7-text-embedding",
            command,
        )
        self.assertIn(f"embedding_endpoint={endpoint}", command)
        self.assertIn(
            "CLAUDE_CODE_OAUTH_TOKEN=${CLAUDE_CODE_OAUTH_TOKEN}",
            command,
        )

    def test_claude_code_accepts_api_and_oauth_credentials(self) -> None:
        credential_names = (
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
        )
        for credential_name in credential_names:
            with self.subTest(credential=credential_name), patch.dict(
                runner.os.environ,
                {credential_name: "secret"},
                clear=True,
            ):
                runner.validate_profile_credentials(
                    ("baseline",),
                    agent="claude-code",
                    model="claude-opus-5",
                )

    def test_claude_code_requires_api_or_oauth_credentials(self) -> None:
        with patch.dict(runner.os.environ, {}, clear=True), self.assertRaisesRegex(
            ValueError,
            "Anthropic API or OAuth credentials",
        ):
            runner.validate_profile_credentials(
                ("baseline",),
                agent="claude-code",
                model="claude-opus-5",
            )

    def test_doctor_forwards_embedding_endpoint_to_credential_validation(
        self,
    ) -> None:
        endpoint = "https://embedding.example/v1/embeddings"

        with patch.object(doctor, "validate_profile_credentials") as validate:
            doctor._collect_run_checks(
                agent="claude-code",
                model="claude-opus-5",
                profiles=("zvec-grep",),
                zvec_grep_package=runner.ZVEC_GREP_PACKAGE,
                embedding_model=runner.ZVEC_GREP_EMBEDDING,
                embedding_endpoint=endpoint,
            )

        self.assertEqual(validate.call_args.kwargs["embedding_endpoint"], endpoint)

    def test_custom_glm_uses_openai_compatible_provider(self) -> None:
        suite = runner.load_suite(self.suite_name, tier="smoke")

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

    def test_qwen_code_agent_is_not_supported(self) -> None:
        with self.assertRaisesRegex(ValueError, "unsupported agent 'qwen-coder'"):
            runner.resolve_agent_model("qwen-coder", "qwen3.7-max")

    def test_opencode_qwen_uses_dashscope_provider(self) -> None:
        suite = runner.load_suite(self.suite_name, tier="smoke")

        command = runner.build_harbor_command(
            suite,
            profile="baseline",
            agent="opencode",
            model="qwen3.7-max",
            job_name="opencode-qwen-test",
        )

        agent_index = command.index("--agent")
        model_index = command.index("--model")
        self.assertEqual(command[agent_index + 1], runner.OPENCODE_IMPORT_PATH)
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
        self.assertEqual(environment["OPENAI_API_KEY"], "opencode-qwen-secret")
        self.assertEqual(
            environment["OPENAI_BASE_URL"], runner.OPENCODE_DASHSCOPE_BASE_URL
        )

    def test_opencode_zvec_profile_keeps_mcp_in_native_config(self) -> None:
        suite = runner.load_suite(self.suite_name, tier="smoke")

        command = runner.build_harbor_command(
            suite,
            profile="zvec-grep",
            agent="opencode",
            model="aliyun-glm-5.2",
            job_name="opencode-native-zvec-test",
        )

        agent_index = command.index("--agent")
        self.assertEqual(command[agent_index + 1], runner.ZVEC_OPENCODE_IMPORT_PATH)
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
        for agent in ("claude-code", "codex", "opencode"):
            with self.subTest(agent=agent), self.assertRaisesRegex(
                ValueError, "Workspace Remote Embedding authorization"
            ):
                runner.validate_zvec_grep_package_compatibility(
                    ("zvec-grep",),
                    agent=agent,
                    zvec_grep_package="0.1.5",
                )

    def test_zvec_claude_code_combines_mixin_with_harbor_adapter(self) -> None:
        self.assertTrue(issubclass(ZvecClaudeCode, ZvecGrepMixin))
        self.assertTrue(issubclass(ZvecClaudeCode, ClaudeCode))

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
                runner.validate_job_destinations(jobs_dir, (("baseline", "existing"),))


class SuiteTierTests(unittest.TestCase):
    def setUp(self) -> None:
        self.suite_name = _install_external_suite_fixture(self)

    def test_local_swe_qa_smoke_uses_configured_tasks(self) -> None:
        suite_path = (
            Path(__file__).resolve().parents[1] / "suites" / "swe-qa-bench.yaml"
        )

        suite = runner.load_suite(suite_path, tier="smoke")

        self.assertEqual(
            suite.tasks,
            (
                "reflex-6",
                "pylint-9",
                "matplotlib-37",
                "streamlink-14",
                "xarray-32",
            ),
        )

    def test_local_swe_qa_suite_uses_harbor_path(self) -> None:
        suite_path = (
            Path(__file__).resolve().parents[1] / "suites" / "swe-qa-bench.yaml"
        )
        suite = runner.load_suite(suite_path, tier="full")

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
        suite = runner.load_suite(self.suite_name, tier="full")

        command = runner.build_harbor_command(
            suite,
            profile="baseline",
            agent="opencode",
            model="aliyun-glm-5.2",
            job_name="full-test",
        )

        self.assertEqual(suite.dataset, _FIXTURE_DATASET)
        self.assertIsNone(suite.path)
        self.assertIsNone(suite.tasks)
        self.assertEqual(
            command[command.index("--dataset") + 1],
            _FIXTURE_DATASET,
        )
        self.assertNotIn("--include-task-name", command)

    def test_task_overrides_are_forwarded_as_repeatable_filters(self) -> None:
        suite = runner.load_suite(
            self.suite_name,
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
            runner.load_suite(self.suite_name, tier="ci")


if __name__ == "__main__":
    unittest.main()
