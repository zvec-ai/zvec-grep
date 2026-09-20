from __future__ import annotations

import unittest
from typing import Any
from unittest.mock import AsyncMock, patch

from harbor.agents.installed.base import NonZeroAgentExitCodeError

from zg_bench.agents.opencode import (
    ResilientOpenCode,
    resilient_nvm_node_install_snippet,
)
from zg_bench.agents.zvec_opencode import ZvecOpenCode


class _InstallHarness(ResilientOpenCode):
    def __init__(self, *, failures: int = 0) -> None:
        self._version = "1.18.4"
        self.failures = failures
        self.root_commands: list[str] = []
        self.agent_commands: list[str] = []

    async def exec_as_root(
        self, environment: Any, command: str, **kwargs: Any
    ) -> None:
        self.root_commands.append(command)

    async def exec_as_agent(
        self, environment: Any, command: str, **kwargs: Any
    ) -> None:
        self.agent_commands.append(command)
        if self.failures:
            self.failures -= 1
            raise NonZeroAgentExitCodeError("transient install failure")


class ResilientOpenCodeTests(unittest.IsolatedAsyncioTestCase):
    def test_nvm_install_is_cache_first_and_does_not_pipe_to_bash(self) -> None:
        snippet = resilient_nvm_node_install_snippet()

        self.assertIn('if [ ! -s "$NVM_DIR/nvm.sh" ]', snippet)
        self.assertIn("--fail", snippet)
        self.assertIn("--retry 3", snippet)
        self.assertIn("--retry-all-errors", snippet)
        self.assertIn("--retry-max-time 90", snippet)
        self.assertIn('--output "$nvm_installer"', snippet)
        self.assertNotIn("| bash", snippet)

    async def test_install_retries_transient_nonzero_failures(self) -> None:
        agent = _InstallHarness(failures=2)

        with patch(
            "zg_bench.agents.opencode.asyncio.sleep", new=AsyncMock()
        ) as sleep:
            await agent.install(object())

        self.assertEqual(len(agent.root_commands), 1)
        self.assertEqual(len(agent.agent_commands), 3)
        self.assertEqual(sleep.await_count, 2)
        command = agent.agent_commands[0]
        self.assertIn("opencode-ai@1.18.4", command)
        self.assertIn('installed_opencode_version="$(opencode --version', command)

    async def test_install_reraises_after_bounded_attempts(self) -> None:
        agent = _InstallHarness(failures=3)

        with (
            patch("zg_bench.agents.opencode.asyncio.sleep", new=AsyncMock()),
            self.assertRaises(NonZeroAgentExitCodeError),
        ):
            await agent.install(object())

        self.assertEqual(len(agent.agent_commands), 3)

    def test_zvec_profile_uses_the_same_resilient_adapter(self) -> None:
        self.assertTrue(issubclass(ZvecOpenCode, ResilientOpenCode))
