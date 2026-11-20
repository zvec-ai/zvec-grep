from __future__ import annotations

import json
import re
from dataclasses import replace
from pathlib import Path
from typing import Any

from .models import ToolCall, TraceSummary, Usage

DOCID_PATTERN = re.compile(r"(?<![A-Za-z0-9_.-])(\d+)\.md(?::\d+(?:-\d+)?)?")
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
    return {match for value in values for match in DOCID_PATTERN.findall(_text(value))}


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


def _parse_codex_trace(path: Path, final_path: Path | None = None) -> TraceSummary:
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


def opencode_tool_call(part: dict[str, Any]) -> ToolCall:
    """Map native/MCP calls without changing the persisted OpenCode events."""
    state = part.get("state") or {}
    name = str(part.get("tool", "unknown"))
    arguments = state.get("input")
    output = state.get("output", state.get("error"))
    timing = state.get("time") or {}
    duration = None
    if isinstance(timing.get("start"), (int, float)) and isinstance(
        timing.get("end"), (int, float)
    ):
        duration = max(0, timing["end"] - timing["start"]) / 1000
    kind = "mcp" if name.startswith(("zvec_grep_", "zvec-grep_")) else "builtin"
    if name == "bash":
        kind, name = "command", "command_execution"
        arguments = (
            arguments.get("command") if isinstance(arguments, dict) else arguments
        )
    return ToolCall(
        kind=kind,
        name=name,
        arguments=arguments,
        output=_text(output)[:MAX_SUMMARY_OUTPUT] if output is not None else None,
        status=str(state.get("status", "unclosed")),
        duration_seconds=duration,
    )


def _parse_opencode_trace(path: Path) -> TraceSummary:
    session_id: str | None = None
    messages: dict[str, dict[str, str]] = {}
    last_message = ""
    final_message = ""
    last_reason: str | None = None
    started_steps: set[str] = set()
    completed_steps: dict[str, dict[str, Any]] = {}
    tools: dict[str, ToolCall] = {}
    docids: set[str] = set()
    errors: list[str] = []
    totals = {"input": 0, "output": 0, "cache": 0, "reasoning": 0}

    def tokens(value: Any, field: str) -> int:
        if isinstance(value, bool) or not isinstance(value, int) or value < 0:
            errors.append(f"invalid OpenCode token count: {field}")
            return 0
        return value

    with path.open(encoding="utf-8", errors="replace") as source:
        for number, line in enumerate(source, 1):
            if not line.strip():
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError as error:
                errors.append(f"invalid JSONL event at line {number}: {error}")
                continue
            if not isinstance(event, dict):
                errors.append(f"invalid OpenCode event at line {number}")
                continue
            event_type = event.get("type")
            part = event.get("part") or {}
            if not isinstance(part, dict):
                errors.append(f"invalid OpenCode part at line {number}")
                continue
            observed_session = event.get("sessionID") or part.get("sessionID")
            if observed_session:
                if session_id is None:
                    session_id = str(observed_session)
                elif session_id != observed_session:
                    errors.append("multiple OpenCode sessions in one attempt")
                    continue
            message_id = str(part.get("messageID", ""))
            if event_type == "error":
                errors.append(_text(event.get("error") or event))
            elif event_type == "step_start":
                started_steps.add(message_id)
                last_reason = None
            elif event_type == "text":
                messages.setdefault(message_id, {})[str(part.get("id", number))] = str(
                    part.get("text", "")
                )
                last_message = message_id
            elif event_type == "tool_use":
                call_id = (
                    f"{message_id}:{part.get('callID') or part.get('id') or number}"
                )
                tools[call_id] = opencode_tool_call(part)
                state = part.get("state") or {}
                docids.update(extract_docids(state.get("input"), state.get("output")))
            elif event_type == "step_finish":
                if not message_id:
                    errors.append("OpenCode step is missing a message ID")
                    continue
                if message_id in completed_steps:
                    if completed_steps[message_id] != part:
                        errors.append("conflicting duplicate OpenCode step_finish")
                    continue
                completed_steps[message_id] = part
                last_reason = str(part.get("reason", ""))
                final_message = message_id
                raw = part.get("tokens")
                if not isinstance(raw, dict):
                    errors.append("OpenCode step_finish is missing token usage")
                    continue
                cache = raw.get("cache")
                if not isinstance(cache, dict):
                    errors.append("OpenCode step_finish is missing cache usage")
                    continue
                cache_read = tokens(cache.get("read"), "cache.read")
                cache_write = tokens(cache.get("write"), "cache.write")
                reasoning = tokens(raw.get("reasoning"), "reasoning")
                # OpenCode's input/output exclude cache/reasoning. Existing
                # benchmark totals include both; cache.read is an input subset.
                totals["input"] += (
                    tokens(raw.get("input"), "input") + cache_read + cache_write
                )
                totals["output"] += tokens(raw.get("output"), "output") + reasoning
                totals["cache"] += cache_read
                totals["reasoning"] += reasoning

    if started_steps - completed_steps.keys():
        errors.append("unfinished OpenCode model step")
    if any(call.status not in {"completed", "error"} for call in tools.values()):
        errors.append("unfinished OpenCode tool call")
    completed = bool(session_id) and last_reason == "stop" and not errors
    last_agent_message = "\n".join(messages.get(last_message, {}).values())
    final_response = (
        "\n".join(messages.get(final_message, {}).values()) if completed else ""
    )
    return TraceSummary(
        thread_id=session_id,
        final_response=final_response,
        last_agent_message=last_agent_message,
        turn_completed=completed,
        usage=Usage(
            input_tokens=totals["input"],
            cached_input_tokens=totals["cache"],
            output_tokens=totals["output"],
            reasoning_output_tokens=totals["reasoning"],
        )
        if completed_steps
        else None,
        tool_calls=tuple(tools.values()),
        observed_docids=tuple(sorted(docids)),
        errors=tuple(errors),
    )


def parse_trace(path: Path, final_path: Path | None = None) -> TraceSummary:
    """Read raw OpenCode JSONL or historical Codex traces for old reports."""
    with path.open(encoding="utf-8", errors="replace") as source:
        for line in source:
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(event, dict) and (
                event.get("sessionID")
                or event.get("type")
                in {
                    "step_start",
                    "step_finish",
                    "tool_use",
                    "text",
                    "reasoning",
                }
            ):
                return _parse_opencode_trace(path)
    return _parse_codex_trace(path, final_path)
