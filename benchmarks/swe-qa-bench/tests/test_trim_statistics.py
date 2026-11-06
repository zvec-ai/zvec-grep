"""Fixed score-tail selection, linked usage, and equal-weight aggregation."""

from __future__ import annotations

import copy
import unittest

from swe_qa_fixtures import (
    _judged_task_report,
    _session_usage_fixture,
    _set_report_trial_judgements,
)
from zg_bench.metrics.summary import aggregate_cases, profile_statistics
from zg_bench.metrics.usage import SESSION_USAGE_SCOPE, validate_usage_metrics


def profile(scores):
    report = _judged_task_report("task:1")
    _set_report_trial_judgements(report, baseline=scores, zvec=scores)
    return report["cases"][0]["profiles"]["baseline"]


class TrimStatisticsTests(unittest.TestCase):
    def test_selects_exactly_one_minimum_and_maximum_in_one_pass(self):
        for scores, retained, excluded in (
            ([95, 13, 94, 90, 95], [1, 3, 4], [(2, "low"), (5, "high")]),
            ([90, 87, 87, 56, 88], [2, 3, 5], [(1, "high"), (4, "low")]),
            ([98, 100, 73, 91, 90], [1, 4, 5], [(2, "high"), (3, "low")]),
            ([95, 92, 95, 95, 95], [1, 3, 4], [(2, "low"), (5, "high")]),
            ([13, 80, 90, 91, 92], [2, 3, 4], [(1, "low"), (5, "high")]),
            ([90] * 10, list(range(2, 10)), [(1, "low"), (10, "high")]),
            (list(range(10, 101, 10)), list(range(2, 10)), [(1, "low"), (10, "high")]),
        ):
            with self.subTest(scores=scores):
                source = profile(scores)
                before = copy.deepcopy(source)
                result = profile_statistics(source)
                f = result["filter"]
                self.assertEqual(f["status"], "applied")
                self.assertEqual(f["retained_trial_indexes"], retained)
                self.assertEqual(
                    [(r["trial_index"], r["direction"]) for r in f["excluded_trials"]],
                    excluded,
                )
                self.assertEqual(f["original_count"], len(scores))
                self.assertEqual(f["retained_count"], len(scores) - 2)
                self.assertEqual(f["excluded_count"], 2)
                self.assertEqual(result["raw"]["judge"], sum(scores) / len(scores))
                self.assertEqual(
                    result["filtered"]["judge"],
                    sum(scores[i - 1] for i in retained) / len(retained),
                )
                self.assertNotIn("q1", f)
                self.assertNotIn("lower_bound", f)
                self.assertEqual(source, before)

    def test_ties_use_trial_index_and_keep_evidence_order(self):
        source = profile([90] * 10)
        # Shuffled artifacts must select original trial 1 and 10, not array ends.
        order = [5, 10, 3, 8, 1, 7, 4, 9, 2, 6]
        trials = source["trials"]
        source["trials"] = [trials[i - 1] for i in order]
        result = profile_statistics(source)["filter"]
        self.assertEqual(result["retained_trial_indexes"], [5, 3, 8, 7, 4, 9, 2, 6])
        self.assertEqual(
            [(r["trial_index"], r["direction"]) for r in result["excluded_trials"]],
            [(10, "high"), (1, "low")],
        )
        self.assertEqual([t["trial_index"] for t in source["trials"]], order)

    def test_missing_trial_index_uses_original_position(self):
        source = profile([90] * 10)
        for trial in source["trials"]:
            del trial["trial_index"]
        result = profile_statistics(source)["filter"]
        self.assertEqual(result["retained_trial_indexes"], list(range(2, 10)))
        self.assertEqual(
            [row["trial_index"] for row in result["excluded_trials"]], [1, 10]
        )

    def test_one_two_and_three_trial_legacy_samples(self):
        for scores in ([70], [5, 100], [5, 70, 100]):
            with self.subTest(scores=scores):
                result = profile_statistics(profile(scores))
                f = result["filter"]
                self.assertEqual(f["original_count"], len(scores))
                if len(scores) < 3:
                    self.assertEqual(f["status"], "unavailable_insufficient_trials")
                    self.assertEqual(f["retained_count"], len(scores))
                    self.assertEqual(f["excluded_count"], 0)
                    self.assertEqual(result["raw"], result["filtered"])
                else:
                    self.assertEqual(f["status"], "applied")
                    self.assertEqual(f["retained_trial_indexes"], [2])
                    self.assertEqual(f["excluded_count"], 2)
                    self.assertEqual(result["filtered"]["judge"], 70)

    def test_all_usage_components_follow_the_same_score_mask(self):
        source = profile(list(range(10, 101, 10)))
        usage = _session_usage_fixture()
        for index, trial in enumerate(source["trials"]):
            scale = 100 if index in (0, 9) else 1
            split = {
                part: {key: value * scale for key, value in usage[part].items()}
                for part in ("root", "descendants", "total")
            }
            trial["metrics"].update(
                **split["total"],
                usage_scope=SESSION_USAGE_SCOPE,
                session_usage={"scope": SESSION_USAGE_SCOPE, "complete": True, **split},
                agent_wall_seconds=10 * scale,
            )
        before = copy.deepcopy(source)
        result = profile_statistics(source)
        filtered = result["filtered"]
        for key, value in usage["total"].items():
            self.assertAlmostEqual(filtered[key], value)
        self.assertEqual(filtered["agent_wall_seconds"], 10)
        self.assertAlmostEqual(filtered["cost_usd"], 0.6)
        self.assertEqual(result["raw"]["agent_wall_seconds"], 208)
        self.assertEqual(source, before)
        self.assertEqual(source["judge"]["usage"]["calls"], 10)
        validate_usage_metrics(filtered, integer=False)
        for part in ("root", "descendants", "total"):
            for key, value in usage[part].items():
                self.assertAlmostEqual(filtered["session_usage"][part][key], value)

    def test_resource_extremes_never_change_score_selection(self):
        source = profile([90] * 10)
        source["trials"][4]["metrics"].update(
            input_tokens=1000000, agent_wall_seconds=999
        )
        result = profile_statistics(source)
        self.assertEqual(result["filter"]["retained_trial_indexes"], list(range(2, 10)))
        self.assertEqual(result["filter"]["excluded_count"], 2)
        self.assertEqual(result["filtered"]["input_tokens"], (1000000 + 7 * 100) / 8)
        self.assertEqual(result["filtered"]["agent_wall_seconds"], (999 + 7 * 20) / 8)

    def test_task_and_profile_populations_stay_independent_and_equal_weight(self):
        first = _judged_task_report("task:1")
        _set_report_trial_judgements(first, baseline=[75] * 10, zvec=[90] * 9 + [5])
        second = _judged_task_report("task:2")
        _set_report_trial_judgements(
            second, baseline=[50] * 5, zvec=[40] * 3 + [100] * 2
        )
        a, b = first["cases"][0], second["cases"][0]
        before = copy.deepcopy([a, b])
        result = aggregate_cases([a, b])
        # 90 from eight trials and 60 from three trials each receive task weight 1/2.
        self.assertEqual(result["profiles"]["zvec-grep"]["judge"], 75)
        self.assertEqual(result["profiles"]["baseline"]["judge"], 62.5)
        counts = result["filter"]["profiles"]
        self.assertEqual(counts["baseline"]["retained_count"], 11)
        self.assertEqual(counts["zvec-grep"]["retained_count"], 11)
        self.assertNotEqual(
            result["profiles"]["zvec-grep"]["judge"], (90 * 8 + 60 * 3) / 11
        )
        self.assertEqual(result["raw"]["profiles"]["zvec-grep"]["judge"], 72.75)
        self.assertEqual(result["filter"]["method"], "score_trim_min_max_once")
        self.assertEqual(result["filter"]["trim_count_per_tail"], 1)
        self.assertEqual([a, b], before)

    def test_summary_only_legacy_evidence_is_explicitly_unscreened(self):
        source = profile([70] * 5)
        del source["trials"]
        result = profile_statistics(source)
        self.assertEqual(result["filter"]["status"], "unavailable_no_trial_evidence")
        self.assertIsNone(result["filter"]["original_count"])
        self.assertEqual(result["raw"], result["filtered"])


if __name__ == "__main__":
    unittest.main()
