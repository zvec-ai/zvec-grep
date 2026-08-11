from __future__ import annotations

import json
import os
import shutil
import tempfile
import unittest
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from unittest.mock import patch

from zg_bench.agents.zvec_grep import ZvecGrepMixin
from zg_bench.settings import ZVEC_GREP_INDEX_SEED_ENV


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
        self._embedding_model = "local/potion-code-16m-v2"
        self._embedding_api_key: str | None = None
        self._index_seed_root: Path | None = None
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


class _SetupBase:
    def __init__(self, *args: Any, extra_env: dict[str, str], **kwargs: Any) -> None:
        self.extra_env = extra_env

    async def setup(self, environment: Any) -> None:
        return None


class _FakeEnvironment:
    default_user = "agent"

    def __init__(self, container_root: Path) -> None:
        self.container_root = container_root
        self.uploads: list[tuple[Path, str]] = []
        self.downloads: list[tuple[str, Path]] = []

    def container_path(self, path: str) -> Path:
        if not path.startswith("/"):
            raise AssertionError(f"container path must be absolute: {path}")
        return self.container_root / path.lstrip("/")

    async def upload_dir(self, source: Path, destination: str) -> None:
        source = Path(source)
        self.uploads.append((source, destination))
        target = self.container_path(destination)
        target.mkdir(parents=True, exist_ok=True)
        shutil.copytree(source, target, dirs_exist_ok=True)

    async def download_dir(self, source: str, destination: Path) -> None:
        destination = Path(destination)
        self.downloads.append((source, destination))
        destination.mkdir(parents=True, exist_ok=True)
        shutil.copytree(
            self.container_path(source),
            destination,
            dirs_exist_ok=True,
        )


class _SetupHarness(ZvecGrepMixin, _SetupBase):
    def __init__(self, seed_root: Path, *, secret: str | None = None) -> None:
        self.agent_commands: list[str] = []
        self.root_commands: list[str] = []
        self.setup_metadata: dict[str, Any] = {}
        with patch.dict(
            os.environ,
            {ZVEC_GREP_INDEX_SEED_ENV: str(seed_root)},
        ):
            super().__init__(
                zvec_grep_package="/tmp/zg-bench-zvec-grep.tgz",
                zvec_binding_package="@zvec/bindings-linux-x64@0.5.0",
                embedding_model="local/potion-code-16m-v2",
                zvec_grep_package_sha256="a" * 64,
            )
        self._embedding_api_key = secret

    async def _install_zvec_grep(self, environment: Any) -> tuple[str, bool]:
        return "0.1.5", False

    async def _resolve_workdir(self, environment: Any) -> str:
        return "/workspace"

    async def _hide_index_from_git(
        self, environment: Any, workdir: str
    ) -> bool:
        return True

    async def exec_as_agent(
        self, environment: _FakeEnvironment, command: str, **kwargs: Any
    ) -> _Result:
        self.agent_commands.append(command)
        workdir = kwargs.get("cwd", "/workspace")
        index_dir = environment.container_path(f"{workdir}/.zvec-grep")
        if "ZG_INDEX_REPO_COMMIT" in command:
            return _Result(
                stdout=(
                    "ZG_INDEX_SEED_AVAILABLE=1\n"
                    f"ZG_INDEX_REPO_COMMIT={'c' * 40}\n"
                )
            )
        if command == "rm -rf -- .zvec-grep && mkdir -p .zvec-grep":
            shutil.rmtree(index_dir, ignore_errors=True)
            index_dir.mkdir(parents=True)
            return _Result()
        if command.startswith("if zg status --check-ready;"):
            ready_path = index_dir / "ready"
            ready = ready_path.is_file() and ready_path.read_text() == "ready\n"
            return _Result(stdout=f"ZG_INDEX_SEED_READY={int(ready)}\n")
        if command.startswith("zg index "):
            index_dir.mkdir(parents=True, exist_ok=True)
            (index_dir / "ready").write_text("ready\n", encoding="utf-8")
            (index_dir / "index.bin").write_bytes(b"trusted-index-data")
            return _Result(stdout="index built")
        if command == "zg status --check-ready":
            return _Result(stdout="\N{CHECK MARK} Workspace index is ready")
        if command == "rm -rf -- .zvec-grep":
            shutil.rmtree(index_dir, ignore_errors=True)
            return _Result()
        raise AssertionError(f"unexpected setup command: {command}")

    async def exec_as_root(
        self, environment: Any, command: str, **kwargs: Any
    ) -> _Result:
        self.root_commands.append(command)
        return _Result()

    def _write_setup_metadata(self, metadata: dict[str, Any]) -> None:
        self.setup_metadata = dict(metadata)


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

    async def test_index_seed_key_changes_with_commit_package_and_model(
        self,
    ) -> None:
        identity = {
            "format_version": 1,
            "repo_commit": "a" * 40,
            "zvec_grep_version": "0.1.5",
            "zvec_grep_package": "b" * 64,
            "binding_package": "@zvec/bindings-linux-x64@0.5.0",
            "embedding_model": "local/potion-code-16m-v2",
            "platform": "linux/amd64",
            "workdir": "/workspace",
        }
        original_key = ZvecGrepMixin._index_seed_key(identity)
        changes = {
            "repo_commit": "d" * 40,
            "zvec_grep_package": "e" * 64,
            "embedding_model": "local/embeddinggemma-300m",
            "workdir": "/app",
        }

        for field, value in changes.items():
            with self.subTest(field=field):
                changed = dict(identity)
                changed[field] = value
                self.assertNotEqual(
                    ZvecGrepMixin._index_seed_key(changed),
                    original_key,
                )

    async def test_remote_embedding_disables_index_seed_without_probe(self) -> None:
        harness = _InstallHarness()
        harness._embedding_model = "qwen/text-embedding-v4"

        identity = await harness._index_seed_identity(
            object(),
            "/workspace",
            zvec_grep_version="0.1.5",
        )

        self.assertIsNone(identity)
        self.assertEqual(harness.agent_commands, [])

    async def test_host_seed_roundtrip_restores_and_skips_cold_index(self) -> None:
        secret = "embedding-secret-must-not-be-persisted"
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            seed_root = (root / "seed-cache").resolve()
            cold_environment = _FakeEnvironment(root / "cold-container")
            cold_harness = _SetupHarness(seed_root, secret=secret)

            await cold_harness.setup(cold_environment)

            seed_key = cold_harness.setup_metadata["index_seed_key"]
            seed_dir = seed_root / seed_key
            identity = json.loads(
                (seed_dir / "identity.json").read_text(encoding="utf-8")
            )
            self.assertTrue(ZvecGrepMixin._index_seed_is_valid(seed_dir, seed_key))
            self.assertEqual(cold_environment.uploads, [])
            self.assertEqual(
                [source for source, _ in cold_environment.downloads],
                ["/workspace/.zvec-grep"],
            )
            self.assertTrue(
                any(
                    command.startswith("zg index ")
                    for command in cold_harness.agent_commands
                )
            )
            self.assertEqual(identity["repo_commit"], "c" * 40)
            self.assertEqual(identity["zvec_grep_package"], "a" * 64)
            self.assertEqual(identity["embedding_model"], "local/potion-code-16m-v2")
            self.assertEqual(identity["workdir"], "/workspace")
            self.assertNotIn("api_key", json.dumps(identity).lower())
            host_contents = b"".join(
                path.read_bytes() for path in seed_dir.rglob("*") if path.is_file()
            )
            self.assertNotIn(secret.encode(), host_contents)

            hit_environment = _FakeEnvironment(root / "hit-container")
            hit_harness = _SetupHarness(seed_root, secret=secret)

            await hit_harness.setup(hit_environment)

            self.assertEqual(
                hit_environment.uploads,
                [(seed_dir / "workspace", "/workspace/.zvec-grep")],
            )
            self.assertEqual(hit_environment.downloads, [])
            self.assertFalse(
                any(
                    command.startswith("zg index ")
                    for command in hit_harness.agent_commands
                )
            )
            self.assertTrue(hit_harness.setup_metadata["index_seed_restored"])
            self.assertTrue(hit_harness.setup_metadata["index_cache_hit"])
            self.assertEqual(hit_harness.setup_metadata["index_duration_seconds"], 0.0)
            self.assertEqual(
                hit_environment.container_path(
                    "/workspace/.zvec-grep/index.bin"
                ).read_bytes(),
                b"trusted-index-data",
            )
            for harness in (cold_harness, hit_harness):
                container_commands = "\n".join(
                    harness.agent_commands + harness.root_commands
                )
                self.assertNotIn(str(seed_root), container_commands)

    async def test_invalid_restored_seed_falls_back_and_replaces_host_seed(
        self,
    ) -> None:
        secret = "embedding-secret-must-not-be-persisted"
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            seed_root = (root / "seed-cache").resolve()
            initial_environment = _FakeEnvironment(root / "initial-container")
            initial_harness = _SetupHarness(seed_root, secret=secret)
            await initial_harness.setup(initial_environment)
            seed_key = initial_harness.setup_metadata["index_seed_key"]
            seed_dir = seed_root / seed_key
            (seed_dir / "workspace" / "ready").write_text(
                "stale\n",
                encoding="utf-8",
            )

            retry_environment = _FakeEnvironment(root / "retry-container")
            retry_harness = _SetupHarness(seed_root, secret=secret)

            await retry_harness.setup(retry_environment)

            cleanup = retry_harness.agent_commands.index("rm -rf -- .zvec-grep")
            cold_index = retry_harness.agent_commands.index(
                "zg index --embedding local/potion-code-16m-v2"
            )
            self.assertLess(cleanup, cold_index)
            self.assertEqual(len(retry_environment.uploads), 1)
            self.assertEqual(len(retry_environment.downloads), 1)
            self.assertTrue(retry_harness.setup_metadata["index_seed_restored"])
            self.assertFalse(retry_harness.setup_metadata["index_cache_hit"])
            self.assertTrue(retry_harness.setup_metadata["index_seed_saved"])
            self.assertEqual(
                (seed_dir / "workspace" / "ready").read_text(encoding="utf-8"),
                "ready\n",
            )
            self.assertTrue(ZvecGrepMixin._index_seed_is_valid(seed_dir, seed_key))
            host_contents = b"".join(
                path.read_bytes() for path in seed_dir.rglob("*") if path.is_file()
            )
            self.assertNotIn(secret.encode(), host_contents)
            container_commands = "\n".join(
                retry_harness.agent_commands + retry_harness.root_commands
            )
            self.assertNotIn(str(seed_root), container_commands)


if __name__ == "__main__":
    unittest.main()
