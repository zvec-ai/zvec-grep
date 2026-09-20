from __future__ import annotations

import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any


BENCHMARK_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CONFIG_PATH = BENCHMARK_ROOT / "benchmark.toml"
DEFAULT_ARTIFACTS_DIR = BENCHMARK_ROOT / "artifacts"
PROMPT_PATH = BENCHMARK_ROOT / "prompts" / "task.md"
SUITES_DIR = BENCHMARK_ROOT / "suites"


class ConfigError(ValueError):
    """Raised when benchmark.toml is incomplete or invalid."""


@dataclass(frozen=True)
class DatasetConfig:
    queries_repo: str
    queries_revision: str
    queries_split: str
    expected_queries: int
    corpus_repo: str
    corpus_revision: str
    corpus_split: str
    expected_corpus_documents: int


@dataclass(frozen=True)
class ZvecGrepConfig:
    embedding: str
    embedding_concurrency: int
    max_filesize: str
    device: str
    mcp_tool_timeout_seconds: int
    server_port: int


@dataclass(frozen=True)
class RunConfig:
    model: str
    reasoning_effort: str
    trials_per_case: int
    infrastructure_retries: int
    idle_timeout_seconds: int


@dataclass(frozen=True)
class BenchmarkConfig:
    dataset: DatasetConfig
    zvec_grep: ZvecGrepConfig
    run: RunConfig
    path: Path


def _table(raw: dict[str, Any], name: str) -> dict[str, Any]:
    value = raw.get(name)
    if not isinstance(value, dict):
        raise ConfigError(f"missing [{name}] table")
    return value


def _build(cls: type[Any], raw: dict[str, Any], section: str) -> Any:
    try:
        return cls(**raw)
    except TypeError as error:
        raise ConfigError(f"invalid [{section}] configuration: {error}") from error


def load_config(path: Path = DEFAULT_CONFIG_PATH) -> BenchmarkConfig:
    path = path.expanduser().resolve()
    try:
        raw = tomllib.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise ConfigError(f"configuration not found: {path}") from error
    except tomllib.TOMLDecodeError as error:
        raise ConfigError(f"invalid TOML in {path}: {error}") from error

    config = BenchmarkConfig(
        dataset=_build(DatasetConfig, _table(raw, "dataset"), "dataset"),
        zvec_grep=_build(ZvecGrepConfig, _table(raw, "zvec_grep"), "zvec_grep"),
        run=_build(RunConfig, _table(raw, "run"), "run"),
        path=path,
    )
    _validate(config)
    return config


def _validate(config: BenchmarkConfig) -> None:
    for revision in (
        config.dataset.queries_revision,
        config.dataset.corpus_revision,
    ):
        if len(revision) != 40 or any(c not in "0123456789abcdef" for c in revision):
            raise ConfigError("dataset revisions must be pinned 40-character SHAs")
    if config.zvec_grep.embedding_concurrency < 1:
        raise ConfigError("embedding_concurrency must be positive")
    if config.zvec_grep.mcp_tool_timeout_seconds < 1:
        raise ConfigError("mcp_tool_timeout_seconds must be positive")
    if not 1 <= config.zvec_grep.server_port <= 65_535:
        raise ConfigError("server_port must be between 1 and 65535")
    if (
        config.dataset.expected_queries < 1
        or config.dataset.expected_corpus_documents < 1
    ):
        raise ConfigError("expected dataset counts must be positive")
    if config.run.infrastructure_retries < 0:
        raise ConfigError("infrastructure_retries cannot be negative")
    if config.run.trials_per_case < 1:
        raise ConfigError("trials_per_case must be positive")
    if config.run.idle_timeout_seconds < 1:
        raise ConfigError("idle_timeout_seconds must be positive")
    if not config.run.model.strip():
        raise ConfigError("run model cannot be empty")
    if not config.run.reasoning_effort.strip():
        raise ConfigError("run reasoning_effort cannot be empty")
