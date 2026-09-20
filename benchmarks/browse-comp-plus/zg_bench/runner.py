from __future__ import annotations

import hashlib
import json
import os
import platform
import random
import signal
import sys
import time
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .artifacts import (
    find_run,
    fingerprint,
    new_run_id,
    next_attempt_number,
    read_json,
    sha256_file,
    utc_now,
    write_json,
)
from .codex import (
    run_attempt,
    validate_model,
)
from .console import Console
from .config import BENCHMARK_ROOT, BenchmarkConfig, PROMPT_PATH, SUITES_DIR
from .corpus import prepared_corpus, validate_workspace
from .dataset import load_queries, prepared_dataset
from .index import prepared_index
from .models import PROFILES, AttemptResult, Profile
from .process import resolve_executable, run_command
from .profiles import (
    ensure_server,
    prepare_profiles,
    prepare_search_runtime,
    server_url,
    stop_server,
    validate_profiles,
)


class RunTerminated(BaseException):
    def __init__(self, signum: int) -> None:
        self.signum = signum
        super().__init__(f"run terminated by signal {signum}")


@contextmanager
def _cleanup_on_termination() -> Any:
    previous_handlers: dict[int, Any] = {}

    def terminate(signum: int, _frame: Any) -> None:
        raise RunTerminated(signum)

    for signum in (signal.SIGTERM, signal.SIGHUP):
        previous_handlers[signum] = signal.signal(signum, terminate)
    try:
        yield
    finally:
        for signum, handler in previous_handlers.items():
            signal.signal(signum, handler)


def _operating_system_name() -> str:
    system = platform.system()
    if system == "Linux":
        try:
            name = platform.freedesktop_os_release().get("PRETTY_NAME", "")
        except OSError:
            name = ""
        if name:
            return name
    if system == "Darwin":
        version = platform.mac_ver()[0]
        return f"macOS {version}" if version else "macOS"
    return f"{system} {platform.release()}".strip()


def _cpu_model() -> str:
    if platform.system() == "Linux":
        cpuinfo = Path("/proc/cpuinfo")
        if cpuinfo.is_file():
            try:
                lines = cpuinfo.read_text(encoding="utf-8").splitlines()
            except OSError:
                lines = []
            for line in lines:
                key, separator, value = line.partition(":")
                if separator and key.strip() in {"model name", "Hardware"}:
                    if value.strip():
                        return value.strip()
    if platform.system() == "Darwin":
        result = run_command(
            ["sysctl", "-n", "machdep.cpu.brand_string"], timeout=5
        )
        if result.ok and result.stdout.strip():
            return result.stdout.strip()
    return platform.processor().strip() or platform.machine()


def _available_cpu_count() -> int | None:
    affinity = getattr(os, "sched_getaffinity", None)
    if affinity is not None:
        try:
            return len(affinity(0))
        except OSError:
            pass
    return os.cpu_count()


def _environment_metadata(
    config: BenchmarkConfig,
    *,
    codex: Path,
    codex_version: str,
    zg_version: str,
) -> dict[str, Any]:
    return {
        "operating_system": _operating_system_name(),
        "os_release": platform.release(),
        "os_version": platform.version(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "cpu_model": _cpu_model(),
        "logical_cpu_count": os.cpu_count(),
        "available_cpu_count": _available_cpu_count(),
        "python": sys.version.split()[0],
        "codex_bin": str(codex),
        "codex_version": codex_version,
        "query_dataset_repo": config.dataset.queries_repo,
        "query_dataset_revision": config.dataset.queries_revision,
        "query_dataset_split": config.dataset.queries_split,
        "corpus_repo": config.dataset.corpus_repo,
        "corpus_revision": config.dataset.corpus_revision,
        "corpus_split": config.dataset.corpus_split,
        "zg_version": zg_version,
        "embedding": config.zvec_grep.embedding,
        "fts_tokenizer": "jieba",
        "embedding_concurrency": config.zvec_grep.embedding_concurrency,
        "embedding_device": config.zvec_grep.device,
        "max_filesize": config.zvec_grep.max_filesize,
        "mcp_transport": "http",
        "mcp_tool_timeout_seconds": config.zvec_grep.mcp_tool_timeout_seconds,
        "server_port": config.zvec_grep.server_port,
        "zg_server_url": server_url(config),
        "codex_sandbox": "workspace-write",
        "web_search": "disabled",
        "history_persistence": "none",
    }


@dataclass(frozen=True)
class SuiteSelection:
    path: Path
    definition_sha256: str
    mode: str
    target_count: int | None
    dataset_query_count: int
    scanned_query_count: int | None
    excluded_query_ids: tuple[str, ...]
    query_ids: tuple[str, ...]

    def to_dict(self) -> dict[str, Any]:
        resolved = self.path.resolve()
        try:
            definition = str(resolved.relative_to(BENCHMARK_ROOT.resolve()))
        except ValueError:
            definition = str(resolved)
        return {
            "definition": definition,
            "definition_sha256": self.definition_sha256,
            "mode": self.mode,
            "target_count": self.target_count,
            "dataset_query_count": self.dataset_query_count,
            "scanned_query_count": self.scanned_query_count,
            "excluded_query_ids": list(self.excluded_query_ids),
            "selected_query_count": len(self.query_ids),
        }


def _select_suite(
    path: Path,
    lines: list[str],
    all_ids: list[str],
    definition_sha256: str,
) -> SuiteSelection:
    selector: tuple[str, int | None] | None = None
    explicit_ids: list[str] = []
    excluded_ids: list[str] = []

    for line in lines:
        parts = line.split()
        directive = parts[0] if parts and parts[0].startswith("@") else None
        if directive == "@all":
            if len(parts) != 1:
                raise ValueError(f"invalid @all directive: {line}")
            if selector is not None:
                raise ValueError(f"suite contains multiple selectors: {path}")
            selector = ("all", None)
        elif directive == "@first":
            if len(parts) != 2:
                raise ValueError(f"invalid @first directive: {line}")
            try:
                count = int(parts[1])
            except ValueError as error:
                raise ValueError(f"invalid @first count: {parts[1]}") from error
            if count < 1:
                raise ValueError(f"invalid @first count: {count}")
            if selector is not None:
                raise ValueError(f"suite contains multiple selectors: {path}")
            selector = ("first", count)
        elif directive == "@exclude":
            if len(parts) != 2:
                raise ValueError(f"invalid @exclude directive: {line}")
            excluded_ids.append(parts[1])
        elif directive is not None:
            raise ValueError(f"unknown suite directive: {line}")
        else:
            explicit_ids.append(line)

    if selector is not None and explicit_ids:
        raise ValueError(
            f"suite cannot combine a selector with explicit query IDs: {path}"
        )
    if selector is None and excluded_ids:
        raise ValueError(f"@exclude requires @first or @all: {path}")
    if len(set(excluded_ids)) != len(excluded_ids):
        raise ValueError("suite contains duplicate excluded query IDs")

    known_ids = set(all_ids)
    unknown_exclusions = sorted(set(excluded_ids) - known_ids)
    if unknown_exclusions:
        raise ValueError(
            "suite excludes unknown query IDs: " + ", ".join(unknown_exclusions)
        )

    excluded = set(excluded_ids)
    if selector is None:
        missing = sorted(set(explicit_ids) - known_ids)
        if missing:
            raise ValueError(
                f"suite contains unknown query IDs: {', '.join(missing)}"
            )
        if len(set(explicit_ids)) != len(explicit_ids):
            raise ValueError("suite contains duplicate query IDs")
        selected_ids = explicit_ids
        mode = "explicit"
        target_count: int | None = len(explicit_ids)
        scanned_query_count: int | None = None
    elif selector[0] == "all":
        selected_ids = [query_id for query_id in all_ids if query_id not in excluded]
        mode = "all"
        target_count = None
        scanned_query_count = len(all_ids)
    else:
        count = selector[1]
        assert count is not None
        eligible_ids = [query_id for query_id in all_ids if query_id not in excluded]
        if count > len(eligible_ids):
            raise ValueError(
                f"@first {count} cannot be satisfied after exclusions; "
                f"only {len(eligible_ids)} query IDs remain"
            )
        selected_ids = eligible_ids[:count]
        last_selected = all_ids.index(selected_ids[-1])
        scanned_query_count = last_selected + 1
        scanned_ids = set(all_ids[:scanned_query_count])
        ineffective = [
            query_id for query_id in excluded_ids if query_id not in scanned_ids
        ]
        if ineffective:
            raise ValueError(
                "suite exclusions do not affect @first selection: "
                + ", ".join(ineffective)
            )
        mode = "first"
        target_count = count

    if not selected_ids:
        raise ValueError(f"suite contains no selected query IDs: {path}")
    return SuiteSelection(
        path=path,
        definition_sha256=definition_sha256,
        mode=mode,
        target_count=target_count,
        dataset_query_count=len(all_ids),
        scanned_query_count=scanned_query_count,
        excluded_query_ids=tuple(excluded_ids),
        query_ids=tuple(selected_ids),
    )


def load_suite(name: str, queries: list[dict[str, Any]]) -> SuiteSelection:
    path = Path(name)
    if path.suffix != ".txt":
        path = SUITES_DIR / f"{name}.txt"
    if not path.is_file():
        raise ValueError(f"suite not found: {path}")
    definition = path.read_bytes()
    lines = [
        line.strip()
        for line in definition.decode("utf-8").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    if not lines:
        raise ValueError(f"suite contains no query IDs: {path}")
    all_ids = [str(row["query_id"]) for row in queries]
    return _select_suite(path, lines, all_ids, hashlib.sha256(definition).hexdigest())


def _randomized_profile_orders(
    query_ids: list[str], trials_per_case: int
) -> dict[str, dict[int, tuple[Profile, Profile]]]:
    rng = random.SystemRandom()
    output: dict[str, dict[int, tuple[Profile, Profile]]] = {
        query_id: {} for query_id in query_ids
    }
    for trial_index in range(1, trials_per_case + 1):
        orders: list[tuple[Profile, Profile]] = [PROFILES] * (
            len(query_ids) // 2
        )
        orders.extend(
            [("zvec-grep", "baseline")] * (len(query_ids) // 2)
        )
        if len(query_ids) % 2:
            orders.append(
                PROFILES
                if trial_index % 2
                else ("zvec-grep", "baseline")
            )
        rng.shuffle(orders)
        for query_id, order in zip(query_ids, orders, strict=True):
            output[query_id][trial_index] = order
    return output


def _prompt(query: str) -> str:
    template = PROMPT_PATH.read_text(encoding="utf-8")
    return template.replace("{query}", query).rstrip() + "\n"


def _result_path(
    run_root: Path, query_id: str, profile: Profile, trial_index: int
) -> Path:
    return (
        run_root
        / "cases"
        / query_id
        / profile
        / "trials"
        / f"trial-{trial_index:03d}"
        / "result.json"
    )


def _attempt_results(selected_path: Path) -> list[dict[str, Any]]:
    attempts_root = selected_path.parent / "attempts"
    results = [
        read_json(path)
        for path in sorted(attempts_root.glob("attempt-*/result.json"))
    ]
    return sorted(results, key=lambda result: int(result["attempt"]))


def _remaining_attempts(config: BenchmarkConfig, selected_path: Path) -> int:
    failures = sum(
        result.get("infrastructure_failure") is True
        for result in _attempt_results(selected_path)
    )
    return max(0, config.run.infrastructure_retries + 1 - failures)


def _validate_result_identity(
    result: dict[str, Any],
    *,
    query_id: str,
    profile: Profile,
    trial_index: int,
    path: Path,
) -> None:
    expected = (query_id, profile, trial_index)
    actual = (
        str(result.get("query_id")),
        result.get("profile"),
        result.get("trial_index"),
    )
    if actual != expected:
        raise RuntimeError(
            f"trial identity mismatch in {path}: expected {expected}, found {actual}"
        )


def _needs_run(config: BenchmarkConfig, path: Path) -> bool:
    if not path.is_file():
        return True
    result = read_json(path)
    if result["status"] in {"completed", "failed"}:
        return False
    if result["status"] != "infrastructure_failed":
        raise RuntimeError(f"unknown trial status in {path}: {result['status']}")
    return _remaining_attempts(config, path) > 0


def _run_profile(
    config: BenchmarkConfig,
    artifacts: Path,
    run_root: Path,
    query: dict[str, Any],
    profile: Profile,
    trial_index: int,
    model: str,
    reasoning_effort: str,
    profiles_root: Path,
    codex_bin: str,
) -> AttemptResult:
    query_id = str(query["query_id"])
    selected_path = _result_path(run_root, query_id, profile, trial_index)
    persisted_attempts = _attempt_results(selected_path)
    if selected_path.is_file():
        existing = read_json(selected_path)
        _validate_result_identity(
            existing,
            query_id=query_id,
            profile=profile,
            trial_index=trial_index,
            path=selected_path,
        )
        if not _needs_run(config, selected_path):
            return _result_from_dict(existing)
    elif persisted_attempts:
        latest = persisted_attempts[-1]
        latest_path = (
            selected_path.parent
            / "attempts"
            / f"attempt-{int(latest['attempt']):03d}"
            / "result.json"
        )
        _validate_result_identity(
            latest,
            query_id=query_id,
            profile=profile,
            trial_index=trial_index,
            path=latest_path,
        )
        if (
            latest["status"] in {"completed", "failed"}
            or _remaining_attempts(config, selected_path) == 0
        ):
            write_json(selected_path, latest)
            return _result_from_dict(latest)

    attempts_root = selected_path.parent / "attempts"
    attempts_root.mkdir(parents=True, exist_ok=True)
    first_attempt = next_attempt_number(attempts_root)
    final: AttemptResult | None = None
    for offset in range(_remaining_attempts(config, selected_path)):
        number = first_attempt + offset
        output = attempts_root / f"attempt-{number:03d}"
        try:
            final = run_attempt(
                config,
                artifacts,
                query_id=query_id,
                trial_index=trial_index,
                prompt=_prompt(str(query["query"])),
                profile=profile,
                model=model,
                reasoning_effort=reasoning_effort,
                attempt=number,
                output_dir=output,
                profiles_root=profiles_root,
                codex_bin=codex_bin,
                idle_timeout_seconds=config.run.idle_timeout_seconds,
            )
        finally:
            # Validate corpus metadata and remove all non-whitelisted files,
            # including residue left by failed or interrupted attempts.
            validate_workspace(artifacts, profile)
        if (
            final.status == "completed"
            or not final.infrastructure_failure
        ):
            break
    assert final is not None
    write_json(selected_path, final.to_dict())
    return final


def _execution_source_fingerprint() -> str:
    paths = [
        BENCHMARK_ROOT / "zg_bench" / name
        for name in (
            "codex.py",
            "config.py",
            "artifacts.py",
            "corpus.py",
            "dataset.py",
            "index.py",
            "models.py",
            "process.py",
            "profiles.py",
            "runner.py",
            "trace.py",
        )
    ]
    return fingerprint(
        value
        for path in paths
        for value in (str(path.relative_to(BENCHMARK_ROOT)), sha256_file(path))
    )


def _run_protocol(
    config: BenchmarkConfig,
    artifacts: Path,
    *,
    suite: str,
    suite_selection: dict[str, Any],
    query_ids: list[str],
    queries: dict[str, dict[str, Any]],
    profile_orders: dict[str, dict[int, tuple[Profile, Profile]]],
    model: str,
    reasoning_effort: str,
    codex: Path,
    codex_version: str,
    profiles_fingerprint: str,
) -> dict[str, Any]:
    dataset_state = read_json(artifacts / "state" / "dataset.json")
    corpus_state = read_json(artifacts / "state" / "corpus.json")
    index_state = read_json(artifacts / "state" / "index.json")
    return {
        "execution": "sequential_trial_rounds",
        "execution_order": "trial_query_profile",
        "execution_source_sha256": _execution_source_fingerprint(),
        "runner_path": str(BENCHMARK_ROOT),
        "task_prompt_sha256": sha256_file(PROMPT_PATH),
        "query_set_sha256": dataset_state["queries"]["sha256"],
        "suite": suite,
        "suite_definition_sha256": suite_selection["definition_sha256"],
        "suite_selection_sha256": fingerprint(
            [json.dumps(suite_selection, sort_keys=True, separators=(",", ":"))]
        ),
        "query_ids_sha256": fingerprint(query_ids),
        "selected_queries_sha256": fingerprint(
            json.dumps(queries[query_id], sort_keys=True, ensure_ascii=False)
            for query_id in query_ids
        ),
        "profile_orders_sha256": fingerprint(
            f"{query_id}:{trial_index}:{','.join(profile_orders[query_id][trial_index])}"
            for query_id in query_ids
            for trial_index in sorted(profile_orders[query_id])
        ),
        "corpus_fingerprint": corpus_state["fingerprint"],
        "index_fingerprint": index_state["fingerprint"],
        "profiles_fingerprint": profiles_fingerprint,
        "codex_sha256": sha256_file(codex),
        "codex_path": str(codex),
        "codex_version": codex_version,
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": sys.version.split()[0],
        "model": model,
        "reasoning_effort": reasoning_effort,
        "sandbox": "workspace-write",
        "web_search": "disabled",
        "history_persistence": "none",
        "git_ceiling": "workspace-parent",
        "infrastructure_retries": config.run.infrastructure_retries,
        "trials_per_case": config.run.trials_per_case,
        "idle_timeout_seconds": config.run.idle_timeout_seconds,
        "mcp_tool_timeout_seconds": config.zvec_grep.mcp_tool_timeout_seconds,
        "server_port": config.zvec_grep.server_port,
        "configuration_sha256": sha256_file(config.path),
    }


def _validate_run_protocol(recorded: dict[str, Any], actual: dict[str, Any]) -> None:
    if recorded == actual:
        return
    changed = sorted(
        key
        for key in set(recorded) | set(actual)
        if recorded.get(key) != actual.get(key)
    )
    raise RuntimeError(
        "run protocol changed after this run was created: "
        + ", ".join(changed)
        + "; restore the recorded setup or start a new run"
    )


def _protocol_fingerprint(protocol: dict[str, Any]) -> str:
    return fingerprint([json.dumps(protocol, sort_keys=True, separators=(",", ":"))])


def _result_from_dict(raw: dict[str, Any]) -> AttemptResult:
    from .models import ToolCall, TraceSummary, Usage

    trace = raw["trace"]
    raw_usage = trace["usage"]
    usage = (
        Usage(
            input_tokens=int(raw_usage["input_tokens"]),
            cached_input_tokens=int(raw_usage["cached_input_tokens"]),
            output_tokens=int(raw_usage["output_tokens"]),
            reasoning_output_tokens=int(raw_usage["reasoning_output_tokens"]),
        )
        if isinstance(raw_usage, dict)
        else None
    )
    return AttemptResult(
        query_id=str(raw["query_id"]),
        profile=raw["profile"],
        trial_index=int(raw["trial_index"]),
        status=str(raw["status"]),
        attempt=int(raw["attempt"]),
        started_at=str(raw["started_at"]),
        finished_at=str(raw["finished_at"]),
        wall_seconds=float(raw["wall_seconds"]),
        exit_code=int(raw["exit_code"]),
        infrastructure_failure=bool(raw["infrastructure_failure"]),
        trace=TraceSummary(
            thread_id=trace["thread_id"],
            final_response=str(trace["final_response"]),
            last_agent_message=str(trace["last_agent_message"]),
            turn_completed=bool(trace["turn_completed"]),
            usage=usage,
            tool_calls=tuple(ToolCall(**call) for call in trace["tool_calls"]),
            observed_docids=tuple(trace["observed_docids"]),
            errors=tuple(trace["errors"]),
        ),
        paths=dict(raw["paths"]),
    )


def _write_pair(run_root: Path, query_id: str, trials_per_case: int) -> bool:
    def metrics(result: dict[str, Any]) -> dict[str, Any]:
        calls = result["trace"].get("tool_calls", [])
        counts: dict[str, int] = {}
        for call in calls:
            name = str(call.get("name", "unknown"))
            counts[name] = counts.get(name, 0) + 1
        return {
            "status": result["status"],
            "wall_seconds": result["wall_seconds"],
            "usage": result["trace"].get("usage"),
            "tool_calls": len(calls),
            "tool_call_counts": counts,
            "observed_docids": len(result["trace"].get("observed_docids", [])),
            "result": str(
                _result_path(
                    run_root,
                    query_id,
                    result["profile"],
                    int(result["trial_index"]),
                ).resolve()
            ),
        }

    trials: list[dict[str, Any]] = []
    complete = True
    for trial_index in range(1, trials_per_case + 1):
        results: dict[str, Any] = {}
        for profile in PROFILES:
            path = _result_path(run_root, query_id, profile, trial_index)
            if not path.is_file():
                complete = False
                continue
            result = read_json(path)
            _validate_result_identity(
                result,
                query_id=query_id,
                profile=profile,
                trial_index=trial_index,
                path=path,
            )
            results[profile] = result
        if len(results) != len(PROFILES):
            continue
        trial = {
            "trial_index": trial_index,
            "eligible": all(
                result["status"] == "completed" for result in results.values()
            ),
            **{profile: metrics(results[profile]) for profile in PROFILES},
        }
        trials.append(trial)

    pair = {
        "query_id": query_id,
        "expected_trials": trials_per_case,
        "persisted_trials": len(trials),
        "eligible_trials": sum(bool(trial["eligible"]) for trial in trials),
        "eligible": complete
        and len(trials) == trials_per_case
        and all(trial["eligible"] for trial in trials),
        "trials": trials,
    }
    write_json(run_root / "cases" / query_id / "pair.json", pair)
    return bool(pair["eligible"])


def completed_cases(run_root: Path, query_ids: list[str]) -> int:
    count = 0
    for query_id in query_ids:
        path = run_root / "cases" / query_id / "pair.json"
        if path.is_file() and read_json(path)["eligible"] is True:
            count += 1
    return count


def _compact_tokens(value: int | None) -> str:
    if value is None:
        return "-"
    if abs(value) >= 1_000_000:
        return f"{value / 1_000_000:.1f}M"
    if abs(value) >= 1_000:
        return f"{value / 1_000:.1f}k"
    return str(value)


@dataclass
class _RunTokenCounter:
    total_tokens: dict[Profile, int] = field(
        default_factory=lambda: {profile: 0 for profile in PROFILES}
    )
    counted_attempts: set[Path] = field(default_factory=set)

    def add_attempts(self, attempts_root: Path, profile: Profile) -> None:
        for attempt_root in sorted(attempts_root.glob("attempt-*")):
            if (
                not attempt_root.is_dir()
                or not attempt_root.name.removeprefix("attempt-").isdigit()
                or attempt_root in self.counted_attempts
            ):
                continue
            usage_path = attempt_root / "usage.json"
            result_path = attempt_root / "result.json"
            if usage_path.is_file():
                usage = read_json(usage_path)
            elif result_path.is_file():
                result = read_json(result_path)
                usage = result.get("trace", {}).get("usage")
            else:
                continue
            if usage is not None and not isinstance(usage, dict):
                raise RuntimeError(f"invalid attempt usage in {attempt_root}")
            self.counted_attempts.add(attempt_root)
            if usage is None:
                continue
            self.total_tokens[profile] += int(usage.get("input_tokens", 0))
            self.total_tokens[profile] += int(usage.get("output_tokens", 0))

    def add_run(self, run_root: Path) -> None:
        for profile in PROFILES:
            for attempts_root in sorted(
                run_root.glob(
                    f"cases/*/{profile}/trials/trial-*/attempts"
                )
            ):
                self.add_attempts(attempts_root, profile)


def run_benchmark(
    config: BenchmarkConfig,
    artifacts: Path,
    *,
    suite: str,
    run_id: str | None = None,
    codex_bin: str = "codex",
) -> Path:
    model = config.run.model
    reasoning_effort = config.run.reasoning_effort
    query_path = artifacts / "source" / "browsecomp_plus_decrypted.jsonl"
    if not query_path.is_file():
        raise RuntimeError("dataset is missing; run 'zg-bench prepare' first")
    queries = load_queries(query_path)
    by_id = {str(row["query_id"]): row for row in queries}
    validate_model(artifacts, model)
    codex = resolve_executable(codex_bin)
    zg = resolve_executable("zg")
    if codex is None or zg is None:
        raise RuntimeError("Codex and zvec-grep must be available before a run")
    codex_version = run_command([codex, "--version"], timeout=30)
    if not codex_version.ok or not codex_version.stdout.strip():
        raise RuntimeError("could not determine the installed Codex version")
    zg_version = run_command([zg, "--version"], timeout=30)
    actual_zg_version = (
        zg_version.stdout.strip().splitlines()[0] if zg_version.stdout else ""
    )
    if not zg_version.ok or not actual_zg_version:
        raise RuntimeError("could not determine the installed zvec-grep version")
    missing_states = []
    if prepared_dataset(config, artifacts) is None:
        missing_states.append("dataset")
    if prepared_corpus(config, artifacts) is None:
        missing_states.append("corpus")
    if prepared_index(config, artifacts) is None:
        missing_states.append("index")
    if missing_states:
        raise RuntimeError(
            "benchmark preparation is incomplete: " + ", ".join(missing_states)
        )
    if run_id is None:
        run_id = new_run_id()
        run_root = artifacts / "runs" / run_id
        if run_root.exists():
            raise RuntimeError(f"benchmark run already exists: {run_id}")
    else:
        run_root = find_run(artifacts, run_id)
    Console().identifier("Run", run_id)
    metadata_path = run_root / "run.json"
    profiles_root = run_root / "profiles"
    profiles_manifest_path = profiles_root / "manifest.json"
    new_run = not metadata_path.is_file()
    if not new_run:
        metadata = read_json(metadata_path)
        suite = str(metadata["suite"])
        suite_metadata = dict(metadata["suite_selection"])
        query_ids = [str(value) for value in metadata["query_ids"]]
        if suite_metadata["selected_query_count"] != len(query_ids):
            raise RuntimeError("recorded suite selection count is invalid")
        suite_path = Path(str(suite_metadata["definition"]))
        if not suite_path.is_absolute():
            suite_path = BENCHMARK_ROOT / suite_path
        current_suite = load_suite(str(suite_path), queries)
        if (
            current_suite.to_dict() != suite_metadata
            or list(current_suite.query_ids) != query_ids
        ):
            raise RuntimeError(
                "suite definition or selection changed after this run was created; "
                "restore the recorded suite or start a new run"
            )
        profile_orders = {
            str(query_id): {
                int(trial_index): tuple(order)
                for trial_index, order in trials.items()
            }
            for query_id, trials in metadata["profile_orders"].items()
        }
        if metadata["model"] != model:
            raise RuntimeError("cannot resume a run after changing [run].model")
        if metadata["reasoning_effort"] != reasoning_effort:
            raise RuntimeError(
                "cannot resume a run after changing [run].reasoning_effort"
            )
        if metadata["trials_per_case"] != config.run.trials_per_case:
            raise RuntimeError(
                "cannot resume a run after changing [run].trials_per_case"
            )
        recorded_manifest = Path(metadata["profiles_manifest"])
        if recorded_manifest.resolve() != profiles_manifest_path.resolve():
            raise RuntimeError("run profile manifest path does not match its run")
        profile_manifest = validate_profiles(recorded_manifest)
        if metadata["profiles_fingerprint"] != profile_manifest["fingerprint"]:
            raise RuntimeError("run profile fingerprint does not match its manifest")
        recorded_protocol = metadata["protocol"]
        actual_protocol = _run_protocol(
            config,
            artifacts,
            suite=suite,
            suite_selection=suite_metadata,
            query_ids=query_ids,
            queries=by_id,
            profile_orders=profile_orders,
            model=model,
            reasoning_effort=reasoning_effort,
            codex=codex,
            codex_version=codex_version.stdout.strip(),
            profiles_fingerprint=profile_manifest["fingerprint"],
        )
        if metadata["protocol_fingerprint"] != _protocol_fingerprint(
            recorded_protocol
        ):
            raise RuntimeError("recorded run protocol fingerprint is invalid")
        _validate_run_protocol(recorded_protocol, actual_protocol)
    else:
        required_states = {
            name: artifacts / "state" / f"{name}.json"
            for name in ("corpus", "index")
        }
        suite_selection = load_suite(suite, queries)
        suite_metadata = suite_selection.to_dict()
        query_ids = list(suite_selection.query_ids)
        profile_orders = _randomized_profile_orders(
            query_ids, config.run.trials_per_case
        )
        corpus_state = read_json(required_states["corpus"])
        index_state = read_json(required_states["index"])

    missing = [query_id for query_id in query_ids if query_id not in by_id]
    if missing:
        raise RuntimeError(f"run references missing queries: {', '.join(missing)}")
    for query_id in query_ids:
        _write_pair(run_root, query_id, config.run.trials_per_case)
    run_tokens = _RunTokenCounter()
    run_tokens.add_run(run_root)
    tasks: list[tuple[dict[str, Any], int, Profile]] = []
    pending_profiles: dict[tuple[str, int], int] = {}
    for trial_index in range(1, config.run.trials_per_case + 1):
        for query_id in query_ids:
            pending = 0
            for profile in profile_orders[query_id][trial_index]:
                if _needs_run(
                    config,
                    _result_path(run_root, query_id, profile, trial_index),
                ):
                    tasks.append((by_id[query_id], trial_index, profile))
                    pending += 1
            pending_profiles[(query_id, trial_index)] = pending
    total_paired_trials = len(query_ids) * config.run.trials_per_case
    finished_paired_trials = sum(
        pending == 0 for pending in pending_profiles.values()
    )
    server_required = new_run or any(
        profile == "zvec-grep" for _, _, profile in tasks
    )
    benchmark_error: BaseException | None = None
    with _cleanup_on_termination():
        preparation_started_at = utc_now()
        preparation_started = time.monotonic()
        server_start_wall_seconds = 0.0
        profile_preparation_wall_seconds = 0.0
        profile_install_wall_seconds = 0.0
        try:
            if server_required:
                print(
                    "Preparing the zvec-grep daemon, profiles, and existing index...",
                    flush=True,
                )
                server_started = time.monotonic()
                ensure_server(config, artifacts, restart=True)
                server_start_wall_seconds = time.monotonic() - server_started

            if new_run:
                prepare_profiles(
                    config,
                    artifacts,
                    codex_bin=str(codex),
                    profiles_root=profiles_root,
                    manifest_path=profiles_manifest_path,
                )
                profile_manifest = validate_profiles(profiles_manifest_path)
                profile_preparation_wall_seconds = float(
                    profile_manifest["preparation_wall_seconds"]
                )
                profile_install_wall_seconds = float(
                    profile_manifest["install_wall_seconds"]
                )
                protocol = _run_protocol(
                    config,
                    artifacts,
                    suite=suite,
                    suite_selection=suite_metadata,
                    query_ids=query_ids,
                    queries=by_id,
                    profile_orders=profile_orders,
                    model=model,
                    reasoning_effort=reasoning_effort,
                    codex=codex,
                    codex_version=codex_version.stdout.strip(),
                    profiles_fingerprint=profile_manifest["fingerprint"],
                )
                metadata = {
                    "run_id": run_id,
                    "created_at": utc_now(),
                    "suite": suite,
                    "suite_selection": suite_metadata,
                    "query_ids": query_ids,
                    "model": model,
                    "reasoning_effort": reasoning_effort,
                    "profiles": list(PROFILES),
                    "trials_per_case": config.run.trials_per_case,
                    "profile_orders": {
                        query_id: {
                            str(trial_index): list(order)
                            for trial_index, order in trials.items()
                        }
                        for query_id, trials in profile_orders.items()
                    },
                    "profiles_manifest": str(profiles_manifest_path.resolve()),
                    "profiles_fingerprint": profile_manifest["fingerprint"],
                    "protocol": protocol,
                    "protocol_fingerprint": _protocol_fingerprint(protocol),
                    "configuration": str(config.path),
                    "environment": _environment_metadata(
                        config,
                        codex=codex,
                        codex_version=codex_version.stdout.strip(),
                        zg_version=zg_version.stdout.strip(),
                    ),
                    "corpus_fingerprint": corpus_state["fingerprint"],
                    "index_fingerprint": index_state["fingerprint"],
                    "index_build_wall_seconds": float(
                        index_state["build_wall_seconds"]
                    ),
                    "index_bytes": int(index_state["index_bytes"]),
                    "index_statistics": dict(index_state["statistics"]),
                    "runtime_setups": [],
                }
                write_json(metadata_path, metadata)

            if server_required:
                runtime_setup = prepare_search_runtime(
                    config,
                    artifacts,
                    restart_server=False,
                )
                total_preparation_wall_seconds = (
                    time.monotonic() - preparation_started
                )
                runtime_setup.update(
                    {
                        "started_at": preparation_started_at,
                        "finished_at": utc_now(),
                        "total_wall_seconds": total_preparation_wall_seconds,
                        "server_start_wall_seconds": server_start_wall_seconds,
                        "profile_preparation_wall_seconds": (
                            profile_preparation_wall_seconds
                        ),
                        "profile_install_wall_seconds": (
                            profile_install_wall_seconds
                        ),
                    }
                )
                metadata["runtime_setups"].append(runtime_setup)
                write_json(metadata_path, metadata)
                write_json(artifacts / "state" / "runtime.json", runtime_setup)
                print(
                    "zvec-grep runtime ready in "
                    f"{total_preparation_wall_seconds:.1f} seconds",
                    flush=True,
                )

            for query, trial_index, profile in tasks:
                query_id = str(query["query_id"])
                result = _run_profile(
                    config,
                    artifacts,
                    run_root,
                    query,
                    profile,
                    trial_index,
                    model,
                    reasoning_effort,
                    profiles_root,
                    str(codex),
                )
                _write_pair(
                    run_root, query_id, config.run.trials_per_case
                )
                pair_key = (query_id, trial_index)
                pending_profiles[pair_key] -= 1
                if pending_profiles[pair_key] == 0:
                    finished_paired_trials += 1
                input_tokens = (
                    result.trace.usage.input_tokens
                    if result.trace.usage
                    else None
                )
                output_tokens = (
                    result.trace.usage.output_tokens
                    if result.trace.usage
                    else None
                )
                run_tokens.add_attempts(
                    _result_path(
                        run_root, query_id, profile, trial_index
                    ).parent
                    / "attempts",
                    profile,
                )
                print(
                    f"run {run_id} · query {query_id} · "
                    f"repeat {trial_index}/{config.run.trials_per_case} · "
                    f"A/B comparisons {finished_paired_trials}/"
                    f"{total_paired_trials} · "
                    f"profile {profile}: {result.status} · "
                    f"tokens {_compact_tokens(input_tokens)} in / "
                    f"{_compact_tokens(output_tokens)} out · "
                    "run tokens Baseline "
                    f"{_compact_tokens(run_tokens.total_tokens['baseline'])} / "
                    "zvec-grep "
                    f"{_compact_tokens(run_tokens.total_tokens['zvec-grep'])} · "
                    f"{result.wall_seconds:.1f}s",
                    flush=True,
                )

            metadata["finished_at"] = utc_now()
            metadata["completed_cases"] = completed_cases(run_root, query_ids)
            metadata.pop("error", None)
            metadata["status"] = (
                "completed"
                if metadata["completed_cases"] == len(query_ids)
                else "partial"
            )
            write_json(metadata_path, metadata)
            from .report import generate_report

            generate_report(run_root)
        except BaseException as error:
            benchmark_error = error
            if metadata_path.is_file():
                failed_metadata = read_json(metadata_path)
                failed_metadata["finished_at"] = utc_now()
                failed_metadata["completed_cases"] = completed_cases(
                    run_root, query_ids
                )
                failed_metadata["status"] = (
                    "interrupted"
                    if isinstance(error, (KeyboardInterrupt, RunTerminated))
                    else "failed"
                )
                failed_metadata["error"] = {
                    "type": type(error).__name__,
                    "message": str(error),
                }
                write_json(metadata_path, failed_metadata)
            raise
        finally:
            if server_required:
                try:
                    stop_server(config, artifacts)
                except RuntimeError as error:
                    if benchmark_error is None:
                        raise
                    print(
                        f"Warning: zvec-grep server cleanup failed: {error}",
                        file=sys.stderr,
                        flush=True,
                    )
                else:
                    print(
                        f"zvec-grep server stopped at {server_url(config)}",
                        flush=True,
                    )
    return run_root


def resume_benchmark(
    config: BenchmarkConfig,
    artifacts: Path,
    run_id: str,
    *,
    codex_bin: str = "codex",
) -> Path:
    metadata = read_json(find_run(artifacts, run_id) / "run.json")
    return run_benchmark(
        config,
        artifacts,
        suite=str(metadata["suite"]),
        run_id=run_id,
        codex_bin=codex_bin,
    )
