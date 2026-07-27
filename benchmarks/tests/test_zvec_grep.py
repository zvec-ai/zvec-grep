from __future__ import annotations

import asyncio
import json
import tempfile
import unittest
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from zg_bench.agents.zvec_grep import ZvecGrepMixin


@dataclass
class _Result:
    stdout: str = ""
    stderr: str = ""
    return_code: int = 0


class _InstallHarness(ZvecGrepMixin):
    def __init__(self, package: str = "@zvec/zvec-grep@0.1.5") -> None:
        self._zvec_grep_package = package
        self._zvec_binding_package = "@zvec/bindings-linux-x64@0.5.0"
        self._zvec_grep_package_sha256: str | None = None
        self.agent_commands: list[str] = []
        self.root_commands: list[str] = []

    async def exec_as_agent(
        self, environment: Any, command: str, **kwargs: Any
    ) -> _Result:
        self.agent_commands.append(command)
        if command == "zg version -v":
            return _Result(stdout="0.1.5\n")
        return _Result(
            stdout=(
                "ZG_INSTALL_REUSED=0\n"
                "ZG_NODE_PATH=/root/.nvm/versions/node/v22/bin/node\n"
                "ZG_BIN_PATH=/root/.nvm/versions/node/v22/bin/zg\n"
            )
        )

    async def exec_as_root(
        self, environment: Any, command: str, **kwargs: Any
    ) -> _Result:
        self.root_commands.append(command)
        return _Result()

    async def _upload_embedding_config(self, environment: Any) -> None:
        return None


class _SetupParent:
    async def setup(self, environment: Any) -> None:
        return None


class _SetupHarness(ZvecGrepMixin, _SetupParent):
    def __init__(self, root: Path) -> None:
        self.logs_dir = root / "pallets__flask-5014__trial" / "agent"
        self._zvec_grep_package = "@zvec/zvec-grep@0.1.6"
        self._zvec_binding_package = "@zvec/bindings-linux-x64@0.5.0"
        self._embedding_model = "qwen/text-embedding-v4"
        self._mcp_target = None
        self._zvec_grep_package_sha256 = None
        self._api_key_source = "DASHSCOPE_API_KEY"
        self._embedding_api_key = "secret"
        self._index_cache_host_root = root / "cache"
        self._index_cache_container_root = "/opt/zvec-grep-index-cache"
        self._index_cache_dataset = "swe-bench/swe-bench-verified"
        self.agent_commands: list[str] = []
        self.root_commands: list[str] = []
        self.status_failures = 0
        self.publish_failure = False
        self.restore_failure = False

    async def _install_zvec_grep(
        self, environment: Any
    ) -> tuple[str, bool]:
        return "0.1.6", True

    async def _resolve_workdir(self, environment: Any) -> str:
        return "/testbed"

    async def _hide_index_from_git(
        self, environment: Any, workdir: str
    ) -> bool:
        return True

    async def exec_as_agent(
        self, environment: Any, command: str, **kwargs: Any
    ) -> _Result:
        self.agent_commands.append(command)
        if command.startswith("zg status"):
            if self.status_failures:
                self.status_failures -= 1
                raise RuntimeError("cached index is not ready")
            return _Result(stdout="state ready\n")
        return _Result()

    async def exec_as_root(
        self, environment: Any, command: str, **kwargs: Any
    ) -> _Result:
        self.root_commands.append(command)
        if (
            self.restore_failure
            and "cp -a /opt/zvec-grep-index-cache" in command
        ):
            raise OSError("cache backup is unavailable in the container")
        if self.publish_failure and "cp -a /testbed/.zvec-grep" in command:
            raise OSError("cache mount is read-only")
        return _Result()


class InstallZvecGrepTests(unittest.IsolatedAsyncioTestCase):
    def test_remote_index_uses_workspace_authorization(self) -> None:
        self.assertEqual(
            ZvecGrepMixin._authorization_command("qwen/text-embedding-v4"),
            "zg auth grant --capability embedding --scope workspace "
            "--embedding qwen/text-embedding-v4",
        )
        index_command = ZvecGrepMixin._index_command("qwen/text-embedding-v4")
        self.assertEqual(
            index_command,
            "zg index --embedding qwen/text-embedding-v4",
        )
        self.assertNotIn("--allow-remote", index_command)
        self.assertIsNone(
            ZvecGrepMixin._authorization_command("local/embeddinggemma-300m")
        )

    async def test_recognizes_current_and_legacy_ready_status(self) -> None:
        self.assertTrue(
            ZvecGrepMixin._index_is_ready(
                "\x1b[32m✓ Workspace index is ready\x1b[0m\n  /workspace"
            )
        )
        self.assertTrue(ZvecGrepMixin._index_is_ready("state ready\n"))

    async def test_does_not_treat_stale_index_as_ready(self) -> None:
        self.assertFalse(
            ZvecGrepMixin._index_is_ready(
                "! Workspace index needs an update\n  /workspace"
            )
        )

    async def test_local_and_new_packages_use_machine_ready_checks(self) -> None:
        self.assertTrue(
            ZvecGrepMixin._supports_machine_ready_check(
                "0.1.5", package_sha256="a" * 64
            )
        )
        self.assertTrue(
            ZvecGrepMixin._supports_machine_ready_check(
                "0.1.6", package_sha256=None
            )
        )
        self.assertFalse(
            ZvecGrepMixin._supports_machine_ready_check(
                "0.1.5", package_sha256=None
            )
        )

    async def test_bootstraps_node_before_installing_zvec_grep(self) -> None:
        harness = _InstallHarness()

        version, reused = await harness._install_zvec_grep(object())

        install_command = harness.agent_commands[0]
        self.assertTrue(install_command.startswith("set -e; "))
        self.assertIn("! command -v node", install_command)
        self.assertIn("! command -v npm", install_command)
        self.assertIn("nvm-sh/nvm/v0.40.2/install.sh", install_command)
        self.assertLess(
            install_command.index("! command -v node"),
            install_command.index("npm install -g"),
        )
        self.assertEqual(version, "0.1.5")
        self.assertFalse(reused)

    async def test_uses_mirrors_for_node_and_npm_installation(self) -> None:
        harness = _InstallHarness()

        await harness._install_zvec_grep(object())

        install_command = harness.agent_commands[0]
        self.assertIn(
            "export NVM_NODEJS_ORG_MIRROR=https://npmmirror.com/mirrors/node",
            install_command,
        )
        self.assertIn(
            "export NPM_CONFIG_REGISTRY=https://registry.npmmirror.com",
            install_command,
        )
        self.assertLess(
            install_command.index("export NVM_NODEJS_ORG_MIRROR"),
            install_command.index("nvm install 22"),
        )
        self.assertLess(
            install_command.index("export NPM_CONFIG_REGISTRY"),
            install_command.index("npm install -g"),
        )

    async def test_resolves_zg_from_the_npm_global_prefix(self) -> None:
        harness = _InstallHarness()

        await harness._install_zvec_grep(object())

        install_command = harness.agent_commands[0]
        self.assertIn('zg_path="$(npm prefix -g)/bin/zg"', install_command)
        self.assertIn('[ -x "$zg_path" ]', install_command)
        self.assertIn('"$zg_path"', install_command)
        self.assertNotIn('"$(command -v zg)"', install_command)

    async def test_npm_failure_cannot_be_masked_by_marker_output(self) -> None:
        harness = _InstallHarness(package="@zvec/zvec-grep")

        await harness._install_zvec_grep(object())

        install_command = harness.agent_commands[0]
        self.assertIn("npm install -g", install_command)
        self.assertIn("--no-fund && reused=0", install_command)
        self.assertNotIn("--no-fund; reused=0", install_command)

    async def test_local_package_reuses_only_matching_package_hash(self) -> None:
        harness = _InstallHarness(package="/tmp/zg-bench-zvec-grep.tgz")
        harness._zvec_grep_package_sha256 = "a" * 64

        await harness._install_zvec_grep(object())

        install_command = harness.agent_commands[0]
        self.assertIn(".zg-bench-zvec-grep-sha256", install_command)
        self.assertIn("a" * 64, install_command)
        self.assertIn("npm install -g /tmp/zg-bench-zvec-grep.tgz", install_command)


class IndexCacheSetupTests(unittest.IsolatedAsyncioTestCase):
    async def test_cache_lock_waits_for_the_current_builder(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            lock_path = Path(temp_dir) / "case" / ".lock"
            first_lock = await ZvecGrepMixin._acquire_index_cache_lock(
                lock_path
            )
            second_acquire = asyncio.create_task(
                ZvecGrepMixin._acquire_index_cache_lock(lock_path)
            )
            await asyncio.sleep(0.01)
            self.assertFalse(second_acquire.done())

            ZvecGrepMixin._release_index_cache_lock(first_lock)
            second_lock = await asyncio.wait_for(second_acquire, timeout=1)
            ZvecGrepMixin._release_index_cache_lock(second_lock)

    async def test_cache_miss_builds_index_then_publishes_backup(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            harness = _SetupHarness(Path(temp_dir))

            await harness.setup(object())

            self.assertIn(
                "zg index --embedding qwen/text-embedding-v4",
                harness.agent_commands,
            )
            self.assertTrue(
                any(
                    "cp -a /testbed/.zvec-grep" in command
                    for command in harness.root_commands
                )
            )
            metadata = json.loads(
                (Path(harness.logs_dir) / "zvec-grep-setup.json").read_text()
            )
            self.assertFalse(metadata["index_cache_hit"])
            self.assertIn("index_build_duration_seconds", metadata)
            self.assertIn("index_backup_duration_seconds", metadata)

    async def test_cache_path_uses_dataset_case_and_embedding_hierarchy(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            harness = _SetupHarness(Path(temp_dir))

            paths = harness._resolve_index_cache_paths()

            assert paths is not None
            self.assertEqual(
                paths.host_backup.relative_to(harness._index_cache_host_root),
                Path(
                    "swe-bench/swe-bench-verified/"
                    "pallets__flask-5014/qwen/text-embedding-v4/.zvec-grep"
                ),
            )

    async def test_cache_hit_restores_copy_and_skips_index_build(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            harness = _SetupHarness(Path(temp_dir))
            paths = harness._resolve_index_cache_paths()
            assert paths is not None
            paths.host_backup.mkdir(parents=True)
            (paths.host_backup / "marker").write_text("cached")

            await harness.setup(object())

            self.assertNotIn(
                "zg index --embedding qwen/text-embedding-v4",
                harness.agent_commands,
            )
            self.assertTrue(
                any(
                    f"cp -a {paths.container_backup}" in command
                    and "/testbed/.zvec-grep" in command
                    for command in harness.root_commands
                )
            )
            authorization_index = harness.agent_commands.index(
                "zg auth grant --capability embedding --scope workspace "
                "--embedding qwen/text-embedding-v4"
            )
            status_index = harness.agent_commands.index(
                "zg status --check-ready"
            )
            self.assertLess(authorization_index, status_index)
            metadata = json.loads(
                (Path(harness.logs_dir) / "zvec-grep-setup.json").read_text()
            )
            self.assertTrue(metadata["index_cache_hit"])
            self.assertIn("index_restore_duration_seconds", metadata)
            self.assertNotIn("index_build_duration_seconds", metadata)

    async def test_invalid_cache_is_quarantined_then_rebuilt_once(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            harness = _SetupHarness(Path(temp_dir))
            paths = harness._resolve_index_cache_paths()
            assert paths is not None
            paths.host_backup.mkdir(parents=True)
            (paths.host_backup / "marker").write_text("stale")
            harness.status_failures = 1

            await harness.setup(object())

            self.assertEqual(
                harness.agent_commands.count(
                    "zg index --embedding qwen/text-embedding-v4"
                ),
                1,
            )
            invalid_backups = list(
                paths.host_parent.glob(".zvec-grep.invalid-*")
            )
            self.assertEqual(len(invalid_backups), 1)
            self.assertTrue((invalid_backups[0] / "marker").is_file())
            metadata = json.loads(
                (Path(harness.logs_dir) / "zvec-grep-setup.json").read_text()
            )
            self.assertFalse(metadata["index_cache_hit"])
            self.assertTrue(metadata["index_cache_rebuilt"])
            self.assertEqual(
                metadata["index_cache_invalid_path"],
                f"{paths.container_parent}/{invalid_backups[0].name}",
            )

    async def test_quarantine_keeps_only_latest_invalid_backup(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            harness = _SetupHarness(Path(temp_dir))
            paths = harness._resolve_index_cache_paths()
            assert paths is not None
            paths.host_parent.mkdir(parents=True)
            old_invalid = paths.host_parent / ".zvec-grep.invalid-old"
            old_invalid.mkdir()
            paths.host_backup.mkdir()

            invalid_path = harness._quarantine_index_backup(paths)

            assert invalid_path is not None
            self.assertTrue(invalid_path.is_dir())
            self.assertFalse(old_invalid.exists())
            self.assertEqual(
                list(paths.host_parent.glob(".zvec-grep.invalid-*")),
                [invalid_path],
            )

    async def test_backup_failure_does_not_fail_index_setup(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            harness = _SetupHarness(Path(temp_dir))
            harness.publish_failure = True

            await harness.setup(object())

            metadata = json.loads(
                (Path(harness.logs_dir) / "zvec-grep-setup.json").read_text()
            )
            self.assertEqual(metadata["status"], "ready")
            self.assertEqual(
                metadata["index_cache_error"],
                "cache mount is read-only",
            )

    async def test_restore_failure_builds_without_quarantining_backup(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            harness = _SetupHarness(Path(temp_dir))
            paths = harness._resolve_index_cache_paths()
            assert paths is not None
            paths.host_backup.mkdir(parents=True)
            harness.restore_failure = True

            await harness.setup(object())

            self.assertTrue(paths.host_backup.is_dir())
            self.assertEqual(
                list(paths.host_parent.glob(".zvec-grep.invalid-*")),
                [],
            )
            self.assertIn(
                "zg index --embedding qwen/text-embedding-v4",
                harness.agent_commands,
            )
            metadata = json.loads(
                (Path(harness.logs_dir) / "zvec-grep-setup.json").read_text()
            )
            self.assertEqual(metadata["status"], "ready")
            self.assertFalse(metadata["index_cache_rebuilt"])
            self.assertIn(
                "cache backup is unavailable",
                metadata["index_cache_error"],
            )


if __name__ == "__main__":
    unittest.main()
