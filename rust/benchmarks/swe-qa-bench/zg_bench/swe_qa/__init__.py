"""SWE-QA-Bench collection, validation, judging, and reporting helpers."""

from __future__ import annotations


class SweQaError(RuntimeError):
    """A user-facing failure in the SWE-QA benchmark pipeline."""


SELF_JUDGE_LABEL = "glm-5.2-self-judge-v1"
