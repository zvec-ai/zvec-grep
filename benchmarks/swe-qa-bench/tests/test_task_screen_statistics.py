"""Residual-dispersion task screening after fixed score-tail selection."""

from __future__ import annotations

import copy
import math
import statistics
import unittest

from zg_bench.core.errors import SweQaError
from zg_bench.metrics.summary import aggregate_cases, profile_statistics
from zg_bench.metrics.task_screen import (
    DISPERSION_METRICS,
    residual_dispersion,
    screen_tasks,
)


def measured_profile(score_spread=1, *, time_cv=0.1, input_cv=0.1, center=50):
    """Ten tuples, with the middle eight having controlled residual dispersion."""
    scores = [0, *([center - score_spread] * 4), *([center + score_spread] * 4), 100]
    times = [10 * (1 - time_cv / math.sqrt(8))] * 7 + [
        10 * (1 + 7 * time_cv / math.sqrt(8))
    ]
    inputs = [1000 * (1 - input_cv / math.sqrt(8))] * 7 + [
        1000 * (1 + 7 * input_cv / math.sqrt(8))
    ]
    trials = []
    for index, score in enumerate(scores, start=1):
        trials.append(
            {
                "trial_index": index,
                "trial_name": f"trial-{index}",
                "judge": {"total": score, "usage": {"input_tokens": 50}},
                "metrics": {
                    "input_tokens": inputs[index - 2] if 2 <= index <= 9 else 10**8,
                    "output_tokens": index,
                    "tool_calls": index,
                    "agent_wall_seconds": times[index - 2]
                    if 2 <= index <= 9
                    else 10**6,
                    "cost_usd": index / 100,
                },
            }
        )
    return {
        "judge": {"total": statistics.mean(scores), "usage": {"calls": 10}},
        "metrics": {
            key: statistics.mean(t["metrics"][key] for t in trials)
            for key in trials[0]["metrics"]
        },
        "trials": trials,
    }


def task_statistics(values, *, profile_names=("baseline", "zvec-grep")):
    """Synthetic measured dispersions isolate the cross-task reference rule."""
    return [
        {
            "task_id": f"task:{index}",
            "profiles": {
                name: {
                    "filter": {"original_count": 10, "retained_count": 8},
                    "dispersion": {
                        metric: {"status": "available", "value": value}
                        for metric in DISPERSION_METRICS
                    },
                }
                for name in profile_names
            },
        }
        for index, value in enumerate(values, start=1)
    ]


class ResidualDispersionTests(unittest.TestCase):
    def test_dispersion_uses_exact_same_eight_tuples_as_means(self):
        source = measured_profile(3, time_cv=0.4, input_cv=0.7)
        before = copy.deepcopy(source)
        result = profile_statistics(source)
        self.assertEqual(result["filter"]["retained_trial_indexes"], list(range(2, 10)))
        for metric, expected in (
            ("judge", 3 * math.sqrt(8 / 7)),
            ("agent_wall_seconds", 0.4),
            ("input_tokens", 0.7),
        ):
            row = result["dispersion"][metric]
            self.assertEqual(row["status"], "available")
            self.assertEqual(row["sample_count"], 8)
            self.assertAlmostEqual(row["value"], expected)
            self.assertAlmostEqual(row["mean"], result["filtered"][metric])
        self.assertEqual(result["filtered"]["tool_calls"], 5.5)
        self.assertAlmostEqual(result["filtered"]["cost_usd"], 0.055)
        self.assertEqual(source, before)
        self.assertEqual(source["judge"]["usage"]["calls"], 10)

    def test_tied_scores_do_not_independently_trim_cost_outlier(self):
        source = measured_profile(0)
        source["trials"][4]["metrics"]["input_tokens"] = 10**7
        result = profile_statistics(source)
        self.assertIn(5, result["filter"]["retained_trial_indexes"])
        self.assertGreater(result["dispersion"]["input_tokens"]["value"], 2)
        self.assertEqual(result["dispersion"]["judge"]["value"], 0)

    def test_no_evidence_and_one_retained_sample_are_unavailable(self):
        source = measured_profile()
        del source["trials"]
        result = profile_statistics(source)
        for row in result["dispersion"].values():
            self.assertEqual(row["status"], "unavailable_no_trial_evidence")
            self.assertIsNone(row["value"])
            self.assertIsNone(row["sample_count"])
        source = measured_profile()
        source["trials"] = source["trials"][:3]
        result = profile_statistics(source)
        self.assertEqual(result["filter"]["retained_count"], 1)
        for row in result["dispersion"].values():
            self.assertEqual(row["status"], "unavailable_insufficient_samples")
            self.assertIsNone(row["value"])

    def test_two_samples_are_enough_for_sample_sd(self):
        source = measured_profile()
        source["trials"] = source["trials"][:2]
        result = profile_statistics(source)
        self.assertEqual(result["filter"]["status"], "unavailable_insufficient_trials")
        self.assertEqual(result["dispersion"]["judge"]["sample_count"], 2)
        self.assertAlmostEqual(
            result["dispersion"]["judge"]["value"], 49 / math.sqrt(2)
        )

    def test_invalid_or_missing_values_disable_only_that_dispersion(self):
        for value, status in (
            (None, "unavailable_missing_values"),
            (float("nan"), "unavailable_invalid_values"),
            (float("inf"), "unavailable_invalid_values"),
            (-1, "unavailable_invalid_values"),
            (True, "unavailable_invalid_values"),
            ("10", "unavailable_invalid_values"),
        ):
            for metric in DISPERSION_METRICS:
                with self.subTest(value=value, metric=metric):
                    retained = measured_profile()["trials"][1:-1]
                    target, key = (
                        (retained[0]["judge"], "total")
                        if metric == "judge"
                        else (retained[0]["metrics"], metric)
                    )
                    target[key] = value
                    rows = residual_dispersion(retained)
                    self.assertEqual(rows[metric]["status"], status)
                    self.assertIsNone(rows[metric]["value"])
                    for other in set(DISPERSION_METRICS) - {metric}:
                        self.assertEqual(rows[other]["status"], "available")

    def test_zero_resource_mean_is_not_zero_variability(self):
        retained = measured_profile()["trials"][1:-1]
        for trial in retained:
            trial["judge"]["total"] = 0
            trial["metrics"]["input_tokens"] = 0
        rows = residual_dispersion(retained)
        self.assertEqual(rows["judge"]["status"], "available")
        self.assertEqual(rows["judge"]["value"], 0)
        self.assertEqual(rows["input_tokens"]["status"], "unavailable_zero_mean")
        self.assertIsNone(rows["input_tokens"]["value"])


class TaskScreenTests(unittest.TestCase):
    def test_maximum_group_not_mean_and_paired_task_exclusion(self):
        names = tuple(
            f"round{r}:{arm}" for r in (1, 2) for arm in ("base", "node", "rust")
        )
        tasks = task_statistics([1, 2, 3, 4, 5, 6, 7], profile_names=names)
        tasks[-1]["profiles"]["round2:rust"]["dispersion"]["input_tokens"]["value"] = (
            100
        )
        before = copy.deepcopy(tasks)
        result = screen_tasks(tasks)
        self.assertEqual(result["status"], "applied")
        self.assertEqual(result["excluded_task_ids"], ["task:7"])
        reason = result["task_decisions"]["task:7"]["reasons"][0]
        self.assertEqual(reason["source_profile"], "round2:rust")
        self.assertEqual(reason["metric"], "input_tokens")
        self.assertEqual(reason["dispersion"], 100)
        self.assertAlmostEqual(reason["modified_z"], 0.6745 * (100 - 4) / 2)
        for counts in result["profiles"].values():
            self.assertEqual(
                counts,
                {"original_count": 70, "after_trim_count": 56, "final_count": 48},
            )
        self.assertEqual(tasks, before)

    def test_exact_boundary_is_retained_and_only_upper_tail_is_screened(self):
        boundary = 3 + 3.5 * 2 / 0.6745
        tasks = task_statistics([0, 1, 2, 3, 4, 5, boundary])
        result = screen_tasks(tasks)
        self.assertEqual(result["excluded_task_ids"], [])
        self.assertEqual(
            result["task_decisions"]["task:7"]["metrics"]["judge"]["modified_z"], 3.5
        )
        for profile in tasks[-1]["profiles"].values():
            profile["dispersion"]["judge"]["value"] = math.nextafter(boundary, math.inf)
        self.assertEqual(screen_tasks(tasks)["excluded_task_ids"], ["task:7"])
        # A very small dispersion is not an instability candidate.
        low = screen_tasks(task_statistics([0, 96, 97, 98, 99, 100, 101]))
        self.assertLess(
            low["task_decisions"]["task:1"]["metrics"]["judge"]["modified_z"], -3.5
        )
        self.assertEqual(low["excluded_task_ids"], [])

    def test_reference_is_frozen_and_screening_is_single_pass(self):
        result = screen_tasks(task_statistics([1, 2, 3, 4, 5, 12, 100]))
        self.assertEqual(result["excluded_task_ids"], ["task:7"])
        self.assertIn("task:6", result["retained_task_ids"])
        self.assertEqual(result["references"]["judge"]["median"], 4)
        self.assertEqual(result["references"]["judge"]["mad"], 2)
        # Refitting after dropping 100 would incorrectly drop 12 in another pass.
        refitted = screen_tasks(task_statistics([1, 2, 3, 4, 5, 12]))
        self.assertEqual(refitted["excluded_task_ids"], ["task:6"])
        self.assertFalse(result["policy"]["refit_after_exclusion"])
        self.assertEqual(result["policy"]["passes"], 1)

    def test_small_suites_preserve_every_task_and_explain_guard(self):
        for count in range(5):
            with self.subTest(count=count):
                tasks = task_statistics([1, 2, 3, 100][:count])
                result = screen_tasks(tasks)
                self.assertEqual(result["status"], "unavailable_insufficient_tasks")
                self.assertEqual(result["retained_task_count"], count)
                self.assertEqual(result["excluded_task_ids"], [])
                for ref in result["references"].values():
                    self.assertEqual(ref["status"], "unavailable_insufficient_tasks")
                    self.assertIsNone(ref["median"])
                for decision in result["task_decisions"].values():
                    self.assertEqual(decision["status"], "unscreened")

    def test_zero_mad_does_not_invent_epsilon_or_label_constant_suite_stable(self):
        result = screen_tasks(task_statistics([1, 1, 1, 1, 100]))
        self.assertEqual(result["status"], "unavailable_no_metrics")
        self.assertEqual(result["excluded_task_ids"], [])
        for reference in result["references"].values():
            self.assertEqual(reference["status"], "unavailable_zero_mad")
            self.assertEqual(reference["mad"], 0)
            self.assertIsNone(reference["upper_threshold"])

    def test_missing_group_metric_disables_suite_metric_but_not_others(self):
        tasks = task_statistics([1, 2, 3, 4, 100])
        tasks[0]["profiles"]["baseline"]["dispersion"]["input_tokens"] = {
            "status": "unavailable_zero_mean",
            "value": None,
        }
        result = screen_tasks(tasks)
        self.assertEqual(result["status"], "partially_applied")
        ref = result["references"]["input_tokens"]
        self.assertEqual(ref["status"], "unavailable_missing_dispersion")
        self.assertEqual(ref["unavailable_task_ids"], ["task:1"])
        self.assertEqual(result["excluded_task_ids"], ["task:5"])
        reasons = result["task_decisions"]["task:5"]["reasons"]
        self.assertEqual(
            {row["metric"] for row in reasons}, {"judge", "agent_wall_seconds"}
        )
        measure = result["task_decisions"]["task:1"]["metrics"]["input_tokens"]
        self.assertEqual(
            measure["unavailable_profiles"],
            [{"profile": "baseline", "status": "unavailable_zero_mean"}],
        )
        self.assertIsNone(
            result["task_decisions"]["task:5"]["metrics"]["input_tokens"]["modified_z"]
        )

    def test_invalid_dispersion_cannot_be_treated_as_available(self):
        for value in (None, -1, float("inf"), float("nan"), True):
            with self.subTest(value=value):
                tasks = task_statistics([1, 2, 3, 4, 100])
                tasks[0]["profiles"]["baseline"]["dispersion"]["judge"]["value"] = value
                result = screen_tasks(tasks)
                self.assertEqual(
                    result["references"]["judge"]["status"],
                    "unavailable_missing_dispersion",
                )
                self.assertIsNone(result["references"]["judge"]["median"])

    def test_profile_sets_and_task_identities_must_match(self):
        tasks = task_statistics([1, 2, 3, 4, 5])
        del tasks[0]["profiles"]["baseline"]
        with self.assertRaisesRegex(SweQaError, "same profile groups"):
            screen_tasks(tasks)
        tasks = task_statistics([1, 2])
        tasks[1]["task_id"] = tasks[0]["task_id"]
        with self.assertRaisesRegex(SweQaError, "unique task IDs"):
            screen_tasks(tasks)


class TaskScreenAggregateTests(unittest.TestCase):
    def test_final_aggregate_excludes_whole_task_but_preserves_other_views(self):
        cases = []
        for index, spread in enumerate((1, 2, 3, 4, 5, 30), start=1):
            cases.append(
                {
                    "task_id": f"task:{index}",
                    "profiles": {
                        "baseline": measured_profile(spread),
                        "zvec-grep": measured_profile(0, center=75),
                    },
                }
            )
        before = copy.deepcopy(cases)
        result = aggregate_cases(cases)
        self.assertEqual(result["task_filter"]["excluded_task_ids"], ["task:6"])
        for counts in result["task_filter"]["profiles"].values():
            self.assertEqual(
                counts,
                {"original_count": 60, "after_trim_count": 48, "final_count": 40},
            )
        self.assertEqual(result["filter"]["profiles"]["baseline"]["retained_count"], 48)
        self.assertEqual(result["trimmed"]["comparison_samples"]["judge_delta"], 6)
        self.assertEqual(result["raw"]["comparison_samples"]["judge_delta"], 6)
        self.assertEqual(result["comparison_samples"]["judge_delta"], 5)
        self.assertEqual(
            result["task_statistics"][-1]["task_filter"]["status"], "excluded"
        )
        self.assertEqual(result["profiles"]["zvec-grep"]["judge"], 75)
        self.assertEqual(result["profiles"]["baseline"]["judge"], 50)
        self.assertEqual(cases, before)

    def test_mean_version_effect_alone_never_flags_a_task(self):
        cases = [
            {
                "task_id": f"task:{i}",
                "profiles": {
                    "baseline": measured_profile(0, center=90),
                    "zvec-grep": measured_profile(0, center=10 if i == 6 else 90),
                },
            }
            for i in range(7)
        ]
        result = aggregate_cases(cases)
        self.assertEqual(result["task_filter"]["excluded_task_ids"], [])
        self.assertEqual(result["task_filter"]["status"], "unavailable_no_metrics")
        self.assertAlmostEqual(result["comparison"]["judge_delta"], -80 / 7)

    def test_all_tasks_excluded_produces_none_not_zero_or_division_error(self):
        score_spread = [30, 30, 1, 2, 3, 4]
        time_cv = [0.01, 0.02, 1.5, 1.5, 0.03, 0.04]
        input_cv = [0.01, 0.02, 0.03, 0.04, 1.5, 1.5]
        cases = [
            {
                "task_id": f"task:{i}",
                "profiles": {
                    name: measured_profile(s, time_cv=t, input_cv=p)
                    for name in ("baseline", "zvec-grep")
                },
            }
            for i, (s, t, p) in enumerate(
                zip(score_spread, time_cv, input_cv, strict=True)
            )
        ]
        result = aggregate_cases(cases)
        self.assertEqual(result["task_filter"]["retained_task_count"], 0)
        self.assertEqual(result["task_filter"]["excluded_task_count"], 6)
        for profile in result["profiles"].values():
            for key, value in profile.items():
                if key != "usage_scope":
                    self.assertIsNone(value, key)
        self.assertTrue(all(value is None for value in result["comparison"].values()))
        self.assertTrue(
            all(value == 0 for value in result["comparison_samples"].values())
        )
        self.assertEqual(
            result["task_filter"]["profiles"]["baseline"]["final_count"], 0
        )
        self.assertEqual(result["trimmed"]["profiles"]["baseline"]["judge"], 50)
        self.assertEqual(len(result["task_statistics"]), 6)


if __name__ == "__main__":
    unittest.main()
