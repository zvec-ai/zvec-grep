from __future__ import annotations

import csv
import io
import json
import re
import statistics
from collections import Counter
from pathlib import Path
from typing import Any, Iterable

from .artifacts import atomic_write_text, read_json, utc_now, write_json
from .dataset import load_queries
from .models import PROFILES, Profile
from .trace import extract_docids, mcp_result_text


ZG_COMMAND_PATTERN = re.compile(r"(?:^|[;&|()\s])zg(?:\s|$)")


def _is_zvec_tool(name: str) -> bool:
    return name.startswith(("zvec_grep", "zvec-grep"))


def _mean(values: list[float]) -> float | None:
    return statistics.fmean(values) if values else None


def _median(values: list[float]) -> float | None:
    return statistics.median(values) if values else None


def _display(value: int | float | None, *, digits: int = 2) -> str:
    return "-" if value is None else f"{value:,.{digits}f}"


def _markdown_cell(value: Any) -> str:
    if value is None:
        return "-"
    return str(value).replace("|", "\\|").replace("\n", " ")


def _pairs(run_root: Path) -> list[dict[str, Any]]:
    metadata = read_json(run_root / "run.json")
    pairs: list[dict[str, Any]] = []
    for query_id in map(str, metadata["query_ids"]):
        path = run_root / "cases" / query_id / "pair.json"
        if not path.is_file():
            continue
        pair = read_json(path)
        if str(pair.get("query_id")) != query_id:
            raise RuntimeError(
                f"pair identity mismatch in {path}: expected {query_id!r}"
            )
        pairs.append(pair)
    return pairs


def _trial_pairs(pairs: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:
    output: list[dict[str, Any]] = []
    for pair in pairs:
        for trial in pair["trials"]:
            output.append({"query_id": str(pair["query_id"]), **trial})
    return output


def _result(trial: dict[str, Any], profile: Profile) -> dict[str, Any]:
    return read_json(Path(trial[profile]["result"]))


def _judge_path(
    run_root: Path, profile: Profile, query_id: str, trial_index: int
) -> Path:
    return (
        run_root
        / "evaluation"
        / "results"
        / profile
        / query_id
        / f"trial-{trial_index:03d}.json"
    )


def _judgement(
    run_root: Path, profile: Profile, query_id: str, trial_index: int
) -> dict[str, Any] | None:
    path = _judge_path(run_root, profile, query_id, trial_index)
    from .evaluate import _completed_judgement

    if not _completed_judgement(
        path,
        run_root=run_root,
        profile=profile,
        query_id=query_id,
        trial_index=trial_index,
    ):
        return None
    return read_json(path)


def _number_summary(values: list[float]) -> dict[str, int | float | None]:
    return {
        "available": len(values),
        "total": sum(values),
        "mean": _mean(values),
        "median": _median(values),
    }


def _aggregate_profile(
    trials: list[dict[str, Any]], profile: Profile
) -> dict[str, Any]:
    selected = [trial[profile] for trial in trials]
    completed = [row for row in selected if row["status"] == "completed"]
    measured = [row for row in completed if isinstance(row.get("usage"), dict)]

    def token_values(key: str) -> list[float]:
        return [float(row["usage"].get(key, 0)) for row in measured]

    input_tokens = token_values("input_tokens")
    cached_input_tokens = token_values("cached_input_tokens")
    output_tokens = token_values("output_tokens")
    reasoning_tokens = token_values("reasoning_output_tokens")
    total_tokens = [
        input_value + output_value
        for input_value, output_value in zip(
            input_tokens, output_tokens, strict=True
        )
    ]
    wall_seconds = [float(row["wall_seconds"]) for row in completed]
    tool_calls = [float(row.get("tool_calls", 0)) for row in completed]
    command_calls = [
        float(row.get("tool_call_counts", {}).get("command_execution", 0))
        for row in completed
    ]
    zvec_calls = [
        float(
            sum(
                count
                for name, count in row.get("tool_call_counts", {}).items()
                if _is_zvec_tool(str(name))
            )
        )
        for row in completed
    ]
    observed_docids = [float(row.get("observed_docids", 0)) for row in completed]
    return {
        "trials": len(selected),
        "completed": len(completed),
        "status_counts": dict(Counter(row["status"] for row in selected)),
        "tokens": {
            "input": _number_summary(input_tokens),
            "cached_input": _number_summary(cached_input_tokens),
            "output": _number_summary(output_tokens),
            "reasoning_output": _number_summary(reasoning_tokens),
            "total": _number_summary(total_tokens),
            "unavailable": len(completed) - len(measured),
        },
        "wall_seconds": _number_summary(wall_seconds),
        "tools": {
            "all": _number_summary(tool_calls),
            "commands": _number_summary(command_calls),
            "zvec_grep": _number_summary(zvec_calls),
            "observed_docids": _number_summary(observed_docids),
        },
    }


def _judge_quality(
    run_root: Path, trials: list[dict[str, Any]]
) -> dict[str, Any]:
    outcomes: list[dict[str, Any]] = []
    for trial in trials:
        query_id = str(trial["query_id"])
        trial_index = int(trial["trial_index"])
        results = {
            profile: _judgement(run_root, profile, query_id, trial_index)
            for profile in PROFILES
        }
        if any(result is None for result in results.values()):
            continue
        baseline_correct = bool(results["baseline"]["correct"])
        treatment_correct = bool(results["zvec-grep"]["correct"])
        outcomes.append(
            {
                "query_id": query_id,
                "trial_index": trial_index,
                "baseline_correct": baseline_correct,
                "treatment_correct": treatment_correct,
            }
        )
    if not outcomes:
        return {"status": "pending", "expected_trials": len(trials)}
    both_correct = sum(
        row["baseline_correct"] and row["treatment_correct"] for row in outcomes
    )
    baseline_only = sum(
        row["baseline_correct"] and not row["treatment_correct"]
        for row in outcomes
    )
    treatment_only = sum(
        not row["baseline_correct"] and row["treatment_correct"]
        for row in outcomes
    )
    neither = len(outcomes) - both_correct - baseline_only - treatment_only
    return {
        "status": "scored" if len(outcomes) == len(trials) else "partial",
        "expected_trials": len(trials),
        "scored_trials": len(outcomes),
        "baseline_accuracy_percent": 100
        * sum(row["baseline_correct"] for row in outcomes)
        / len(outcomes),
        "treatment_accuracy_percent": 100
        * sum(row["treatment_correct"] for row in outcomes)
        / len(outcomes),
        "both_correct": both_correct,
        "baseline_only_correct": baseline_only,
        "treatment_only_correct": treatment_only,
        "neither_correct": neither,
        "outcomes": outcomes,
    }


def _expected_docids(value: Any) -> set[str]:
    if not isinstance(value, list):
        return set()
    return {
        str(item["docid"] if isinstance(item, dict) else item)
        for item in value
        if (not isinstance(item, dict) or item.get("docid") is not None)
        and isinstance(item, (str, int, dict))
    }


def _retrieval_quality(
    trials: list[dict[str, Any]], queries: dict[str, dict[str, Any]]
) -> dict[str, dict[str, dict[str, int | float | None]]]:
    output: dict[str, dict[str, dict[str, int | float | None]]] = {}
    for label, field in (("evidence", "evidence_docs"), ("gold", "gold_docs")):
        recalls: dict[Profile, list[float]] = {profile: [] for profile in PROFILES}
        hits: Counter[str] = Counter()
        for trial in trials:
            expected = _expected_docids(queries[str(trial["query_id"])][field])
            if not expected:
                continue
            for profile in PROFILES:
                result = _result(trial, profile)
                observed = set(result["trace"].get("observed_docids", []))
                matched = expected & observed
                recalls[profile].append(len(matched) / len(expected))
                hits[profile] += bool(matched)
        output[label] = {
            profile: {
                "eligible_trials": len(recalls[profile]),
                "mean_recall_percent": (
                    100 * statistics.fmean(recalls[profile])
                    if recalls[profile]
                    else None
                ),
                "hit_rate_percent": (
                    100 * hits[profile] / len(recalls[profile])
                    if recalls[profile]
                    else None
                ),
            }
            for profile in PROFILES
        }
    return output


def _tool_interaction_batches(
    events_path: Path, *, evidence: set[str], gold: set[str]
) -> dict[str, int | None]:
    rounds: list[dict[str, set[str]]] = []
    active: set[str] = set()
    round_by_item: dict[str, int] = {}
    with events_path.open(encoding="utf-8", errors="replace") as source:
        for line in source:
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            item = event.get("item") or {}
            item_type = item.get("type")
            if event.get("type") == "item.completed" and item_type == "agent_message":
                for item_id in active:
                    round_by_item.pop(item_id, None)
                active.clear()
                continue
            if item_type not in {"command_execution", "mcp_tool_call"}:
                continue
            if event.get("type") not in {"item.started", "item.completed"}:
                continue
            item_id = str(item.get("id", ""))
            if event.get("type") == "item.started":
                if not active:
                    rounds.append({"evidence": set(), "gold": set()})
                active.add(item_id)
                round_by_item[item_id] = len(rounds) - 1
                continue
            round_index = round_by_item.pop(item_id, None)
            if round_index is None:
                if not active:
                    rounds.append({"evidence": set(), "gold": set()})
                round_index = len(rounds) - 1
            if item_type == "command_execution":
                observed = extract_docids(
                    item.get("command"), item.get("aggregated_output", "")
                )
            else:
                observed = extract_docids(mcp_result_text(item.get("result")))
            rounds[round_index]["evidence"].update(observed & evidence)
            rounds[round_index]["gold"].update(observed & gold)
            active.discard(item_id)

    def first_hit(kind: str) -> int | None:
        return next(
            (number for number, value in enumerate(rounds, 1) if value[kind]), None
        )

    first_evidence = first_hit("evidence")
    first_gold = first_hit("gold")
    return {
        "total": len(rounds),
        "first_evidence": first_evidence,
        "after_first_evidence": (
            len(rounds) - first_evidence if first_evidence is not None else None
        ),
        "first_gold": first_gold,
        "after_first_gold": (
            len(rounds) - first_gold if first_gold is not None else None
        ),
    }


def _interaction_quality(
    trials: list[dict[str, Any]], queries: dict[str, dict[str, Any]]
) -> dict[str, dict[str, int | float | None]]:
    values: dict[Profile, list[dict[str, int | None]]] = {
        profile: [] for profile in PROFILES
    }
    for trial in trials:
        query = queries[str(trial["query_id"])]
        evidence = _expected_docids(query["evidence_docs"])
        gold = _expected_docids(query["gold_docs"])
        for profile in PROFILES:
            result = _result(trial, profile)
            values[profile].append(
                _tool_interaction_batches(
                    Path(result["paths"]["events"]), evidence=evidence, gold=gold
                )
            )

    def aggregate(rows: list[dict[str, int | None]]) -> dict[str, Any]:
        def measured(key: str) -> list[float]:
            return [float(row[key]) for row in rows if row[key] is not None]

        return {
            "eligible_trials": len(rows),
            "batches": _number_summary(measured("total")),
            "first_evidence_hits": len(measured("first_evidence")),
            "first_evidence_batch": _number_summary(measured("first_evidence")),
            "after_first_evidence": _number_summary(
                measured("after_first_evidence")
            ),
            "first_gold_hits": len(measured("first_gold")),
            "first_gold_batch": _number_summary(measured("first_gold")),
            "after_first_gold": _number_summary(measured("after_first_gold")),
        }

    return {profile: aggregate(values[profile]) for profile in PROFILES}


def _resource_rows(profile: dict[str, Any]) -> list[tuple[str, float | None]]:
    return [
        ("Average input tokens", profile["tokens"]["input"]["mean"]),
        ("Average cached input tokens", profile["tokens"]["cached_input"]["mean"]),
        ("Average output tokens", profile["tokens"]["output"]["mean"]),
        (
            "Average reasoning output tokens",
            profile["tokens"]["reasoning_output"]["mean"],
        ),
        ("Average total tokens", profile["tokens"]["total"]["mean"]),
        ("Average tool calls", profile["tools"]["all"]["mean"]),
        ("Average command calls", profile["tools"]["commands"]["mean"]),
        ("Average zvec-grep calls", profile["tools"]["zvec_grep"]["mean"]),
        ("Average Agent time (seconds)", profile["wall_seconds"]["mean"]),
        (
            "Average document ID mentions",
            profile["tools"]["observed_docids"]["mean"],
        ),
    ]


def _case_distribution(
    trials: list[dict[str, Any]], metric: str
) -> dict[str, int | float | None]:
    grouped: dict[str, dict[Profile, list[float]]] = {}
    for trial in trials:
        values: dict[Profile, float] = {}
        for profile in PROFILES:
            row = trial[profile]
            if metric == "input_tokens":
                usage = row.get("usage")
                if not isinstance(usage, dict):
                    break
                value = float(usage.get("input_tokens", 0))
            elif metric == "tool_calls":
                value = float(row.get("tool_calls", 0))
            elif metric == "wall_seconds":
                value = float(row["wall_seconds"])
            else:
                raise ValueError(f"unsupported case distribution metric: {metric}")
            values[profile] = value
        if len(values) != len(PROFILES):
            continue
        case = grouped.setdefault(
            str(trial["query_id"]), {profile: [] for profile in PROFILES}
        )
        for profile, value in values.items():
            case[profile].append(value)
    changes: list[float] = []
    improved = tied = regressed = 0
    eligible_cases = 0
    for profiles in grouped.values():
        if not all(profiles[profile] for profile in PROFILES):
            continue
        eligible_cases += 1
        baseline = statistics.fmean(profiles["baseline"])
        treatment = statistics.fmean(profiles["zvec-grep"])
        if treatment < baseline:
            improved += 1
        elif treatment > baseline:
            regressed += 1
        else:
            tied += 1
        if baseline:
            changes.append(100 * (treatment - baseline) / baseline)
    return {
        "eligible_cases": eligible_cases,
        "improved": improved,
        "tied": tied,
        "regressed": regressed,
        "median_case_change_percent": _median(changes),
    }


def _tool_behavior(trials: list[dict[str, Any]]) -> dict[str, int | float]:
    treatment_trials_with_zg = 0
    treatment_zg_calls = 0
    treatment_successful_zg_calls = 0
    treatment_failed_zg_calls = 0
    treatment_empty_zg_calls = 0
    baseline_direct_zg_commands = 0
    baseline_zvec_mcp_calls = 0
    for trial in trials:
        for profile in PROFILES:
            result = _result(trial, profile)
            trial_has_zg = False
            for call in result["trace"].get("tool_calls", []):
                name = str(call.get("name", ""))
                if profile == "baseline":
                    if name == "command_execution" and ZG_COMMAND_PATTERN.search(
                        str(call.get("arguments", ""))
                    ):
                        baseline_direct_zg_commands += 1
                    if _is_zvec_tool(name):
                        baseline_zvec_mcp_calls += 1
                    continue
                if not _is_zvec_tool(name):
                    continue
                trial_has_zg = True
                treatment_zg_calls += 1
                if call.get("status") == "completed":
                    treatment_successful_zg_calls += 1
                    if not str(call.get("output") or "").strip():
                        treatment_empty_zg_calls += 1
                else:
                    treatment_failed_zg_calls += 1
            treatment_trials_with_zg += trial_has_zg
    total_trials = len(trials)
    return {
        "eligible_trials": total_trials,
        "treatment_trials_with_zvec_grep": treatment_trials_with_zg,
        "treatment_trial_adoption_percent": (
            100 * treatment_trials_with_zg / total_trials if total_trials else 0.0
        ),
        "treatment_zvec_grep_calls": treatment_zg_calls,
        "treatment_successful_zvec_grep_calls": treatment_successful_zg_calls,
        "treatment_failed_zvec_grep_calls": treatment_failed_zg_calls,
        "treatment_empty_zvec_grep_calls": treatment_empty_zg_calls,
        "baseline_direct_zg_commands": baseline_direct_zg_commands,
        "baseline_zvec_grep_mcp_calls": baseline_zvec_mcp_calls,
    }


def _usage_audit(
    run_root: Path, trials: list[dict[str, Any]], suite: str
) -> dict[str, Any]:
    if suite != "smoke":
        return {"status": "not_applicable"}
    rows: list[dict[str, Any]] = []
    from .evaluate import _usage_audit_current

    for trial in trials:
        path = (
            run_root
            / "evaluation"
            / "usage-audit"
            / "results"
            / str(trial["query_id"])
            / f"trial-{int(trial['trial_index']):03d}.json"
        )
        query_id = str(trial["query_id"])
        trial_index = int(trial["trial_index"])
        if not _usage_audit_current(
            path,
            run_root=run_root,
            query_id=query_id,
            trial_index=trial_index,
        ):
            continue
        result = read_json(path)
        rows.append(result)
    status = (
        "scored"
        if len(rows) == len(trials)
        else "partial"
        if rows
        else "pending"
    )
    return {
        "status": status,
        "expected_trials": len(trials),
        "evaluated_trials": len(rows),
        "correct_trials": sum(bool(row["correct_usage"]) for row in rows),
        "trials": rows,
    }


def _runtime_preparation(metadata: dict[str, Any]) -> dict[str, int | float]:
    setups = metadata["runtime_setups"]
    return {
        "sessions": len(setups),
        "total_wall_seconds": sum(float(row["total_wall_seconds"]) for row in setups),
        "server_start_wall_seconds": sum(
            float(row["server_start_wall_seconds"]) for row in setups
        ),
        "profile_preparation_wall_seconds": sum(
            float(row["profile_preparation_wall_seconds"]) for row in setups
        ),
        "profile_install_wall_seconds": sum(
            float(row["profile_install_wall_seconds"]) for row in setups
        ),
        "warmup_wall_seconds": sum(
            float(row["warmup_wall_seconds"]) for row in setups
        ),
    }


def _primary_rows(
    quality: dict[str, Any], profiles: dict[str, dict[str, Any]]
) -> list[tuple[str, float | None, float | None, bool]]:
    rows = [
        (
            "Accuracy (%)",
            quality.get("baseline_accuracy_percent"),
            quality.get("treatment_accuracy_percent"),
            True,
        )
    ]
    baseline_rows = _resource_rows(profiles["baseline"])
    treatment_rows = dict(_resource_rows(profiles["zvec-grep"]))
    rows.extend(
        (label, value, treatment_rows[label], False)
        for label, value in baseline_rows
    )
    return rows


def _relative_change(baseline: float, treatment: float) -> str:
    if baseline == 0:
        return "N/A"
    value = 100 * (treatment - baseline) / baseline
    if value > 0:
        return f"+{value:,.2f}%"
    if value < 0:
        return f"−{abs(value):,.2f}%"
    return "0.00%"


def _markdown_metric_rows(
    rows: Iterable[tuple[str, float | None, float | None, bool]]
) -> str:
    output = []
    for label, baseline, treatment, percentage_points in rows:
        difference = None
        if baseline is not None and treatment is not None:
            difference = treatment - baseline
        absolute_change = _display(difference)
        relative_change = "-"
        if difference is not None:
            if percentage_points:
                absolute_change += " pp"
            else:
                relative_change = _relative_change(baseline, treatment)
        output.append(
            f"| {label} | {_display(baseline)} | {_display(treatment)} | "
            f"{absolute_change} | {relative_change} |"
        )
    return "\n".join(output)


def _retrieval_rows(
    retrieval: dict[str, Any], interactions: dict[str, Any]
) -> list[tuple[str, float | None, float | None, bool]]:
    return [
        (
            "Evidence recall (%)",
            retrieval["evidence"]["baseline"]["mean_recall_percent"],
            retrieval["evidence"]["zvec-grep"]["mean_recall_percent"],
            True,
        ),
        (
            "Evidence hit rate (%)",
            retrieval["evidence"]["baseline"]["hit_rate_percent"],
            retrieval["evidence"]["zvec-grep"]["hit_rate_percent"],
            True,
        ),
        (
            "Gold recall (%)",
            retrieval["gold"]["baseline"]["mean_recall_percent"],
            retrieval["gold"]["zvec-grep"]["mean_recall_percent"],
            True,
        ),
        (
            "Gold hit rate (%)",
            retrieval["gold"]["baseline"]["hit_rate_percent"],
            retrieval["gold"]["zvec-grep"]["hit_rate_percent"],
            True,
        ),
        (
            "Average tool-interaction batches",
            interactions["baseline"]["batches"]["mean"],
            interactions["zvec-grep"]["batches"]["mean"],
            False,
        ),
        (
            "Average batch to first evidence",
            interactions["baseline"]["first_evidence_batch"]["mean"],
            interactions["zvec-grep"]["first_evidence_batch"]["mean"],
            False,
        ),
        (
            "Average batch to first gold",
            interactions["baseline"]["first_gold_batch"]["mean"],
            interactions["zvec-grep"]["first_gold_batch"]["mean"],
            False,
        ),
    ]


def _environment_rows(environment: dict[str, Any]) -> str:
    cpu_counts = (
        f"{environment['available_cpu_count'] or '-'} available / "
        f"{environment['logical_cpu_count'] or '-'} logical"
    )
    values = (
        ("Operating system", environment["operating_system"]),
        ("Kernel / platform", environment["platform"]),
        ("Architecture", environment["machine"]),
        ("CPU", environment["cpu_model"]),
        ("CPU count", cpu_counts),
        ("Python", environment["python"]),
        ("Codex", environment["codex_version"]),
        ("Codex executable", environment["codex_bin"]),
        ("Codex sandbox", environment["codex_sandbox"]),
        ("Web search", environment["web_search"]),
        ("History persistence", environment["history_persistence"]),
        (
            "Query dataset",
            f"{environment['query_dataset_repo']}@"
            f"{environment['query_dataset_revision']}",
        ),
        ("Query split", environment["query_dataset_split"]),
        (
            "Corpus",
            f"{environment['corpus_repo']}@{environment['corpus_revision']}",
        ),
        ("Corpus split", environment["corpus_split"]),
        ("zvec-grep", environment["zg_version"]),
        ("Embedding", environment["embedding"]),
        ("FTS tokenizer", environment["fts_tokenizer"]),
        ("Index embedding concurrency", environment["embedding_concurrency"]),
        ("Configured embedding device", environment["embedding_device"]),
        ("Maximum indexed file size", environment["max_filesize"]),
        ("MCP transport", environment["mcp_transport"]),
        ("MCP tool timeout", f"{environment['mcp_tool_timeout_seconds']} seconds"),
        ("zvec-grep server", environment["zg_server_url"]),
    )
    return "\n".join(
        f"| {label} | {_markdown_cell(value)} |" for label, value in values
    )


def _write_trial_csv(
    report_dir: Path, run_root: Path, trials: list[dict[str, Any]]
) -> None:
    output = io.StringIO()
    writer = csv.writer(output)
    writer.writerow(
        [
            "query_id",
            "trial_index",
            "profile",
            "status",
            "correct",
            "input_tokens",
            "cached_input_tokens",
            "output_tokens",
            "reasoning_output_tokens",
            "total_tokens",
            "wall_seconds",
            "tool_calls",
            "command_calls",
            "zvec_grep_calls",
            "observed_docids",
        ]
    )
    for trial in trials:
        for profile in PROFILES:
            row = trial[profile]
            usage = row.get("usage") or {}
            judgement = _judgement(
                run_root,
                profile,
                str(trial["query_id"]),
                int(trial["trial_index"]),
            )
            writer.writerow(
                [
                    trial["query_id"],
                    trial["trial_index"],
                    profile,
                    row["status"],
                    judgement["correct"] if judgement else "",
                    usage.get("input_tokens", ""),
                    usage.get("cached_input_tokens", ""),
                    usage.get("output_tokens", ""),
                    usage.get("reasoning_output_tokens", ""),
                    usage.get("total_tokens", ""),
                    row["wall_seconds"],
                    row.get("tool_calls", 0),
                    row.get("tool_call_counts", {}).get("command_execution", 0),
                    sum(
                        count
                        for name, count in row.get("tool_call_counts", {}).items()
                        if _is_zvec_tool(str(name))
                    ),
                    row.get("observed_docids", 0),
                ]
            )
    atomic_write_text(report_dir / "trials.csv", output.getvalue())


def _write_case_csv(report_dir: Path, trials: list[dict[str, Any]]) -> None:
    grouped: dict[str, list[dict[str, Any]]] = {}
    for trial in trials:
        grouped.setdefault(str(trial["query_id"]), []).append(trial)
    output = io.StringIO()
    writer = csv.writer(output)
    writer.writerow(
        [
            "query_id",
            "completed_trials",
            "baseline_mean_input_tokens",
            "treatment_mean_input_tokens",
            "input_tokens_change_percent",
            "baseline_mean_tool_calls",
            "treatment_mean_tool_calls",
            "tool_calls_change_percent",
            "baseline_mean_wall_seconds",
            "treatment_mean_wall_seconds",
            "wall_seconds_change_percent",
        ]
    )

    def paired_means(
        rows: list[dict[str, Any]], metric: str
    ) -> tuple[float | None, float | None]:
        values: dict[Profile, list[float]] = {
            profile: [] for profile in PROFILES
        }
        for row in rows:
            paired: dict[Profile, float] = {}
            for profile in PROFILES:
                profile_row = row[profile]
                if metric == "input_tokens":
                    usage = profile_row.get("usage")
                    if not isinstance(usage, dict):
                        break
                    paired[profile] = float(usage.get("input_tokens", 0))
                else:
                    paired[profile] = float(profile_row[metric])
            if len(paired) != len(PROFILES):
                continue
            for profile, value in paired.items():
                values[profile].append(value)
        return tuple(
            statistics.fmean(values[profile]) if values[profile] else None
            for profile in PROFILES
        )

    def change(
        baseline: float | None, treatment: float | None
    ) -> float | str:
        if baseline is None or treatment is None or baseline == 0:
            return ""
        return 100 * (treatment - baseline) / baseline

    for query_id, rows in sorted(grouped.items()):
        values: list[float | str] = [query_id, len(rows)]
        for metric in ("input_tokens", "tool_calls", "wall_seconds"):
            baseline, treatment = paired_means(rows, metric)
            values.extend(
                (
                    "" if baseline is None else baseline,
                    "" if treatment is None else treatment,
                    change(baseline, treatment),
                )
            )
        writer.writerow(values)
    atomic_write_text(report_dir / "cases.csv", output.getvalue())


def generate_report(run_root: Path) -> Path:
    metadata = read_json(run_root / "run.json")
    pairs = _pairs(run_root)
    trials = [trial for trial in _trial_pairs(pairs) if trial["eligible"]]
    query_path = run_root.parent.parent / "source" / "browsecomp_plus_decrypted.jsonl"
    queries = {str(row["query_id"]): row for row in load_queries(query_path)}
    quality = _judge_quality(run_root, trials)
    profiles = {
        profile: _aggregate_profile(trials, profile) for profile in PROFILES
    }
    retrieval = _retrieval_quality(trials, queries)
    interactions = _interaction_quality(trials, queries)
    both_correct_keys = {
        (str(row["query_id"]), int(row["trial_index"]))
        for row in quality.get("outcomes", [])
        if row["baseline_correct"] and row["treatment_correct"]
    }
    both_correct_trials = [
        trial
        for trial in trials
        if (str(trial["query_id"]), int(trial["trial_index"]))
        in both_correct_keys
    ]
    both_correct_profiles = {
        profile: _aggregate_profile(both_correct_trials, profile)
        for profile in PROFILES
    }
    both_correct_retrieval = _retrieval_quality(both_correct_trials, queries)
    both_correct_interactions = _interaction_quality(both_correct_trials, queries)
    runtime_preparation = _runtime_preparation(metadata)
    evaluator_path = run_root / "evaluation" / "summary.json"
    from .evaluate import evaluation_complete

    evaluator = (
        read_json(evaluator_path)
        if evaluator_path.is_file() and evaluation_complete(run_root)
        else None
    )
    planned_cases = len(metadata["query_ids"])
    trials_per_case = int(metadata["trials_per_case"])
    planned_trials = planned_cases * trials_per_case
    summary = {
        "generated_at": utc_now(),
        "run_id": metadata["run_id"],
        "suite": metadata["suite"],
        "model": metadata["model"],
        "reasoning_effort": metadata["reasoning_effort"],
        "environment": metadata["environment"],
        "cases": {
            "planned": planned_cases,
            "persisted": len(pairs),
            "completed": sum(bool(pair["eligible"]) for pair in pairs),
        },
        "trials": {
            "per_case": trials_per_case,
            "planned": planned_trials,
            "persisted": len(_trial_pairs(pairs)),
            "completed": len(trials),
        },
        "profiles": profiles,
        "quality": quality,
        "retrieval": retrieval,
        "tool_interaction_batches": interactions,
        "quality_conditioned": {
            "both_correct": {
                "trials": len(both_correct_trials),
                "profiles": both_correct_profiles,
                "retrieval": both_correct_retrieval,
                "tool_interaction_batches": both_correct_interactions,
            }
        },
        "case_distribution": {
            metric: _case_distribution(trials, metric)
            for metric in ("input_tokens", "tool_calls", "wall_seconds")
        },
        "tool_behavior": _tool_behavior(trials),
        "zvec_grep_usage_audit": _usage_audit(
            run_root, trials, str(metadata["suite"])
        ),
        "index": {
            "build_wall_seconds": metadata["index_build_wall_seconds"],
            "bytes": metadata["index_bytes"],
            "statistics": metadata["index_statistics"],
        },
        "runtime_preparation": runtime_preparation,
        "evaluator": evaluator,
    }
    report_dir = run_root / "report"
    write_json(report_dir / "summary.json", summary)
    primary_rows = _primary_rows(quality, profiles)
    both_rows = _primary_rows(quality, both_correct_profiles)[1:]
    both_rows.extend(
        _retrieval_rows(both_correct_retrieval, both_correct_interactions)
    )
    distribution_rows = "\n".join(
        "| {metric} | {improved} | {tied} | {regressed} | {median} |".format(
            metric=label,
            improved=summary["case_distribution"][metric]["improved"],
            tied=summary["case_distribution"][metric]["tied"],
            regressed=summary["case_distribution"][metric]["regressed"],
            median=_display(
                summary["case_distribution"][metric]["median_case_change_percent"]
            ),
        )
        for metric, label in (
            ("input_tokens", "Input tokens"),
            ("tool_calls", "Tool calls"),
            ("wall_seconds", "Agent time"),
        )
    )
    behavior = summary["tool_behavior"]
    usage_audit = summary["zvec_grep_usage_audit"]
    index = summary["index"]
    usage_audit_markdown = ""
    if usage_audit.get("status") in {"scored", "partial"}:
        usage_audit_markdown = f"""

## Smoke-test usage audit

A model reviews each Treatment trace to determine whether the Agent used zvec-grep appropriately and whether its results helped the investigation. This diagnostic is separate from the deterministic call counts above.

| Metric | Value |
| --- | ---: |
| Correct and helpful usage | {usage_audit['correct_trials']} / {usage_audit['evaluated_trials']} |
| Expected trials | {usage_audit['expected_trials']} |
"""
    if quality.get("status") in {"scored", "partial"}:
        quality_outcomes_markdown = f"""| Outcome | Trials |
| --- | ---: |
| Both correct | {quality['both_correct']} |
| Baseline only correct | {quality['baseline_only_correct']} |
| Treatment only correct | {quality['treatment_only_correct']} |
| Neither correct | {quality['neither_correct']} |
"""
        both_correct_markdown = f"""Resource use is most directly comparable when both conditions answer correctly; otherwise, it may reflect an unsuccessful trajectory—for example, premature stopping or prolonged, unfocused searching when the model cannot resolve the task—rather than retrieval efficiency. This secondary view therefore compares resource and retrieval behavior on the {len(both_correct_trials)} paired trials where both Baseline and Treatment answered correctly. The primary results above still include all completed trials.

| Metric | Baseline | Treatment (zvec-grep) | Absolute change | Relative change |
| --- | ---: | ---: | ---: | ---: |
{_markdown_metric_rows(both_rows)}
"""
    else:
        quality_outcomes_markdown = "Answer evaluation is pending."
        both_correct_markdown = (
            "Both-correct analysis will be available after answer evaluation."
        )
    markdown = f"""# BrowseComp-Plus paired report

- Run: `{summary['run_id']}`
- Suite: `{summary['suite']}`
- Model: `{summary['model']}`
- Reasoning: `{summary['reasoning_effort']}`
- Completed cases: {summary['cases']['completed']} / {summary['cases']['planned']}
- Completed trials: {summary['trials']['completed']} / {summary['trials']['planned']} ({trials_per_case} per case)

## Primary results

Every completed Baseline and Treatment trial is included in the averages. Changes are calculated as Treatment relative to Baseline.

| Metric | Baseline | Treatment (zvec-grep) | Absolute change | Relative change |
| --- | ---: | ---: | ---: | ---: |
{_markdown_metric_rows(primary_rows)}

## Quality outcomes

{quality_outcomes_markdown}

## Both-correct analysis

{both_correct_markdown}

## Case-level distribution

Each case is compared using the mean of its trials. Negative median change means lower use with zvec-grep.

| Metric | Improved | Tied | Regressed | Median case change (%) |
| --- | ---: | ---: | ---: | ---: |
{distribution_rows}

## Retrieval diagnostics

| Metric | Baseline | Treatment (zvec-grep) | Absolute change | Relative change |
| --- | ---: | ---: | ---: | ---: |
{_markdown_metric_rows(_retrieval_rows(retrieval, interactions))}

## Tool behavior

| Metric | Value |
| --- | ---: |
| Treatment trials using zvec-grep | {behavior['treatment_trials_with_zvec_grep']} / {behavior['eligible_trials']} ({behavior['treatment_trial_adoption_percent']:.2f}%) |
| Treatment zvec-grep calls | {behavior['treatment_zvec_grep_calls']} |
| Successful zvec-grep calls | {behavior['treatment_successful_zvec_grep_calls']} |
| Failed zvec-grep calls | {behavior['treatment_failed_zvec_grep_calls']} |
| Successful calls with empty output | {behavior['treatment_empty_zvec_grep_calls']} |
| Baseline direct `zg` commands | {behavior['baseline_direct_zg_commands']} |
| Baseline zvec-grep MCP calls | {behavior['baseline_zvec_grep_mcp_calls']} |
{usage_audit_markdown}

## zvec-grep index preparation

Index preparation is measured separately and excluded from Agent execution metrics.

| Metric | Value |
| --- | ---: |
| Build time (seconds) | {index['build_wall_seconds']:.2f} |
| Index size (bytes) | {index['bytes']:,} |
| Index statistics | `{_markdown_cell(json.dumps(index['statistics'], sort_keys=True))}` |

## Runtime preparation

| Phase | Wall seconds |
| --- | ---: |
| End-to-end preparation | {runtime_preparation['total_wall_seconds']:.2f} |
| Server startup | {runtime_preparation['server_start_wall_seconds']:.2f} |
| Profile preparation | {runtime_preparation['profile_preparation_wall_seconds']:.2f} |
| `zg --install` | {runtime_preparation['profile_install_wall_seconds']:.2f} |
| Runtime verification and index warmup | {runtime_preparation['warmup_wall_seconds']:.2f} |

## Environment

| Setting | Value |
| --- | --- |
{_environment_rows(summary['environment'])}
"""
    if evaluator:
        markdown += "\n## Evaluator\n\n"
        markdown += (
            f"- Model: `{evaluator['model']}`\n"
            f"- Reasoning: `{evaluator['reasoning_effort']}`\n"
            "- Evaluator usage and time are excluded from Agent execution metrics.\n"
        )
        markdown += """

| Workload | Calls | Input tokens | Cached input tokens | Output tokens | Reasoning output tokens | Wall seconds |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
"""
        for label, key in (
            ("Answer judgements", "answer_judgements"),
            ("zvec-grep usage audits", "zvec_grep_usage_audits"),
            ("Total", "total"),
        ):
            cost = evaluator["cost"][key]
            markdown += (
                f"| {label} | {cost['completed_calls']} | "
                f"{cost['input_tokens']} | {cost['cached_input_tokens']} | "
                f"{cost['output_tokens']} | {cost['reasoning_output_tokens']} | "
                f"{cost['wall_seconds']:.2f} |\n"
            )
    atomic_write_text(report_dir / "summary.md", markdown)
    _write_trial_csv(report_dir, run_root, trials)
    _write_case_csv(report_dir, trials)
    return report_dir
