"""Account for OpenCode's stored session tree without exporting message text.

OpenCode 1.18.4 stores per-request usage on assistant messages and repeats it
on step-finish parts. Only messages contribute tokens; only tool parts
contribute tool counts. Title/background requests not persisted as messages
are outside this explicitly named session-tree scope.
"""

from __future__ import annotations

import json
import math
from collections import defaultdict
from typing import Any


# Pass as one argv argument to `opencode db ... --format json`. JSON objects
# keep the three row types distinct without exposing content, tool arguments,
# tool output, reasoning text, or credentials from the local database.
USAGE_SQL = """
SELECT 'session' AS kind,
       json_object('id', id, 'parent_id', parent_id) AS data
FROM session
UNION ALL
SELECT 'message' AS kind,
       json_object(
           'id', id, 'session_id', session_id,
           'provider_id', json_extract(data, '$.providerID'),
           'model_id', json_extract(data, '$.modelID'),
           'cost_usd', json_extract(data, '$.cost'),
           'input', json_extract(data, '$.tokens.input'),
           'output', json_extract(data, '$.tokens.output'),
           'reasoning', json_extract(data, '$.tokens.reasoning'),
           'cache_read', json_extract(data, '$.tokens.cache.read'),
           'cache_write', json_extract(data, '$.tokens.cache.write'),
           'tokens_present', json_type(data, '$.tokens') = 'object',
           'finished', json_extract(data, '$.time.completed') IS NOT NULL,
           'has_error', COALESCE(json_type(data, '$.error') != 'null', 0)
       ) AS data
FROM message
WHERE json_extract(data, '$.role') = 'assistant'
UNION ALL
SELECT 'tool' AS kind,
       json_object(
           'id', id, 'call_id', json_extract(data, '$.callID'),
           'session_id', session_id, 'message_id', message_id,
           'name', json_extract(data, '$.tool'),
           'status', json_extract(data, '$.state.status'),
           'child_session_id', json_extract(data, '$.state.metadata.sessionId')
       ) AS data
FROM part
WHERE json_extract(data, '$.type') = 'tool'
""".strip()

_COUNTS = (
    "input_tokens", "output_tokens", "text_output_tokens", "reasoning_tokens",
    "cache_read_tokens", "cache_write_tokens", "uncached_input_tokens",
    "tool_calls", "llm_calls",
)


def _empty_metrics() -> dict[str, Any]:
    return {**dict.fromkeys(_COUNTS, 0), "cost_usd": 0}


def _sum_metrics(rows: list[dict[str, Any]]) -> dict[str, Any]:
    result = _empty_metrics()
    for name in _COUNTS:
        result[name] = sum(row[name] for row in rows)
    costs = [row["cost_usd"] for row in rows]
    result["cost_usd"] = None if any(cost is None for cost in costs) else sum(costs)
    return result


def summarize_session_usage(
    snapshot: list[dict[str, Any]],
    root_session_id: str,
    *,
    capture_complete: bool = True,
) -> dict[str, Any]:
    """Summarize one root and all its descendants, de-duplicating stored IDs.

    ``snapshot`` is the JSON result of :data:`USAGE_SQL`. Missing/unfinished
    evidence makes ``complete`` false, while known usage is still returned.
    ``output_tokens`` includes reasoning; ``input_tokens`` includes cache.
    An all-zero/unavailable stored cost is unknown, not a free model call.
    """
    errors: list[str] = []
    rows: dict[str, dict[str, dict[str, Any]]] = {
        "session": {}, "message": {}, "tool": {},
    }
    if not capture_complete:
        errors.append("capture_incomplete")
    if not isinstance(snapshot, list):
        errors.append("invalid_snapshot")
        snapshot = []
    for index, row in enumerate(snapshot):
        try:
            kind = row["kind"]
            data = row["data"]
            if isinstance(data, str):
                data = json.loads(data)
            if kind not in rows or not isinstance(data, dict):
                raise ValueError("invalid row")
            identifier = data["id"]
            if not isinstance(identifier, str) or not identifier:
                raise ValueError("invalid id")
            previous = rows[kind].get(identifier)
            if previous is not None and previous != data:
                errors.append(f"conflicting_{kind}:{identifier}")
            else:
                rows[kind][identifier] = data
        except (KeyError, TypeError, ValueError):
            errors.append(f"invalid_snapshot_row:{index}")

    children: dict[str, list[str]] = defaultdict(list)
    for sid, session in rows["session"].items():
        if session.get("parent_id"):
            children[session["parent_id"]].append(sid)
    selected: list[str] = []
    seen: set[str] = set()
    pending = [root_session_id]
    if root_session_id not in rows["session"]:
        errors.append(f"missing_root_session:{root_session_id}")
        pending = []
    while pending:
        sid = pending.pop(0)
        if sid in seen:
            errors.append(f"session_cycle:{sid}")
            continue
        seen.add(sid)
        selected.append(sid)
        pending.extend(sorted(children[sid]))

    def number(value: Any, label: str, *, optional: bool = False) -> int | float:
        if value is None and optional:
            return 0
        if (
            isinstance(value, bool)
            or not isinstance(value, (int, float))
            or not math.isfinite(value)
            or value < 0
        ):
            errors.append(f"invalid_usage:{label}")
            return 0
        return value

    sessions = []
    for sid in selected:
        metrics = _empty_metrics()
        messages = [m for m in rows["message"].values() if m.get("session_id") == sid]
        tools = [p for p in rows["tool"].values() if p.get("session_id") == sid]
        # A row in `session` only proves creation, not that the requested
        # delegate's usage was captured. Never certify such an empty export.
        if not messages:
            errors.append(f"missing_session_messages:{sid}")
        message_ids = {m["id"] for m in messages}
        costs = []
        for message in messages:
            mid = message["id"]
            if not message.get("tokens_present"):
                errors.append(f"missing_message_usage:{mid}")
            if not message.get("finished"):
                errors.append(f"unfinished_message:{mid}")
            if message.get("has_error"):
                errors.append(f"message_error:{mid}")
            uncached = number(message.get("input"), f"{mid}:input")
            output = number(message.get("output"), f"{mid}:output")
            reasoning = number(message.get("reasoning"), f"{mid}:reasoning")
            cache_read = number(message.get("cache_read"), f"{mid}:cache_read")
            cache_write = number(message.get("cache_write"), f"{mid}:cache_write")
            metrics["uncached_input_tokens"] += uncached
            metrics["cache_read_tokens"] += cache_read
            metrics["cache_write_tokens"] += cache_write
            metrics["input_tokens"] += uncached + cache_read + cache_write
            metrics["text_output_tokens"] += output
            metrics["reasoning_tokens"] += reasoning
            metrics["output_tokens"] += output + reasoning
            costs.append(number(message.get("cost_usd"), f"{mid}:cost", optional=True))
        metrics["llm_calls"] = len(messages)
        metrics["tool_calls"] = len(tools)
        # Zero is also OpenCode's value for unpriced custom providers. A
        # partial known subtotal must not masquerade as the complete cost.
        # Empty scopes contribute zero; scopes containing unpriced messages
        # propagate unknown through every enclosing total.
        metrics["cost_usd"] = None if any(cost == 0 for cost in costs) else sum(costs)
        for tool in tools:
            tid = tool["id"]
            if tool.get("message_id") not in message_ids:
                errors.append(f"missing_tool_message:{tid}")
            if tool.get("status") not in ("completed", "error"):
                errors.append(f"unfinished_tool:{tid}")
            if tool.get("name") == "task":
                child = tool.get("child_session_id")
                if not child and tool.get("status") == "completed":
                    errors.append(f"missing_task_child_reference:{tid}")
                elif child and child not in rows["session"]:
                    errors.append(f"missing_child_session:{child}")
                elif child and child not in seen:
                    errors.append(f"child_outside_session_tree:{child}")
        sessions.append({
            "session_id": sid,
            "parent_session_id": rows["session"][sid].get("parent_id"),
            **metrics,
            "message_ids": sorted(message_ids),
            "tool_ids": sorted(p["id"] for p in tools),
            "provider_models": [
                {"provider_id": provider, "model_id": model}
                for provider, model in sorted({
                    (m.get("provider_id") or "", m.get("model_id") or "")
                    for m in messages
                })
            ],
        })
    return {
        "schema_version": 1,
        "scope": "opencode-session-tree-v1",
        "complete": not errors,
        "errors": sorted(set(errors)),
        "root_session_id": root_session_id,
        "sessions": sessions,
        "root": _sum_metrics([s for s in sessions if s["session_id"] == root_session_id]),
        "descendants": _sum_metrics([s for s in sessions if s["session_id"] != root_session_id]),
        "total": _sum_metrics(sessions),
    }
