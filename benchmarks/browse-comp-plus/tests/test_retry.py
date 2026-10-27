from __future__ import annotations

import json
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path

from zg_bench.models import TraceSummary
from zg_bench.opencode import _classify_attempt
from zg_bench.trace import parse_trace


class OpenCodeRetryTests(unittest.TestCase):
    def classify(self, error=None, *, stderr="", idle_killed=False):
        with tempfile.TemporaryDirectory() as directory:
            events = Path(directory) / "events.jsonl"
            events.write_text(
                json.dumps(
                    {"type": "error", "sessionID": "fixture", "error": error}
                )
                + "\n"
                if error is not None
                else ""
            )
            trace = parse_trace(events)
        return _classify_attempt(
            exit_code=1, idle_killed=idle_killed, trace=trace, stderr=stderr
        )

    def test_temporary_http_errors_are_retryable_without_matching_old_keywords(self):
        for message in (
            "429 Too Many Requests",
            "500 Internal Server Error",
            "502 Bad Gateway",
            "HTTP/1.1 503",
            "HTTP error 504",
            "status code: 529",
            "status_code=500",
        ):
            with self.subTest(message=message):
                self.assertEqual(
                    self.classify(stderr=message), ("infrastructure_failed", True)
                )

    def test_structured_provider_retry_flag_and_status_survive_jsonl_parsing(self):
        for data in (
            {"message": "upstream unavailable", "isRetryable": True},
            {"message": "request rejected", "statusCode": 429},
            {"message": "request rejected", "statusCode": 500},
            {"cause": {"status": 502}},
            {"cause": {"isRetryable": True}},
        ):
            with self.subTest(data=data):
                self.assertEqual(
                    self.classify({"name": "APIError", "data": data}),
                    ("infrastructure_failed", True),
                )

    def test_explicit_nonretryable_provider_error_is_respected(self):
        self.assertEqual(
            self.classify(
                {
                    "name": "APIError",
                    "data": {
                        "message": "500 Internal Server Error",
                        "statusCode": 500,
                        "isRetryable": False,
                    },
                }
            ),
            ("failed", False),
        )

    def test_exhausted_quota_is_terminal_even_when_provider_marks_429_retryable(self):
        for code in (
            "insufficient_quota",
            "quota_exhausted",
            "insufficient_balance",
            "FreeAllocatedQuotaExceeded",
        ):
            with self.subTest(code=code):
                self.assertEqual(
                    self.classify(
                        {
                            "name": "APIError",
                            "data": {
                                "message": "rate limit",
                                "statusCode": 429,
                                "isRetryable": True,
                                "responseBody": json.dumps({"error": {"code": code}}),
                            },
                        }
                    ),
                    ("failed", False),
                )

    def test_usage_limit_messages_are_terminal_even_if_process_was_killed(self):
        for message in (
            "ApiUsageLimitError: usage limit reached",
            "HTTP 429: You exceeded your current quota",
            "HTTP 429: billing hard limit reached",
        ):
            with self.subTest(message=message):
                self.assertEqual(
                    self.classify(stderr=message, idle_killed=True),
                    ("failed", False),
                )

    def test_authentication_and_invalid_requests_do_not_retry(self):
        for status in (400, 401, 403, 404):
            with self.subTest(status=status):
                self.assertEqual(
                    self.classify(
                        {"name": "APIError", "data": {"statusCode": status}}
                    ),
                    ("failed", False),
                )
        self.assertEqual(
            self.classify(stderr="expected 500 documents, found 429"),
            ("failed", False),
        )

    def test_a_completed_answer_never_retries_for_quality_or_stderr_warnings(self):
        trace = TraceSummary(
            thread_id="fixture",
            final_response="An incorrect answer",
            last_agent_message="An incorrect answer",
            turn_completed=True,
            usage=None,
            tool_calls=(),
            observed_docids=(),
        )
        for response in ("An incorrect answer", "I could not determine the answer."):
            with self.subTest(response=response):
                self.assertEqual(
                    _classify_attempt(
                        exit_code=0,
                        idle_killed=False,
                        trace=replace(trace, final_response=response),
                        stderr="earlier request: 429 Too Many Requests",
                    ),
                    ("completed", False),
                )


if __name__ == "__main__":
    unittest.main()
