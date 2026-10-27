from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch

from zg_bench.config import load_config
from zg_bench.opencode import _classify_attempt, run_prompt
from zg_bench.profiles import opencode_configuration
from zg_bench.report import _tool_interaction_batches
from zg_bench.trace import parse_trace


def event(kind, message="message-1", **part):
    return {
        "type": kind,
        "sessionID": "session-1",
        "part": {"id": f"{kind}-{message}", "messageID": message, **part},
    }


def finish(message="message-1", reason="stop"):
    return event(
        "step_finish",
        message,
        reason=reason,
        tokens={
            "input": 40,
            "output": 30,
            "reasoning": 20,
            "cache": {"read": 60, "write": 5},
        },
    )


class OpenCodeTraceTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.path = Path(temporary.name) / "events.jsonl"

    def parse(self, *events):
        self.path.write_text("".join(json.dumps(value) + "\n" for value in events))
        return parse_trace(self.path)

    def test_multistep_usage_includes_cache_and_reasoning_and_only_final_text(self):
        trace = self.parse(
            event("step_start"),
            event("text", text="I will search."),
            event("reasoning", text="This must not become the answer."),
            finish(reason="tool-calls"),
            event("step_start", "message-2"),
            event("text", "message-2", text="The answer is 42."),
            finish("message-2"),
        )
        self.assertTrue(trace.turn_completed)
        self.assertEqual(trace.final_response, "The answer is 42.")
        self.assertEqual(
            trace.usage.to_dict(),
            {
                "input_tokens": 210,
                "cached_input_tokens": 120,
                "output_tokens": 100,
                "reasoning_output_tokens": 40,
                "total_tokens": 310,
            },
        )

    def test_duplicate_finish_does_not_double_count_tokens(self):
        trace = self.parse(
            event("step_start"), event("text", text="Answer"), finish(), finish()
        )
        self.assertEqual(trace.usage.input_tokens, 105)
        self.assertTrue(trace.turn_completed)

    def test_tool_ids_are_scoped_to_each_model_message(self):
        events = []
        for message in ("first", "second"):
            events.extend(
                (
                    event("step_start", message),
                    event(
                        "tool_use",
                        message,
                        tool="read",
                        callID="reused-id",
                        state={
                            "status": "completed",
                            "input": {"filePath": "123.md"},
                            "output": "fixture",
                        },
                    ),
                    event("text", message, text="Answer"),
                    finish(
                        message, reason="tool-calls" if message == "first" else "stop"
                    ),
                )
            )
        self.assertEqual(len(self.parse(*events).tool_calls), 2)

    def test_hard_timeout_interrupts_active_process_and_blocked_stdin(self):
        binary = self.path.parent / "active-process"
        binary.write_text(
            f"#!{sys.executable}\nimport sys, time\n"
            "while True:\n    print('active', file=sys.stderr, flush=True)\n    time.sleep(0.05)\n"
        )
        binary.chmod(0o755)
        result = run_prompt(
            binary,
            home=self.path.parent,
            workspace=self.path.parent,
            model="fixture",
            prompt="x" * (1024 * 1024),
            output_dir=self.path.parent / "output",
            idle_timeout_seconds=10,
            timeout_seconds=1,
        )
        self.assertTrue(result["idle_killed"])
        self.assertLess(result["wall_seconds"], 5)
        self.assertFalse(result["trace"].turn_completed)
        self.assertEqual(
            _classify_attempt(
                exit_code=result["exit_code"],
                idle_killed=result["idle_killed"],
                trace=result["trace"],
                stderr="",
            ),
            ("infrastructure_failed", True),
        )

    def test_failed_or_nonfinal_finish_never_completes(self):
        for reason in ("tool-calls", "length", "content-filter", "error", "unknown"):
            with self.subTest(reason=reason):
                trace = self.parse(
                    event("step_start"),
                    event("text", text="Partial"),
                    finish(reason=reason),
                )
                self.assertFalse(trace.turn_completed)
                self.assertEqual(trace.final_response, "")
                self.assertEqual(
                    _classify_attempt(
                        exit_code=0, idle_killed=False, trace=trace, stderr=""
                    ),
                    ("failed", False),
                )
        trace = self.parse(
            event("step_start"),
            event("text", text="Answer"),
            finish(),
            {"type": "error", "error": {"message": "provider failed"}},
        )
        self.assertFalse(trace.turn_completed)
        self.assertIn("provider failed", trace.errors[0])

    def test_error_after_stop_and_unfinished_new_step_invalidate_completion(self):
        trace = self.parse(
            event("step_start"),
            event("text", text="Old answer"),
            finish(),
            event("step_start", "message-2"),
        )
        self.assertFalse(trace.turn_completed)
        self.assertIn("unfinished OpenCode model step", trace.errors)

    def test_missing_usage_and_foreign_session_fail_closed(self):
        broken = finish()
        broken["part"].pop("tokens")
        trace = self.parse(event("text", text="Answer"), broken)
        self.assertFalse(trace.turn_completed)
        foreign = finish()
        foreign["sessionID"] = "child-session"
        trace = self.parse(event("step_start"), event("text", text="Answer"), foreign)
        self.assertFalse(trace.turn_completed)
        self.assertIn("multiple OpenCode sessions in one attempt", trace.errors)

    def test_tools_and_batch_grouping_preserve_native_and_mcp_evidence(self):
        read = event(
            "tool_use",
            tool="read",
            callID="read-1",
            state={
                "status": "completed",
                "input": {"filePath": "/corpus/123.md"},
                "output": "evidence",
                "time": {"start": 1000, "end": 1500},
            },
        )
        mcp = event(
            "tool_use",
            tool="zvec_grep_semantic_search",
            callID="search-1",
            state={
                "status": "completed",
                "input": {"query": "question"},
                "output": "456.md:1 gold evidence",
            },
        )
        bash = event(
            "tool_use",
            "message-2",
            tool="bash",
            callID="bash-1",
            state={
                "status": "completed",
                "input": {"command": "cat 789.md"},
                "output": "last",
            },
        )
        trace = self.parse(
            event("step_start"),
            read,
            mcp,
            finish(reason="tool-calls"),
            event("step_start", "message-2"),
            bash,
            event("text", "message-2", text="Answer"),
            finish("message-2"),
        )
        self.assertEqual(
            [tool.name for tool in trace.tool_calls],
            ["read", "zvec_grep_semantic_search", "command_execution"],
        )
        self.assertEqual(
            [tool.kind for tool in trace.tool_calls], ["builtin", "mcp", "command"]
        )
        self.assertEqual(trace.tool_calls[0].duration_seconds, 0.5)
        self.assertEqual(trace.tool_calls[2].arguments, "cat 789.md")
        self.assertEqual(trace.observed_docids, ("123", "456", "789"))
        self.assertEqual(
            _tool_interaction_batches(self.path, evidence={"123"}, gold={"789"}),
            {
                "total": 2,
                "first_evidence": 1,
                "after_first_evidence": 1,
                "first_gold": 2,
                "after_first_gold": 0,
            },
        )

    def test_invalid_json_cannot_hide_behind_persisted_answer(self):
        self.parse(event("step_start"), event("text", text="Answer"), finish())
        with self.path.open("a") as destination:
            destination.write("broken-event\n")
        final = self.path.parent / "response.md"
        final.write_text("A stale answer")
        trace = parse_trace(self.path, final)
        self.assertFalse(trace.turn_completed)
        self.assertEqual(trace.final_response, "")


@unittest.skipUnless(
    os.environ.get("OPENCODE_BENCHMARK_TEST_BINARY"),
    "requires pinned OpenCode binary; loopback HTTP requests only",
)
class OpenCodeBinaryContractTests(unittest.TestCase):
    def test_real_binary_matches_request_trace_and_no_tool_judge_contract(self):
        configured_binary = os.environ["OPENCODE_BENCHMARK_TEST_BINARY"]
        resolved_binary = shutil.which(configured_binary)
        self.assertIsNotNone(resolved_binary, f"OpenCode not found: {configured_binary}")
        binary = Path(resolved_binary).resolve()
        self.assertEqual(
            subprocess.check_output(
                [binary, "--version"], text=True, timeout=10
            ).strip(),
            "1.18.4",
        )
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            workspace = root / "corpus"
            workspace.mkdir()
            (workspace / "123.md").write_text("The fixture value is 42.\n")
            requests = []
            fixture_key = "offline-fixture-not-a-real-key"

            class Provider(BaseHTTPRequestHandler):
                def log_message(self, *_args):
                    pass

                def do_POST(self):
                    if self.headers.get("Authorization") != f"Bearer {fixture_key}":
                        self.send_error(401)
                        return
                    body = json.loads(
                        self.rfile.read(int(self.headers["Content-Length"]))
                    )
                    requests.append(body)
                    after_tool = any(
                        message.get("role") == "tool"
                        for message in body.get("messages", [])
                    )
                    if body.get("tools") and not after_tool:
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
                                            {"filePath": str(workspace / "123.md")}
                                        ),
                                    },
                                }
                            ],
                        }
                        reason = "tool_calls"
                    else:
                        delta = {
                            "role": "assistant",
                            "content": "The fixture value is 42.",
                        }
                        reason = "stop"
                    envelope = {
                        "id": "fixture",
                        "object": "chat.completion.chunk",
                        "created": 1,
                        "model": body["model"],
                    }
                    chunks = [
                        {
                            **envelope,
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {
                                        "role": "assistant",
                                        "reasoning_content": "fixture-reasoning",
                                    },
                                    "finish_reason": None,
                                }
                            ],
                        },
                        {
                            **envelope,
                            "choices": [
                                {"index": 0, "delta": delta, "finish_reason": None}
                            ],
                        },
                        {
                            **envelope,
                            "choices": [
                                {"index": 0, "delta": {}, "finish_reason": reason}
                            ],
                            "usage": {
                                "prompt_tokens": 100,
                                "completion_tokens": 50,
                                "prompt_tokens_details": {"cached_tokens": 60},
                                "completion_tokens_details": {"reasoning_tokens": 20},
                            },
                        },
                    ]
                    payload = (
                        "".join(
                            "data: " + json.dumps(chunk) + "\n\n" for chunk in chunks
                        )
                        + "data: [DONE]\n\n"
                    ).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)

            server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
            server.daemon_threads = True
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                for judge in (False, True):
                    with (
                        self.subTest(judge=judge),
                        patch.dict(
                            os.environ,
                            {
                                "OPENAI_API_KEY": fixture_key,
                                "BROWSE_COMP_MODEL_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
                            },
                        ),
                    ):
                        requests.clear()
                        home = root / ("judge-home" if judge else "agent-home")
                        home.mkdir()
                        config = load_config()
                        (home / "opencode.json").write_text(
                            json.dumps(opencode_configuration(config, judge=judge))
                        )
                        output = root / ("judge-output" if judge else "agent-output")
                        result = run_prompt(
                            binary,
                            home=home,
                            workspace=workspace,
                            model=config.run.model,
                            prompt="Read 123.md and report the fixture value.",
                            output_dir=output,
                            timeout_seconds=60,
                        )
                        self.assertEqual(
                            result["exit_code"], 0, (output / "stderr.log").read_text()
                        )
                        self.assertFalse(result["idle_killed"])
                        trace = result["trace"]
                        self.assertTrue(trace.turn_completed, trace.errors)
                        self.assertEqual(
                            trace.final_response, "The fixture value is 42."
                        )
                        self.assertEqual(
                            (output / "response.md").read_text(), trace.final_response
                        )
                        steps = 1 if judge else 2
                        self.assertEqual(
                            len(requests), steps, "Unexpected background model request"
                        )
                        self.assertEqual(trace.usage.input_tokens, 100 * steps)
                        self.assertEqual(trace.usage.cached_input_tokens, 60 * steps)
                        self.assertEqual(trace.usage.output_tokens, 50 * steps)
                        self.assertEqual(
                            trace.usage.reasoning_output_tokens, 20 * steps
                        )
                        self.assertEqual(len(trace.tool_calls), 0 if judge else 1)
                        raw = [
                            json.loads(line)
                            for line in (output / "events.jsonl")
                            .read_text()
                            .splitlines()
                        ]
                        self.assertEqual(
                            sum(item["type"] == "step_finish" for item in raw), steps
                        )
                        self.assertTrue(
                            any(item["type"] == "reasoning" for item in raw)
                        )
                        for request in requests:
                            self.assertEqual(request["model"], "qwen3.8-max")
                            self.assertEqual(request.get("temperature"), 0)
                            self.assertEqual(request.get("seed"), 42)
                            self.assertTrue(request.get("enable_thinking"))
                            self.assertEqual(request.get("reasoning_effort"), "high")
                            self.assertEqual(request.get("max_tokens"), 32000)
                            names = {
                                item["function"]["name"]
                                for item in request.get("tools", [])
                            }
                            self.assertFalse(
                                names
                                & {"task", "webfetch", "websearch", "skill", "question"}
                            )
                            if judge:
                                self.assertFalse(names)
                        if not judge:
                            assistants = [
                                message
                                for message in requests[-1]["messages"]
                                if message.get("tool_calls")
                            ]
                            self.assertEqual(
                                assistants[0].get("reasoning_content"),
                                "fixture-reasoning",
                            )
                            self.assertEqual(trace.observed_docids, ("123",))
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=5)
