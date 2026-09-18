from __future__ import annotations

import json
import asyncio
import os
import shutil
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock, patch

from harbor.agents.factory import AgentFactory
from harbor.agents.installed.base import NonZeroAgentExitCodeError
from harbor.agents.installed.opencode import OpenCode
from harbor.cli.utils import parse_env_vars, parse_kwargs
from harbor.models.agent.context import AgentContext
from harbor.models.trial.config import AgentConfig

from zg_bench import runner
from zg_bench.agents.opencode import (
    ResilientOpenCode,
    resilient_nvm_node_install_snippet,
)
from zg_bench.agents.zvec_opencode import ZvecOpenCode
from zg_bench.agents.opencode_usage import USAGE_SQL


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

    async def test_snapshot_captured_in_finally_without_hiding_run_failure(self) -> None:
        for error in (None, RuntimeError("model failed"), asyncio.CancelledError()):
            with self.subTest(error=type(error).__name__), tempfile.TemporaryDirectory() as temp:
                agent = ResilientOpenCode(logs_dir=Path(temp), collect_session_usage=True)
                context = AgentContext()
                with (
                    patch.object(OpenCode, "run", new=AsyncMock(side_effect=error)),
                    patch.object(agent, "exec_as_agent", new=AsyncMock()) as export,
                ):
                    if error is None:
                        await agent.run("test", object(), context)
                    else:
                        with self.assertRaises(type(error)):
                            await agent.run("test", object(), context)
                export.assert_awaited_once()
                self.assertIn("opencode db", export.call_args.kwargs["command"])
                self.assertEqual(export.call_args.kwargs["timeout_sec"], 30)
                self.assertEqual(agent._usage_capture_complete, error is None)
                self.assertTrue(context.is_empty(), "Harbor must still run post-run parsing")

    async def test_export_failure_marks_incomplete_and_preserves_execution_error(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            agent = ResilientOpenCode(logs_dir=Path(temp), collect_session_usage=True)
            context = AgentContext()
            with (
                patch.object(OpenCode, "run", new=AsyncMock(side_effect=ValueError("original"))),
                patch.object(agent, "exec_as_agent", new=AsyncMock(side_effect=TimeoutError())),
                self.assertRaisesRegex(ValueError, "original"),
            ):
                await agent.run("test", object(), context)
            agent.populate_context_post_run(context)
            usage = json.loads((Path(temp) / "session-usage.json").read_text())
            self.assertFalse(usage["complete"])
            self.assertIn("snapshot_capture_failed:TimeoutError", usage["errors"])

    def test_usage_capture_flag_requires_actual_boolean(self) -> None:
        with self.assertRaisesRegex(ValueError, "boolean"):
            ResilientOpenCode(logs_dir=Path("unused"), collect_session_usage="false")


@unittest.skipUnless(
    os.environ.get("OPENCODE_BENCHMARK_TEST_BINARY"),
    "requires pinned OpenCode binary; sends requests only to a local fake provider",
)
class OpenCodeSamplingContractTests(unittest.TestCase):
    def test_task_subagent_and_background_request_contract(self) -> None:
        requested_binary = os.environ["OPENCODE_BENCHMARK_TEST_BINARY"]
        binary = shutil.which(requested_binary) or str(
            Path(requested_binary).resolve()
        )
        self.assertEqual(
            subprocess.check_output(
                [binary, "--version"], text=True, timeout=10
            ).strip(),
            runner.OPENCODE_VERSION,
        )
        suite = runner.load_suite("swe-qa-bench", tier="smoke")
        cases = (
            ("custom-openai/glm-5.2", True),
            ("custom-openai/qwen3.8-max", True),
            ("aliyun-glm-5.2", True),
            # Prove that neither provider defaults nor the fixture itself hide
            # web tools: removing the benchmark denies must expose both tools.
            ("custom-openai/glm-5.2", False),
        )
        for model, disable_web_tools in cases:
            with (
                self.subTest(model=model, disable_web_tools=disable_web_tools),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                # macOS /var is a symlink to /private/var. Match the runtime's
                # canonical cwd so a local child read is not an external path.
                root = Path(temp_dir).resolve()
                corpus = root / "corpus"
                corpus.mkdir()
                (corpus / "README.md").write_text("The fixture value is 42.\n")
                requests: list[dict[str, Any]] = []
                child_requests: list[dict[str, Any]] = []
                authorization_headers: list[str | None] = []
                fixture_key = "offline-fixture-not-a-real-key"

                class FakeProvider(BaseHTTPRequestHandler):
                    def log_message(self, *_args: Any) -> None:
                        pass

                    def do_POST(self) -> None:
                        authorization = self.headers.get("Authorization")
                        authorization_headers.append(authorization)
                        if authorization != f"Bearer {fixture_key}":
                            self.send_error(401, "No valid API key provided")
                            return
                        body = json.loads(
                            self.rfile.read(int(self.headers["Content-Length"]))
                        )
                        requests.append(body)
                        is_child = bool(self.headers.get("x-parent-session-id"))
                        if is_child:
                            child_requests.append(body)
                        task = bool(body.get("tools"))
                        after_tool = any(
                            message.get("role") == "tool"
                            for message in body.get("messages", [])
                        )
                        if task and not after_tool and not is_child:
                            delta = {
                                "role": "assistant",
                                "tool_calls": [
                                    {
                                        "index": index,
                                        "id": f"delegate-{agent}",
                                        "type": "function",
                                        "function": {
                                            "name": "task",
                                            "arguments": json.dumps(
                                                {
                                                    "description": "Read local fixture value",
                                                    "prompt": "Read README.md and report its fixture value.",
                                                    "subagent_type": agent,
                                                }
                                            ),
                                        },
                                    }
                                    for index, agent in enumerate(("general", "explore"))
                                ],
                            }
                            finish = "tool_calls"
                        elif task and not after_tool:
                            delta = {
                                "role": "assistant",
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "read-fixture",
                                        "type": "function",
                                        "function": {
                                            "name": "read",
                                            "arguments": json.dumps(
                                                {"filePath": str(corpus / "README.md")}
                                            ),
                                        },
                                    }
                                ],
                            }
                            finish = "tool_calls"
                        else:
                            delta = {
                                "role": "assistant",
                                "content": "The fixture value is 42.",
                            }
                            finish = "stop"
                        envelope = {
                            "id": "fixture",
                            "object": "chat.completion.chunk",
                            "created": 1,
                            "model": body["model"],
                        }
                        chunks = [
                            # This synthetic value verifies that Qwen reasoning
                            # survives a tool round-trip without real model data.
                            *(
                                [{
                                    **envelope,
                                    "choices": [{
                                        "index": 0,
                                        "delta": {"role": "assistant", "reasoning_content": "fixture-reasoning"},
                                        "finish_reason": None,
                                    }],
                                }]
                                if body["model"] == "qwen3.8-max"
                                else []
                            ),
                            {
                                **envelope,
                                "choices": [
                                    {"index": 0, "delta": delta, "finish_reason": None}
                                ],
                            },
                            {
                                **envelope,
                                "choices": [
                                    {"index": 0, "delta": {}, "finish_reason": finish}
                                ],
                                "usage": {
                                    "prompt_tokens": 100,
                                    "completion_tokens": 50,
                                    "total_tokens": 150,
                                    "prompt_tokens_details": {"cached_tokens": 60},
                                    "completion_tokens_details": {"reasoning_tokens": 20},
                                },
                            },
                        ]
                        payload = (
                            "".join(
                                "data: " + json.dumps(chunk) + "\n\n"
                                for chunk in chunks
                            )
                            + "data: [DONE]\n\n"
                        ).encode()
                        self.send_response(200)
                        self.send_header("Content-Type", "text/event-stream")
                        self.send_header("Content-Length", str(len(payload)))
                        self.end_headers()
                        self.wfile.write(payload)

                server = ThreadingHTTPServer(("127.0.0.1", 0), FakeProvider)
                server.daemon_threads = True
                thread = threading.Thread(target=server.serve_forever, daemon=True)
                thread.start()
                try:
                    command = runner.build_harbor_command(
                        suite,
                        profile="baseline",
                        agent="opencode",
                        model=model,
                        job_name="wire-sampling-test",
                    )
                    config = json.loads(
                        next(
                            value.removeprefix("opencode_config=")
                            for value in command
                            if value.startswith("opencode_config=")
                        )
                    )
                    provider = next(iter(config["provider"].values()))
                    provider["options"]["baseURL"] = (
                        f"http://127.0.0.1:{server.server_port}/v1"
                    )
                    config.update(autoupdate=False, share="disabled", lsp=False)
                    if not disable_web_tools:
                        for permissions in (
                            config["permission"],
                            *(agent["permission"] for agent in config["agent"].values()),
                        ):
                            for tool in ("webfetch", "websearch"):
                                permissions.pop(tool)
                    for agent in ("build", "general", "explore"):
                        config["agent"][agent]["steps"] = 3
                    config_path = root / "opencode.json"
                    config_path.write_text(json.dumps(config))
                    logs_dir = root / "agent"
                    logs_dir.mkdir()
                    # Follow Harbor's real CLI parsing and AgentFactory env
                    # resolution. Host credentials must reach the isolated
                    # runtime through --agent-env, never a fixture shortcut.
                    source_key = (
                        "GLM_API_KEY" if model.startswith("custom-openai/")
                        else "DASHSCOPE_API_KEY"
                    )
                    with patch.dict(os.environ, {source_key: fixture_key}, clear=True):
                        host_env = runner.execution_environment(agent="opencode", model=model)
                    with patch.dict(os.environ, host_env, clear=True):
                        agent = AgentFactory.create_agent_from_config(
                            AgentConfig(
                                name=command[command.index("--agent") + 1],
                                model_name=command[command.index("--model") + 1],
                                env=parse_env_vars([
                                    command[index + 1]
                                    for index, value in enumerate(command)
                                    if value == "--agent-env"
                                ]),
                                kwargs=parse_kwargs([
                                    command[index + 1]
                                    for index, value in enumerate(command)
                                    if value == "--agent-kwarg"
                                ]),
                            ),
                            logs_dir=logs_dir,
                        )
                    self.assertEqual(agent.extra_env.get("OPENAI_API_KEY"), fixture_key)
                    env = {
                        "PATH": os.environ.get("PATH", os.defpath),
                        "HOME": str(root),
                        "LANG": "en_US.UTF-8",
                        "OPENCODE_CONFIG": str(config_path),
                        "OPENCODE_DISABLE_MODELS_FETCH": "true",
                        "OPENCODE_DISABLE_PROJECT_CONFIG": "true",
                        "OPENCODE_DISABLE_EXTERNAL_SKILLS": "true",
                        # websearch is otherwise absent for these providers.
                        "OPENCODE_ENABLE_EXA": "true",
                        "XDG_CONFIG_HOME": str(root / "config"),
                        "XDG_DATA_HOME": str(root / "data"),
                        "XDG_STATE_HOME": str(root / "state"),
                        "XDG_CACHE_HOME": str(root / "cache"),
                        **agent.extra_env,
                    }
                    result = subprocess.run(
                        [
                            binary,
                            "--model",
                            command[command.index("--model") + 1],
                            "run", "--format", "json",
                            "--dangerously-skip-permissions", "--",
                            "Read README.md and report its fixture value.",
                        ],
                        env=env,
                        cwd=corpus,
                        capture_output=True,
                        text=True,
                        timeout=60,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn("collect_session_usage=true", command)
                    snapshot = subprocess.check_output(
                        [binary, "db", USAGE_SQL, "--format", "json"],
                        env=env, cwd=corpus, text=True, timeout=30,
                    )
                    (logs_dir / "opencode.txt").write_text(result.stdout)
                    (logs_dir / "opencode-session-snapshot.json").write_text(snapshot)
                    agent._usage_capture_complete = True
                    context = AgentContext()
                    agent.populate_context_post_run(context)
                    usage = json.loads((logs_dir / "session-usage.json").read_text())
                    self.assertTrue(usage["complete"], usage["errors"])
                    self.assertEqual(len(usage["sessions"]), 3)
                    self.assertEqual(usage["root"]["input_tokens"], 200)
                    self.assertEqual(usage["descendants"]["input_tokens"], 400)
                    self.assertEqual(usage["total"]["tool_calls"], 4)
                    self.assertEqual(usage["total"]["llm_calls"], 6)
                    self.assertEqual(context.n_input_tokens, 600)
                    self.assertEqual(context.n_output_tokens, 300)
                    self.assertEqual(context.n_cache_tokens, 360)
                    self.assertEqual(usage["total"]["reasoning_tokens"], 120)
                    self.assertEqual(usage["total"]["text_output_tokens"], 180)
                    self.assertTrue((logs_dir / "trajectory.json").exists())
                    task_requests = [body for body in requests if body.get("tools")]
                    self.assertEqual(
                        len(task_requests), 6, result.stdout + result.stderr
                    )
                    self.assertEqual(
                        len([body for body in child_requests if body.get("tools")]),
                        4,
                        "Expected a read and continuation for both general and explore",
                    )
                    self.assertGreater(
                        len(requests), len(task_requests),
                        "Expected a title/summary request",
                    )
                    self.assertEqual(
                        authorization_headers, [f"Bearer {fixture_key}"] * len(requests)
                    )
                    for request in requests:
                        is_qwen = model == "custom-openai/qwen3.8-max"
                        self.assertEqual(request["model"], "qwen3.8-max" if is_qwen else "glm-5.2")
                        self.assertEqual(
                            request.get("temperature"), runner.OPENCODE_QWEN_TEMPERATURE if is_qwen else 0
                        )
                        self.assertEqual(request.get("seed"), 42)
                        self.assertIs(
                            request.get("enable_thinking"), runner.OPENCODE_QWEN_ENABLE_THINKING if is_qwen else True
                        )
                        self.assertEqual(
                            request.get("reasoning_effort"), runner.OPENCODE_QWEN_REASONING_EFFORT if is_qwen else "high"
                        )
                        self.assertEqual(request.get("max_tokens"), 32000)
                        self.assertNotIn("response_format", request)
                        self.assertNotIn("reasoningEffort", request)
                    if model == "custom-openai/qwen3.8-max":
                        replayed_assistants = [
                            message
                            for request in task_requests
                            for message in request["messages"]
                            if message.get("role") == "assistant" and message.get("tool_calls")
                        ]
                        self.assertEqual(len(replayed_assistants), 3)
                        for message in replayed_assistants:
                            self.assertEqual(message.get("reasoning_content"), "fixture-reasoning")
                    for request in task_requests:
                        tool_names = {
                            tool["function"]["name"] for tool in request["tools"]
                        }
                        self.assertTrue({"read", "grep", "glob"} <= tool_names)
                        for tool in ("webfetch", "websearch"):
                            self.assertEqual(
                                tool in tool_names,
                                not disable_web_tools,
                                f"Unexpected {tool} availability: {sorted(tool_names)}",
                            )
                finally:
                    server.shutdown()
                    server.server_close()
                    thread.join(timeout=5)
