from __future__ import annotations

import json
import re
from dataclasses import replace
from pathlib import Path
from typing import Any

from .models import ToolCall, TraceSummary, Usage


DOCID_PATTERN = re.compile(
    r"(?<![A-Za-z0-9_.-])(\d+)\.md(?::\d+(?:-\d+)?)?"
)
MAX_SUMMARY_OUTPUT = 8_000


def _text(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, str):
        return value
    return json.dumps(value, ensure_ascii=False)


def mcp_result_text(result: Any) -> str:
    if not isinstance(result, dict):
        return _text(result)
    content = result.get("content")
    if not isinstance(content, list):
        return _text(result)
    values = [
        str(item.get("text", ""))
        for item in content
        if isinstance(item, dict) and item.get("type") == "text"
    ]
    return "\n".join(values)


def extract_docids(*values: Any) -> set[str]:
    return {
        match
        for value in values
        for match in DOCID_PATTERN.findall(_text(value))
    }


def _tool_call(item: dict[str, Any], *, completed: bool) -> ToolCall | None:
    item_type = str(item.get("type", "unknown"))
    status = item.get("status") if completed else "in_progress"
    if item_type == "command_execution":
        output = str(item.get("aggregated_output", "")) if completed else None
        return ToolCall(
            kind="command",
            name="command_execution",
            arguments=item.get("command"),
            output=output[:MAX_SUMMARY_OUTPUT] if output is not None else None,
            status=status,
        )
    if item_type == "mcp_tool_call":
        output = mcp_result_text(item.get("result")) if completed else None
        return ToolCall(
            kind="mcp",
            name=str(item.get("tool", "unknown")),
            arguments=item.get("arguments"),
            output=output[:MAX_SUMMARY_OUTPUT] if output is not None else None,
            status=status,
        )
    return None


def parse_trace(path: Path, final_path: Path | None = None) -> TraceSummary:
    thread_id: str | None = None
    last_agent_message = ""
    turn_completed = False
    usage: Usage | None = None
    calls: dict[str, ToolCall] = {}
    call_order: list[str] = []
    completed_calls: set[str] = set()
    docids: set[str] = set()
    errors: list[str] = []

    with path.open(encoding="utf-8", errors="replace") as source:
        for number, line in enumerate(source, 1):
            if not line.strip():
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError as error:
                errors.append(f"invalid JSONL event at line {number}: {error}")
                continue
            event_type = event.get("type")
            if event_type == "thread.started":
                thread_id = event.get("thread_id")
            elif event_type == "turn.completed":
                turn_completed = True
                raw = event.get("usage") or {}
                usage = Usage(
                    input_tokens=int(raw.get("input_tokens", 0)),
                    cached_input_tokens=int(raw.get("cached_input_tokens", 0)),
                    output_tokens=int(raw.get("output_tokens", 0)),
                    reasoning_output_tokens=int(raw.get("reasoning_output_tokens", 0)),
                )
            elif event_type in {"error", "turn.failed"}:
                errors.append(
                    _text(event.get("message") or event.get("error") or event)
                )
            if event_type == "item.started":
                item = event.get("item") or {}
                call = _tool_call(item, completed=False)
                if call is not None:
                    call_id = str(item.get("id") or f"started:{number}")
                    if call_id not in calls:
                        calls[call_id] = call
                        call_order.append(call_id)
                continue
            if event_type != "item.completed":
                continue
            item = event.get("item") or {}
            item_type = str(item.get("type", "unknown"))
            if item_type == "agent_message":
                last_agent_message = str(item.get("text", last_agent_message))
                continue
            if item_type == "command_execution":
                output = str(item.get("aggregated_output", ""))
                docids.update(extract_docids(item.get("command"), output))
            elif item_type == "mcp_tool_call":
                output = mcp_result_text(item.get("result"))
                docids.update(extract_docids(output))
            else:
                continue
            call = _tool_call(item, completed=True)
            assert call is not None
            call_id = str(item.get("id") or f"completed:{number}")
            if call_id not in calls:
                call_order.append(call_id)
            calls[call_id] = call
            completed_calls.add(call_id)

    final_response = last_agent_message if turn_completed else ""
    if turn_completed and final_path and final_path.is_file():
        persisted = final_path.read_text(encoding="utf-8", errors="replace").strip()
        if persisted:
            final_response = persisted
    return TraceSummary(
        thread_id=thread_id,
        final_response=final_response,
        last_agent_message=last_agent_message,
        turn_completed=turn_completed,
        usage=usage,
        tool_calls=tuple(
            calls[call_id]
            if call_id in completed_calls
            else replace(calls[call_id], status="unclosed")
            for call_id in call_order
        ),
        observed_docids=tuple(sorted(docids)),
        errors=tuple(errors),
    )
