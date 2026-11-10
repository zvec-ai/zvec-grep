from __future__ import annotations

import json
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, patch

from harbor.agents.installed.base import NonZeroAgentExitCodeError
from harbor.agents.installed.opencode import OpenCode
from harbor.models.agent.context import AgentContext

from zg_bench.agents.opencode import ResilientOpenCode
from zg_bench.agents import ripgrep


def ready_evidence() -> dict:
    return dict(schema_version=1, status="ready", version=ripgrep.RIPGREP_VERSION,
                command_path=ripgrep.RIPGREP_PATH, real_path=ripgrep.RIPGREP_PATH,
                binary_sha256=ripgrep.RIPGREP_BINARY_SHA256,
                search_output=ripgrep.RIPGREP_SEARCH_OUTPUT,
                uid=0, gid=0, user="root", cwd="/app", path="/usr/local/bin:/usr/bin",
                version_output="ripgrep 14.1.1\nfeatures:-simd-accel,+pcre2")


class RipgrepContractTests(unittest.TestCase):
    def test_install_checks_archive_and_binary_before_installing(self):
        command = ripgrep.ripgrep_install_command()
        self.assertLess(command.index(ripgrep.RIPGREP_ARCHIVE_SHA256), command.index("tar -xzf"))
        self.assertLess(command.index(ripgrep.RIPGREP_BINARY_SHA256), command.index("install -m"))
        self.assertIn("--retry 3", command)
        self.assertNotIn("apt-get", command)
        subprocess.run(["bash", "-n", "-c", command], check=True)
        subprocess.run(["bash", "-n", "-c", ripgrep.ripgrep_preflight_command()], check=True)

    def test_stale_binary_wrong_path_or_failed_search_fail_closed(self):
        ripgrep.validate_ripgrep_preflight(ready_evidence())
        for key, value in (
            ("binary_sha256", "0" * 64), ("version", "14.1.0"),
            ("command_path", "/tmp/rg"), ("real_path", "/tmp/rg"),
            ("status", "failed"), ("search_output", ""), ("cwd", ""),
            ("uid", None), ("path", ""),
        ):
            with self.subTest(key=key), self.assertRaises(RuntimeError):
                ripgrep.validate_ripgrep_preflight(ready_evidence() | {key: value})

    def test_unfrozen_hashes_cannot_install(self):
        with patch.object(ripgrep, "RIPGREP_ARCHIVE_SHA256", "unknown"):
            with self.assertRaises(ValueError):
                ripgrep.ripgrep_install_command()


class RipgrepLifecycleTests(unittest.IsolatedAsyncioTestCase):
    async def test_ready_probe_precedes_model_and_preserves_context(self):
        with tempfile.TemporaryDirectory() as temp:
            agent = ResilientOpenCode(logs_dir=Path(temp))
            context = AgentContext()
            events = []

            async def probe(*args, **kwargs):
                events.append("probe")
                self.assertNotIn("user", kwargs)
                self.assertNotIn("cwd", kwargs)
                self.assertNotIn("env", kwargs)
                self.assertIn(". ~/.nvm/nvm.sh", kwargs["command"])
                return SimpleNamespace(stdout=json.dumps(ready_evidence()))

            async def model(*args, **kwargs):
                events.append("model")
                self.assertEqual(args[0], "unchanged question")

            with (patch.object(agent, "exec_as_agent", new=AsyncMock(side_effect=probe)),
                  patch.object(OpenCode, "run", new=AsyncMock(side_effect=model))):
                await agent.run("unchanged question", object(), context)
            self.assertEqual(events, ["probe", "model"])
            evidence = json.loads((Path(temp) / ripgrep.RIPGREP_PREFLIGHT_FILENAME).read_text())
            self.assertEqual(evidence["status"], "ready")
            self.assertEqual(evidence["archive_sha256"], ripgrep.RIPGREP_ARCHIVE_SHA256)
            self.assertGreaterEqual(evidence["preflight_wall_seconds"], 0)
            self.assertTrue(context.is_empty())

    async def test_invalid_or_missing_rg_never_starts_model_or_usage_export(self):
        for response in (
            ready_evidence() | {"binary_sha256": "0" * 64},
            {"status": "failed", "error": {"name": "ENOENT"}},
            [],
        ):
            with self.subTest(response=response), tempfile.TemporaryDirectory() as temp:
                agent = ResilientOpenCode(logs_dir=Path(temp), collect_session_usage=True)
                with (
                    patch.object(agent, "exec_as_agent", new=AsyncMock(
                        return_value=SimpleNamespace(stdout=json.dumps(response)))) as execute,
                    patch.object(OpenCode, "run", new=AsyncMock()) as model,
                    self.assertRaises(RuntimeError),
                ):
                    await agent.run("test", object(), AgentContext())
                model.assert_not_awaited()
                execute.assert_awaited_once()
                evidence = json.loads((Path(temp) / ripgrep.RIPGREP_PREFLIGHT_FILENAME).read_text())
                self.assertEqual(evidence["status"], "failed")
                self.assertEqual(evidence["validation_error"], "RuntimeError")

    async def test_shell_failure_is_preserved_and_does_not_export_usage(self):
        with tempfile.TemporaryDirectory() as temp:
            agent = ResilientOpenCode(logs_dir=Path(temp), collect_session_usage=True)
            with (
                patch.object(agent, "exec_as_agent", new=AsyncMock(
                    side_effect=NonZeroAgentExitCodeError("nvm unavailable"))) as execute,
                patch.object(OpenCode, "run", new=AsyncMock()) as model,
                self.assertRaises(NonZeroAgentExitCodeError),
            ):
                await agent.run("test", object(), AgentContext())
            execute.assert_awaited_once()
            model.assert_not_awaited()
            evidence = json.loads((Path(temp) / ripgrep.RIPGREP_PREFLIGHT_FILENAME).read_text())
            self.assertEqual(evidence["validation_error"], "NonZeroAgentExitCodeError")

    async def test_rg_install_failure_does_not_retry_original_curl_or_install_opencode(self):
        with tempfile.TemporaryDirectory() as temp:
            agent = ResilientOpenCode(logs_dir=Path(temp), version="1.18.4")
            with (
                patch.object(agent, "exec_as_root", new=AsyncMock(side_effect=[
                    None, NonZeroAgentExitCodeError("rg checksum failure")])) as root,
                patch.object(agent, "exec_as_agent", new=AsyncMock()) as execute,
                self.assertRaises(NonZeroAgentExitCodeError),
            ):
                await agent.install(object())
            self.assertEqual(root.await_count, 2)
            self.assertEqual(root.call_args_list[0].kwargs["command"],
                             "apt-get update && apt-get install -y curl")
            execute.assert_not_awaited()


if __name__ == "__main__":
    unittest.main()
