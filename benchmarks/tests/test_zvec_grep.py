from __future__ import annotations

import unittest
from dataclasses import dataclass
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


class InstallZvecGrepTests(unittest.IsolatedAsyncioTestCase):
    async def test_index_authorizes_remote_embedding_once(self) -> None:
        self.assertEqual(
            ZvecGrepMixin._index_command("qwen/text-embedding-v4"),
            "zg index --embedding qwen/text-embedding-v4 --allow-remote once",
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


if __name__ == "__main__":
    unittest.main()
