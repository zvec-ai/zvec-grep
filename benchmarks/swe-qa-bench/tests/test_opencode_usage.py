from __future__ import annotations

import json
import sqlite3
import unittest

from zg_bench.agents.opencode_usage import USAGE_SQL, summarize_session_usage


def session(sid: str, parent: str | None = None) -> dict:
    return {"kind": "session", "data": {"id": sid, "parent_id": parent}}


def message(mid: str, sid: str, **changes) -> dict:
    return {"kind": "message", "data": {
        "id": mid, "session_id": sid, "provider_id": "test", "model_id": "mock",
        "input": 40, "output": 30, "reasoning": 20, "cache_read": 60,
        "cache_write": 0, "tokens_present": True, "finished": True,
        "has_error": False, "cost_usd": 0, **changes,
    }}


def tool(tid: str, sid: str, mid: str, child: str | None = None, **changes) -> dict:
    return {"kind": "tool", "data": {
        "id": tid, "call_id": tid, "session_id": sid, "message_id": mid,
        "name": "task" if child else "read", "status": "completed",
        "child_session_id": child, **changes,
    }}


class SessionUsageTests(unittest.TestCase):
    def test_nested_resumed_sessions_and_duplicate_snapshot_count_once(self):
        rows = [
            session("root"), session("child", "root"), session("grand", "child"),
            message("m1", "root"), message("m2", "child"), message("m3", "grand"),
            message("m4", "child"),  # resumed child gets a new assistant message
            tool("t1", "root", "m1", "child"), tool("t2", "root", "m1", "child"),
            tool("t3", "child", "m2", "grand"), tool("t4", "grand", "m3"),
            session("unrelated"), message("other", "unrelated", input=10000),
            tool("other-tool", "unrelated", "other"),
        ]
        report = summarize_session_usage(rows + rows, "root")
        self.assertTrue(report["complete"], report["errors"])
        self.assertEqual(len(report["sessions"]), 3)
        self.assertEqual(report["total"]["llm_calls"], 4)
        self.assertEqual(report["total"]["tool_calls"], 4)
        self.assertEqual(report["total"]["input_tokens"], 400)
        self.assertEqual(report["total"]["output_tokens"], 200)
        self.assertEqual(report["root"]["input_tokens"], 100)
        self.assertEqual(report["descendants"]["input_tokens"], 300)
        self.assertEqual(report["total"]["uncached_input_tokens"], 160)
        self.assertEqual(report["total"]["cache_read_tokens"], 240)
        self.assertEqual(report["total"]["text_output_tokens"], 120)
        self.assertEqual(report["total"]["reasoning_tokens"], 80)
        self.assertIsNone(report["total"]["cost_usd"])

    def test_cache_write_and_nonzero_cost(self):
        report = summarize_session_usage([
            session("r"), message("m", "r", cache_write=12, cost_usd=0.15),
        ], "r")
        self.assertEqual(report["total"]["input_tokens"], 112)
        self.assertEqual(report["total"]["cache_write_tokens"], 12)
        self.assertEqual(report["total"]["cost_usd"], 0.15)

    def test_missing_child_or_unfinished_capture_is_incomplete(self):
        rows = [session("r"), message("m", "r"), tool("t", "r", "m", "missing")]
        report = summarize_session_usage(rows, "r")
        self.assertFalse(report["complete"])
        self.assertIn("missing_child_session:missing", report["errors"])
        self.assertEqual(report["total"]["input_tokens"], 100)
        report = summarize_session_usage(rows[:2], "r", capture_complete=False)
        self.assertFalse(report["complete"])
        self.assertIn("capture_incomplete", report["errors"])

    def test_existing_but_empty_delegated_session_is_incomplete(self):
        report = summarize_session_usage([
            session("r"), session("child", "r"), message("m", "r"),
            tool("t", "r", "m", "child"),
        ], "r")
        self.assertFalse(report["complete"])
        self.assertIn("missing_session_messages:child", report["errors"])
        self.assertEqual(report["total"]["llm_calls"], 1)
        self.assertEqual(report["total"]["input_tokens"], 100)
        report = summarize_session_usage([session("empty-root")], "empty-root")
        self.assertFalse(report["complete"])
        self.assertIn("missing_session_messages:empty-root", report["errors"])

    def test_partial_cost_is_unknown_within_session_and_across_sessions(self):
        for missing_cost in (0, None):
            with self.subTest(missing_cost=missing_cost):
                report = summarize_session_usage([
                    session("r"), message("m", "r", cost_usd=0.15),
                    message("m2", "r", cost_usd=missing_cost),
                ], "r")
                self.assertTrue(report["complete"], report["errors"])
                self.assertIsNone(report["root"]["cost_usd"])
                self.assertEqual(report["descendants"]["cost_usd"], 0)
                self.assertIsNone(report["total"]["cost_usd"])
                report = summarize_session_usage([
                    session("r"), session("child", "r"),
                    message("m", "r", cost_usd=0.15),
                    message("m2", "child", cost_usd=missing_cost),
                    tool("t", "r", "m", "child"),
                ], "r")
                self.assertTrue(report["complete"], report["errors"])
                self.assertEqual(report["root"]["cost_usd"], 0.15)
                self.assertIsNone(report["descendants"]["cost_usd"])
                self.assertIsNone(report["total"]["cost_usd"])

    def test_empty_descendants_have_zero_cost(self):
        report = summarize_session_usage([
            session("r"), message("m", "r", cost_usd=0.15),
        ], "r")
        self.assertTrue(report["complete"], report["errors"])
        self.assertEqual(report["descendants"]["cost_usd"], 0)
        self.assertEqual(report["total"]["cost_usd"], 0.15)

    def test_missing_root_conflicting_rows_and_unfinished_messages(self):
        self.assertFalse(summarize_session_usage([], "r")["complete"])
        report = summarize_session_usage([
            session("r"), message("m", "r", finished=False), message("m", "r"),
        ], "r")
        self.assertFalse(report["complete"])
        self.assertIn("conflicting_message:m", report["errors"])
        self.assertIn("unfinished_message:m", report["errors"])

    def test_child_reference_cannot_pull_in_an_unrelated_root(self):
        report = summarize_session_usage([
            session("r"), session("other"), message("m", "r"),
            message("om", "other"), tool("t", "r", "m", "other"),
        ], "r")
        self.assertFalse(report["complete"])
        self.assertEqual(report["total"]["llm_calls"], 1)
        self.assertIn("child_outside_session_tree:other", report["errors"])

    def test_sql_filters_private_content_and_ignores_step_finish_duplicates(self):
        db = sqlite3.connect(":memory:")
        db.row_factory = sqlite3.Row
        db.executescript("""
            CREATE TABLE session(id TEXT, parent_id TEXT);
            CREATE TABLE message(id TEXT, session_id TEXT, data TEXT);
            CREATE TABLE part(id TEXT, session_id TEXT, message_id TEXT, data TEXT);
        """)
        db.execute("INSERT INTO session VALUES ('r', NULL)")
        usage = {"input": 40, "output": 30, "reasoning": 20,
                 "cache": {"read": 60, "write": 0}}
        db.execute("INSERT INTO message VALUES (?, ?, ?)", ("m", "r", json.dumps({
            "role": "assistant", "providerID": "test", "modelID": "mock", "cost": 0,
            "tokens": usage, "time": {"completed": 123}, "text": "PRIVATE_MESSAGE",
        })))
        db.execute("INSERT INTO message VALUES (?, ?, ?)", ("u", "r", json.dumps({
            "role": "user", "text": "PRIVATE_USER",
        })))
        for pid, data in [
            ("t", {"type": "tool", "tool": "read", "callID": "c", "state": {
                "status": "completed", "input": "PRIVATE_INPUT", "output": "PRIVATE_OUTPUT",
            }}),
            ("finish", {"type": "step-finish", "tokens": usage}),
            ("reasoning", {"type": "reasoning", "text": "PRIVATE_REASONING"}),
        ]:
            db.execute("INSERT INTO part VALUES (?, ?, ?, ?)", (pid, "r", "m", json.dumps(data)))
        rows = [dict(row) for row in db.execute(USAGE_SQL)]
        self.assertNotIn("PRIVATE_", json.dumps(rows))
        report = summarize_session_usage(rows, "r")
        self.assertTrue(report["complete"], report["errors"])
        self.assertEqual(report["total"]["input_tokens"], 100)
        self.assertEqual(report["total"]["output_tokens"], 50)
        self.assertEqual(report["total"]["tool_calls"], 1)
        self.assertEqual(report["total"]["llm_calls"], 1)


if __name__ == "__main__":
    unittest.main()
