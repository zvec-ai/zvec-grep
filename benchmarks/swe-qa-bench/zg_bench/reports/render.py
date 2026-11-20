"""Render and write SWE-QA reports without calling a model."""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any

from zg_bench.core.errors import SweQaError
from zg_bench.metrics.summary import aggregate_cases
from zg_bench.metrics.usage import (
    LEGACY_USAGE_SCOPE,
    SESSION_USAGE_SCOPE,
)


def _fmt_number(value: int | float, *, decimals: int = 2) -> str:
    return f"{float(value):,.{decimals}f}"


def _fmt_delta(value: float | int | None, *, suffix: str = "") -> str:
    if value is None:
        return "N/A"
    numeric = float(value)
    if round(numeric, 2) == 0:
        numeric = 0.0
    return f"{numeric:+.2f}{suffix}"


def metric_cell(
    baseline: int | float | None,
    zvec: int | float | None,
    reduction: float | None,
    *,
    decimals: int = 2,
) -> str:
    if baseline is None or zvec is None:
        return "N/A"
    left = _fmt_number(baseline, decimals=decimals)
    right = _fmt_number(zvec, decimals=decimals)
    # The stored comparison remains a reduction: positive means zvec-grep used
    # less. In the table, present every third value as zvec-grep minus baseline,
    # so efficiency wins are negative while Judge improvements stay positive.
    change = None if reduction is None else -reduction
    return f"{left} / {right} / {_fmt_delta(change, suffix='%')}"


def render_report(report: dict[str, Any]) -> str:
    usage_scope = report.get("usage_scope", LEGACY_USAGE_SCOPE)
    # Old artifacts may contain a different aggregate policy. Rebuild from the
    # raw cases so the Markdown and current statistical method always agree.
    aggregate = aggregate_cases(report["cases"])
    filtering = aggregate["filter"]
    task_filter = aggregate["task_filter"]

    def optional_number(value, *, decimals=2):
        return "N/A" if value is None else _fmt_number(value, decimals=decimals)

    def table_row(label, profiles, comparison):
        baseline, zvec = (profiles[name] for name in ("baseline", "zvec-grep"))
        cells = [
            label,
            f"{optional_number(baseline['judge'])} / {optional_number(zvec['judge'])} / {_fmt_delta(comparison['judge_delta'])}",
        ]
        for metric, change in (
            ("input_tokens", "input_token_reduction_pct"),
            ("tool_calls", "toolcall_reduction_pct"),
            ("agent_wall_seconds", "time_reduction_pct"),
        ):
            cells.append(
                metric_cell(baseline[metric], zvec[metric], comparison[change])
            )
        return "| " + " | ".join(cells) + " |"

    lines = ["# SWE-QA-Bench CI report", ""]
    missing_tasks = report.get("gate", {}).get("missing_tasks", [])
    if missing_tasks:
        lines.extend(
            (
                f"**Incomplete run:** aggregated {len(report['cases'])} completed task(s); "
                f"{len(missing_tasks)} expected task(s) did not produce reports: "
                + ", ".join(f"`{task}`" for task in missing_tasks)
                + ". Missing tasks are not assigned zero values and are excluded from every Aggregate metric.",
                "",
            )
        )
    lines.extend(
        (
            "All comparison cells use `baseline / zvec-grep / change`. Resource savings are negative; Judge gains are positive.",
            "",
            "| Summary | Judge mean | Input token mean | Tool call mean | Agent time mean (s) |",
            "|---|---:|---:|---:|---:|",
            table_row(
                "Raw", aggregate["raw"]["profiles"], aggregate["raw"]["comparison"]
            ),
            table_row(
                "After score trim, before task screening",
                aggregate["trimmed"]["profiles"],
                aggregate["trimmed"]["comparison"],
            ),
            table_row(
                "**After MAD task screening**",
                aggregate["profiles"],
                aggregate["comparison"],
            ),
            "",
            "| Profile | Completed tasks | Raw samples | After score trim | After MAD task screening |",
            "|---|---:|---:|---:|---:|",
        )
    )
    for name, counts in task_filter["profiles"].items():
        values = [
            str(counts[key]) if counts[key] is not None else "N/A"
            for key in ("original_count", "after_trim_count", "final_count")
        ]
        lines.append(
            f"| {name} | {filtering['task_count']} | " + " | ".join(values) + " |"
        )
    lines.extend(
        (
            "",
            f"Task coverage after MAD screening: **{task_filter['retained_task_count']}/{task_filter['original_task_count']} completed tasks**; "
            f"{task_filter['excluded_task_count']} excluded. Screening status: `{task_filter['status']}`. "
            "All profiles use the same retained task set. Missing execution reports remain separate completion failures.",
            "",
            "**Trim one minimum and one maximum score** separately within each model × task × profile. "
            "Sort once by `(judge.total, trial_index)` and remove the first and last trials; "
            "use the original position when `trial_index` is missing. "
            "Score ties use trial order, never time or tokens. Ten trials therefore retain eight, even when all scores tie. "
            "Groups with fewer than three trials retain all samples and are marked as insufficient for trimming.",
            "",
            "An excluded trial's score, time, tokens, cost and tool calls are removed together from statistical means; "
            "individual trials are selected by score, not independently by time or token values. "
            "Each task is first averaged over its retained trials, then **tasks in each displayed stage receive equal weight**. "
            "Aggregate resources are means, not sums. Every Aggregate change is calculated directly from the displayed Aggregate values, not an average of task percentages. "
            "Raw trials, actual judge usage and completion failures are preserved, including statistically excluded tasks.",
            "",
            "**MAD task screening:** use the retained trials' sample standard deviation for score and coefficient of variation "
            "(sample standard deviation / mean) for time and input tokens. For each metric, take the maximum dispersion across "
            "the profiles in this report, then compute its median and MAD across tasks. Exclude a task from every profile if any "
            "available metric has `modified Z = 0.6745 × (dispersion − median) / MAD > 3.5`. "
            "This is a single upper-tail pass; the reference is not refitted after exclusions. "
            "At least five tasks are required as an engineering safeguard, not an official statistical minimum. "
            "A metric is not screened if any task/profile lacks its dispersion or its across-task MAD is zero; "
            "other available metrics may still be screened. Unavailable references are reported below and do not trigger exclusions.",
            "",
            "Reference profiles: " + ", ".join(f"`{name}`" for name in task_filter["profiles"]) + ". "
            "Only this report's populations are used; historical rounds or a historical six-group comparison are not silently added. "
            "The [NIST modified-Z guidance](https://www.itl.nist.gov/div898/handbook/eda/section3/eda35h.htm) "
            "labels potential outliers above an absolute threshold of 3.5. This benchmark applies its upper tail to task dispersion "
            "as a reporting rule, not a p-value or proof that excluded executions are invalid.",
            "",
            "### Task-screening references",
            "",
            "| Metric | Status | Across-task median | MAD | Upper dispersion threshold | Tasks with unavailable dispersion |",
            "|---|---|---:|---:|---:|---|",
        )
    )
    for metric, reference in task_filter["references"].items():
        lines.append(
            f"| {metric} | {reference['status']} | "
            + " | ".join(optional_number(reference.get(key), decimals=6) for key in ("median", "mad", "upper_threshold"))
            + " | " + (", ".join(reference.get("unavailable_task_ids", [])) or "—") + " |"
        )
    lines.extend(
        (
            "",
            "### Tasks excluded from the final aggregate",
            "",
            "| Task | Source profile | Metric | Max dispersion | Median | MAD | Modified Z | Upper threshold | Reason |",
            "|---|---|---|---:|---:|---:|---:|---:|---|",
        )
    )
    for task_id in task_filter["excluded_task_ids"]:
        for reason in task_filter["task_decisions"][task_id]["reasons"]:
            reference = task_filter["references"][reason["metric"]]
            lines.append(
                f"| {task_id} | {reason['source_profile']} | {reason['metric']} | "
                + " | ".join(optional_number(value, decimals=6) for value in (
                    reason["dispersion"], reference.get("median"), reference.get("mad"),
                    reason["modified_z"], reason["upper_threshold"],
                ))
                + " | modified Z > 3.5; excluded from every profile |"
            )
    if not task_filter["excluded_task_ids"]:
        lines.extend(("", "No tasks were excluded by MAD screening."))
    lines.extend(
        (
            "",
            "### Per-task results: After score trimming",
            "",
            "All completed tasks are listed, including those excluded from the final aggregate.",
            "",
            "| Case | Judge mean | Input token mean | Tool call mean | Agent time mean (s) | Retained/original (baseline / zvec-grep) | MAD task status |",
            "|---|---:|---:|---:|---:|---|---|",
        )
    )
    exclusions = []
    for task in aggregate["task_statistics"]:
        profiles = {name: row["filtered"] for name, row in task["profiles"].items()}
        counts = []
        for name, row in task["profiles"].items():
            f = row["filter"]
            counts.append(
                f"{f['retained_count']}/{f['original_count']}"
                if f["original_count"] is not None
                else "N/A (unscreened)"
            )
            for excluded in f["excluded_trials"]:
                exclusions.append((task["task_id"], name, f, excluded))
        lines.append(
            table_row(task["task_id"], profiles, task["comparison"])
            + " "
            + " / ".join(counts)
            + f" | {task['task_filter']['status']} |"
        )
    lines.append("")
    for name in filtering["profiles"]:
        legacy_tasks = [
            task["task_id"]
            for task in aggregate["task_statistics"]
            if task["profiles"][name]["filter"]["status"]
            == "unavailable_no_trial_evidence"
        ]
        insufficient_tasks = [
            task["task_id"]
            for task in aggregate["task_statistics"]
            if task["profiles"][name]["filter"]["status"]
            == "unavailable_insufficient_trials"
        ]
        if legacy_tasks:
            lines.extend(
                (
                    f"**Unscreened legacy evidence ({name}):** "
                    + ", ".join(legacy_tasks)
                    + ". Raw repetitions are unavailable; these tasks retain their supplied means. Sample counts are N/A.",
                    "",
                )
            )
        if insufficient_tasks:
            lines.extend(
                (
                    f"**Insufficient trials for trimming ({name}):** "
                    + ", ".join(insufficient_tasks)
                    + ". Fewer than three trials are available; all samples are retained and their counts remain visible.",
                    "",
                )
            )
    if exclusions:
        lines.extend(
            (
                "<details>",
                f"<summary>Excluded trials ({len(exclusions)})</summary>",
                "",
                "Trial indices refer to the original profile evidence in report.json. The low and high tails are selected once from the original score ordering.",
                "",
                "| Task | Profile | Trial | Score | Tail |",
                "|---|---|---:|---:|---|",
            )
        )
        for task_id, name, _, trial in exclusions:
            lines.append(
                f"| {task_id} | {name} | {trial['trial_index']} | {trial['score']:.2f} | "
                + f"{trial['direction']} |"
            )
        lines.extend(("", "</details>", ""))
    else:
        lines.extend(("No trials were excluded by score trimming.", ""))
    samples = aggregate["comparison_samples"]
    task_count = len(report["cases"])
    lines.extend(
        (
            "Aggregate comparison sample counts: "
            f"Judge n={samples['judge_delta']}/{task_count}, "
            f"input_token n={samples['input_token_reduction_pct']}/{task_count}, "
            f"toolcall n={samples['toolcall_reduction_pct']}/{task_count}, "
            f"time n={samples['time_reduction_pct']}/{task_count}.",
            "",
        )
    )
    completion_statement = (
        "The completion gate failed because one or more expected tasks did not produce a valid judged pair. "
        "The displayed Aggregate covers completed tasks only."
        if missing_tasks
        else "The hard gate requires every expected trial and every judge call to succeed, including trimmed trials and MAD-excluded tasks."
    )
    judge_metadata = report["judge"]
    generation_description = (
        "Judge generation: "
        f"`temperature={judge_metadata['temperature']}`, "
        f"`seed={judge_metadata.get('seed', 'unrecorded')}`, "
        f"`enable_thinking={str(judge_metadata['enable_thinking']).lower()}`, "
        f"`reasoning_effort={judge_metadata['reasoning_effort']}`, "
        f"`max_tokens={judge_metadata['max_tokens']}`, "
        f"`response_format={(judge_metadata['response_format'] or {}).get('type', 'unset')}`."
        if "enable_thinking" in judge_metadata
        else "Legacy report: judge thinking, reasoning effort, and output limit were not recorded."
    )
    lines.extend(
        (
            f"Judge: **{judge_metadata['label']}** ({judge_metadata['model']} self-judge).",
            "",
            generation_description,
            "",
        )
    )
    if (
        judge_metadata["model"] == "qwen3.8-max"
        and judge_metadata.get("enable_thinking") is True
    ):
        lines.extend(
            (
                "Qwen thinking-mode parameter interpretation: the metadata above records requested values. "
                "The [provider documentation](https://help.aliyun.com/zh/model-studio/qwen-api-via-openai-chat-completions) "
                "specifies that temperatures below 0.6 are raised to 0.6 and `reasoning_effort=high` maps to `xhigh`; "
                "`max_tokens` limits the final answer and excludes reasoning tokens. "
                "These are documented provider behaviors, not effective values observed in the response.",
                "",
            )
        )
    lines.extend(
        (
            "This run is **report-only**. Numeric scores and the Aggregate filter are not code-review or merge gates. "
            + completion_statement,
            "",
            f"Usage scope: **`{usage_scope}`**. "
            + (
                "Tokens and tool calls include the root session and every recursively linked subagent session. "
                "Input includes uncached, cache-read, and cache-write tokens; output includes text and reasoning tokens. "
                "Per-session detail remains in JSON evidence. "
                "Agent wall time already includes awaited subagents and excludes usage-export overhead; child durations are not added. "
                "The judge evaluates only the root final answer. Background title/summary calls not persisted in the session database are outside this scope, so this is not a complete provider bill."
                if usage_scope == SESSION_USAGE_SCOPE
                else "Legacy evidence covers the root trajectory only; subagent usage is unknown and may be omitted. "
                "Do not interpret these resource comparisons as complete agent usage or combine them with session-tree reports."
            ),
            "",
        )
    )
    return "\n".join(lines)


def write_report(report: dict[str, Any], output_dir: Path) -> None:
    markdown = render_report(report)
    try:
        output_dir.mkdir(parents=True, exist_ok=True)
        (output_dir / "report.json").write_text(
            json.dumps(report, ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )
        (output_dir / "report.md").write_text(markdown, encoding="utf-8")
    except OSError as error:
        raise SweQaError(f"could not write SWE-QA report: {error}") from error

    summary_path = os.environ.get("GITHUB_STEP_SUMMARY", "").strip()
    if summary_path:
        try:
            with Path(summary_path).open("a", encoding="utf-8") as summary:
                summary.write(markdown)
                if not markdown.endswith("\n"):
                    summary.write("\n")
        except OSError as error:
            raise SweQaError(
                f"could not append GitHub step summary: {error}"
            ) from error
