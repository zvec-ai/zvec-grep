from __future__ import annotations

from dataclasses import asdict, dataclass, field
from typing import Any, Literal


Profile = Literal["baseline", "zvec-grep"]
PROFILES: tuple[Profile, ...] = ("baseline", "zvec-grep")


@dataclass(frozen=True)
class Usage:
    input_tokens: int = 0
    cached_input_tokens: int = 0
    output_tokens: int = 0
    reasoning_output_tokens: int = 0

    @property
    def total_tokens(self) -> int:
        return self.input_tokens + self.output_tokens

    def to_dict(self) -> dict[str, int]:
        return {**asdict(self), "total_tokens": self.total_tokens}


@dataclass(frozen=True)
class ToolCall:
    kind: str
    name: str
    arguments: Any = None
    output: str | None = None
    status: str | None = None
    duration_seconds: float | None = None

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class TraceSummary:
    thread_id: str | None
    final_response: str
    last_agent_message: str
    turn_completed: bool
    usage: Usage | None
    tool_calls: tuple[ToolCall, ...]
    observed_docids: tuple[str, ...]
    errors: tuple[str, ...] = ()

    def to_dict(self) -> dict[str, Any]:
        return {
            "thread_id": self.thread_id,
            "final_response": self.final_response,
            "last_agent_message": self.last_agent_message,
            "turn_completed": self.turn_completed,
            "usage": self.usage.to_dict() if self.usage else None,
            "tool_calls": [call.to_dict() for call in self.tool_calls],
            "observed_docids": list(self.observed_docids),
            "errors": list(self.errors),
        }


@dataclass
class AttemptResult:
    query_id: str
    profile: Profile
    trial_index: int
    status: str
    attempt: int
    started_at: str
    finished_at: str
    wall_seconds: float
    exit_code: int
    infrastructure_failure: bool
    trace: TraceSummary
    paths: dict[str, str] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "query_id": self.query_id,
            "profile": self.profile,
            "trial_index": self.trial_index,
            "status": self.status,
            "attempt": self.attempt,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "wall_seconds": self.wall_seconds,
            "exit_code": self.exit_code,
            "infrastructure_failure": self.infrastructure_failure,
            "trace": self.trace.to_dict(),
            "paths": self.paths,
        }
