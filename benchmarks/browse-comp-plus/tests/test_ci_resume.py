from __future__ import annotations

import hashlib
import json
import os
import shutil
import tempfile
import unittest
from contextlib import ExitStack
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from zg_bench import evaluate
from zg_bench.artifacts import read_json, sha256_file, write_json
from zg_bench.ci_resume import (
    execution_costs,
    restore_case,
    retry_start,
    seal_evidence,
    verify_evidence,
)
from zg_bench.config import load_config
from zg_bench.models import AttemptResult, TraceSummary
from zg_bench.runner import (
    _needs_run,
    _protocol_fingerprint,
    _run_profile,
    load_suite,
    run_benchmark,
    select_case,
)


class RawCaseResumeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.previous = self.root / "previous"
        self.previous.mkdir()
        self.artifacts = self.root / "artifacts"
        self.config = load_config()
        self.config = replace(
            self.config,
            run=replace(self.config.run, trials_per_case=1, infrastructure_retries=0),
        )
        self.plan = {
            "candidate_commit": "candidate",
            "harness_commit": "harness",
            "configuration_sha256": sha256_file(self.config.path),
            "query_set_sha256": "queries",
            "model_base_url": "https://model.test/v1",
            "suite": "ci",
            "profile_orders": {"827": {"1": ["baseline", "zvec-grep"]}},
        }
        self.plan_path = self.root / "plan.json"
        write_json(self.plan_path, self.plan)
        environment = patch.dict(
            os.environ, GITHUB_RUN_ID="123", GITHUB_RUN_ATTEMPT="2"
        )
        environment.start()
        self.addCleanup(environment.stop)
        self.evidence = self.previous / "raw-827-123-1"
        self.run = self.evidence / "runs" / "20261027-000000"
        protocol = {
            "configuration_sha256": self.plan["configuration_sha256"],
            "query_set_sha256": "queries",
            "model_base_url": self.plan["model_base_url"],
            "profiles_fingerprint": "fresh-profile-hash",
        }
        write_json(
            self.run / "run.json",
            {
                "run_id": self.run.name,
                "query_ids": ["827"],
                "suite": "ci",
                "profile_orders": self.plan["profile_orders"],
                "trials_per_case": 1,
                "protocol": protocol,
                "protocol_fingerprint": _protocol_fingerprint(protocol),
                "profiles_fingerprint": "fresh-profile-hash",
                "status": "partial",
            },
        )
        write_json(
            self.evidence / "ci.json",
            {
                "workflow_run_id": "123",
                "workflow_run_attempt": "1",
                "case_id": "827",
                "candidate_commit": "candidate",
                "harness_commit": "harness",
                "plan_sha256": sha256_file(self.plan_path),
            },
        )
        shutil.copyfile(self.plan_path, self.evidence / "plan.json")
        shutil.copyfile(self.config.path, self.evidence / "benchmark.toml")
        self.result("baseline", "completed", response="wrong answer")
        self.result("zvec-grep", "infrastructure_failed")
        write_json(
            self.run / "evaluation/results/baseline/827/trial-001.json",
            {
                "status": "completed",
                "correct": False,
                "query_id": "827",
                "profile": "baseline",
                "trial_index": 1,
                "candidate_sha256": hashlib.sha256(b"wrong answer").hexdigest(),
                "case_result_sha256": sha256_file(
                    self.trial(self.run, "baseline") / "result.json"
                ),
                "judge_prompt_sha256": evaluate.JUDGE_PROMPT_SHA256,
                "judge_schema_sha256": evaluate.JUDGE_SCHEMA_SHA256,
                "evaluation_source_sha256": evaluate.EVALUATION_SOURCE_SHA256,
            },
        )
        # These should never be restored as execution credentials.
        write_json(self.run / "profiles/baseline/auth.json", {"credential": "old"})
        seal_evidence(self.evidence)

    def trial(self, run, profile):
        return run / "cases/827" / profile / "trials/trial-001"

    def result(self, profile, status, *, response="", number=1):
        trial = self.trial(self.run, profile)
        attempt = trial / "attempts" / f"attempt-{number:03d}"
        attempt.mkdir(parents=True, exist_ok=True)
        for name in ("events.jsonl", "stderr.log", "response.md"):
            (attempt / name).write_text(response)
        result = AttemptResult(
            query_id="827",
            profile=profile,
            trial_index=1,
            status=status,
            attempt=number,
            started_at="2026-10-27T00:00:00Z",
            finished_at="2026-10-27T00:01:00Z",
            wall_seconds=60,
            exit_code=0 if status == "completed" else 1,
            infrastructure_failure=status == "infrastructure_failed",
            trace=TraceSummary(
                thread_id="session",
                final_response=response,
                last_agent_message=response,
                turn_completed=status == "completed",
                usage=None,
                tool_calls=(),
                observed_docids=(),
                errors=(),
            ),
            paths={"events": str(attempt / "events.jsonl")},
        )
        write_json(attempt / "result.json", result.to_dict())
        write_json(trial / "result.json", result.to_dict())
        return result

    def restore(self):
        run_id = restore_case(
            self.artifacts, self.previous, self.plan_path, self.plan, "827", self.config
        )
        return self.artifacts / "runs" / run_id if run_id else None

    def test_wrong_completed_answer_and_judgement_are_reused_without_credentials(self):
        restored = self.restore()
        selected = self.trial(restored, "baseline") / "result.json"
        self.assertFalse(_needs_run(self.config, selected))
        self.assertEqual(
            selected.read_bytes(),
            (self.trial(self.run, "baseline") / "result.json").read_bytes(),
        )
        self.assertFalse(
            read_json(restored / "evaluation/results/baseline/827/trial-001.json")[
                "correct"
            ]
        )
        self.assertFalse((restored / "profiles").exists())
        self.assertEqual(
            read_json(restored / "run.json")["ci_resume_history"][0][
                "automatic_retries"
            ],
            0,
        )

    def test_zero_automatic_retries_allows_one_new_attempt_only(self):
        restored = self.restore()
        selected = self.trial(restored, "zvec-grep") / "result.json"
        self.assertTrue(_needs_run(self.config, selected))
        self.assertEqual(retry_start(selected), 2)
        final = self.result("zvec-grep", "infrastructure_failed", number=2)

        def execute(*args, **kwargs):
            self.assertEqual(kwargs["attempt"], 2)
            write_json(kwargs["output_dir"] / "result.json", final.to_dict())
            return final

        with (
            patch("zg_bench.runner.run_attempt", side_effect=execute) as model,
            patch("zg_bench.runner.validate_workspace"),
        ):
            self.call_profile(restored)
            self.call_profile(restored)
        self.assertEqual(model.call_count, 1)
        self.assertFalse(_needs_run(self.config, selected))
        self.assertTrue(
            (selected.parent / "attempts/attempt-001/result.json").is_file()
        )
        origin = read_json(selected.parent / "attempts/attempt-002/origin.json")
        self.assertEqual(origin["workflow_run_attempt"], "2")

    def call_profile(self, run):
        return _run_profile(
            self.config,
            self.artifacts,
            run,
            {"query_id": "827", "query": "question"},
            "zvec-grep",
            1,
            self.config.run.model,
            self.config.run.reasoning_effort,
            run / "profiles",
            "opencode",
        )

    def test_nonretryable_failure_can_only_retry_in_new_manual_attempt(self):
        self.result("zvec-grep", "failed")
        seal_evidence(self.evidence)
        restored = self.restore()
        selected = self.trial(restored, "zvec-grep") / "result.json"
        self.assertTrue(_needs_run(self.config, selected))
        failed = read_json(selected)
        failed["attempt"] = 2
        write_json(selected, failed)
        self.assertFalse(_needs_run(self.config, selected))

    def test_cancelled_attempt_without_result_preserves_raw_and_uses_next_number(self):
        trial = self.trial(self.run, "zvec-grep")
        (trial / "result.json").unlink()
        (trial / "attempts/attempt-001/result.json").unlink()
        seal_evidence(self.evidence)
        restored = self.restore()
        self.assertEqual(
            retry_start(self.trial(restored, "zvec-grep") / "result.json"), 2
        )
        self.assertTrue(
            (
                self.trial(restored, "zvec-grep") / "attempts/attempt-001/events.jsonl"
            ).is_file()
        )
        rows = read_json(restored / "run.json")["ci_resume_history"][0]["trials"]
        self.assertEqual(rows[1]["prior_attempts_without_result"], 1)

    def test_cancellation_after_complete_result_before_selected_publish_reuses_answer(
        self,
    ):
        (self.trial(self.run, "baseline") / "result.json").unlink()
        seal_evidence(self.evidence)
        restored = self.restore()
        self.assertFalse(
            _needs_run(self.config, self.trial(restored, "baseline") / "result.json")
        )

    def test_missing_or_empty_previous_directory_starts_fresh(self):
        self.assertIsNone(
            restore_case(
                self.artifacts,
                self.root / "missing",
                self.plan_path,
                self.plan,
                "827",
                self.config,
            )
        )
        shutil.rmtree(self.evidence)
        self.assertIsNone(self.restore())

    def test_setup_failed_before_run_starts_fresh(self):
        shutil.rmtree(self.evidence / "runs")
        seal_evidence(self.evidence)
        self.assertIsNone(self.restore())

    def test_cancelled_download_before_plan_starts_fresh_without_importing(self):
        shutil.rmtree(self.evidence / "runs")
        (self.evidence / "plan.json").unlink()
        identity = read_json(self.evidence / "ci.json")
        del identity["plan_sha256"]
        write_json(self.evidence / "ci.json", identity)
        seal_evidence(self.evidence)
        self.assertIsNone(self.restore())
        self.assertFalse((self.artifacts / "runs").exists())

    def test_missing_plan_with_existing_execution_still_fails_closed(self):
        (self.evidence / "plan.json").unlink()
        identity = read_json(self.evidence / "ci.json")
        del identity["plan_sha256"]
        write_json(self.evidence / "ci.json", identity)
        seal_evidence(self.evidence)
        with self.assertRaisesRegex(ValueError, "CI plan"):
            self.restore()

    def test_mutated_raw_and_missing_checksum_fail_closed(self):
        (self.run / "run.json").write_text("{}")
        with self.assertRaisesRegex(ValueError, "integrity"):
            self.restore()
        (self.evidence / "evidence-manifest.json").unlink()
        with self.assertRaisesRegex(ValueError, "manifest"):
            self.restore()

    def test_duplicate_artifact_and_cross_plan_fail_closed(self):
        shutil.copytree(self.evidence, self.previous / "duplicate")
        with self.assertRaisesRegex(ValueError, "duplicate"):
            self.restore()
        shutil.rmtree(self.previous / "duplicate")
        identity = read_json(self.evidence / "ci.json")
        for key in (
            "plan_sha256",
            "candidate_commit",
            "harness_commit",
            "workflow_run_id",
            "case_id",
        ):
            with self.subTest(key=key):
                write_json(self.evidence / "ci.json", {**identity, key: "different"})
                seal_evidence(self.evidence)
                with self.assertRaisesRegex(ValueError, "CI plan"):
                    self.restore()
        write_json(self.evidence / "ci.json", identity)

    def test_same_or_future_workflow_attempt_rejected(self):
        os.environ["GITHUB_RUN_ATTEMPT"] = "1"
        with self.assertRaisesRegex(ValueError, "CI plan"):
            self.restore()

    def test_selected_raw_mismatch_and_incomplete_raw_rejected(self):
        selected = self.trial(self.run, "baseline") / "result.json"
        raw = selected.read_bytes()
        selected.write_bytes(raw.replace(b"wrong answer", b"right answer"))
        seal_evidence(self.evidence)
        with self.assertRaisesRegex(ValueError, "selected result"):
            self.restore()
        selected.write_bytes(raw)
        (selected.parent / "attempts/attempt-001/events.jsonl").unlink()
        seal_evidence(self.evidence)
        with self.assertRaisesRegex(ValueError, "incomplete"):
            self.restore()

    def test_symlink_rejected_even_with_an_existing_manifest(self):
        (self.evidence / "linked").symlink_to(self.root / "plan.json")
        with self.assertRaisesRegex(ValueError, "symlink"):
            verify_evidence(self.evidence)

    def test_successful_judge_reuse_requires_unchanged_full_result(self):
        restored = self.restore()
        judgement = restored / "evaluation/results/baseline/827/trial-001.json"

        def valid():
            return evaluate._completed_judgement(
                judgement,
                run_root=restored,
                profile="baseline",
                query_id="827",
                trial_index=1,
            )

        self.assertTrue(valid())
        selected = self.trial(restored, "baseline") / "result.json"
        result = read_json(selected)
        result["attempt"] = 2  # Even the same answer from a different execution is new.
        write_json(selected, result)
        self.assertFalse(valid())

    def test_costs_include_failed_attempt_and_explicitly_unknown_cancelled_usage(self):
        restored = self.restore()
        incomplete = self.trial(restored, "zvec-grep") / "attempts/attempt-002"
        incomplete.mkdir()
        (incomplete / "events.jsonl").write_text("partial stream")
        cost = execution_costs(restored)["profiles"]["zvec-grep"]
        self.assertEqual(cost["attempts"], 2)
        self.assertEqual(cost["failed_attempts"], 1)
        self.assertEqual(cost["incomplete_attempts"], 1)
        self.assertEqual(cost["known_wall_seconds"], 60)
        self.assertEqual(cost["wall_seconds_unavailable"], 1)
        self.assertEqual(cost["usage_unavailable"], 2)
        self.assertEqual(cost["rows"][0]["query_id"], "827")

    def test_an_interrupted_current_attempt_does_not_gain_an_automatic_retry(self):
        restored = self.restore()
        incomplete = self.trial(restored, "zvec-grep") / "attempts/attempt-002"
        incomplete.mkdir()
        (incomplete / "events.jsonl").write_text("partial stream")
        with patch("zg_bench.runner.run_attempt") as execute:
            with self.assertRaisesRegex(RuntimeError, "budget exhausted"):
                self.call_profile(restored)
            execute.assert_not_called()

    def test_newer_cumulative_artifact_preserves_success_and_history(self):
        newer = self.previous / "raw-827-123-2"
        shutil.copytree(self.evidence, newer)
        identity = read_json(newer / "ci.json")
        identity["workflow_run_attempt"] = "2"
        write_json(newer / "ci.json", identity)
        seal_evidence(newer)
        os.environ["GITHUB_RUN_ATTEMPT"] = "3"
        restored = self.restore()
        self.assertEqual(
            len(read_json(restored / "run.json")["ci_resume_history"][0]["sources"]), 2
        )
        shutil.rmtree(self.artifacts)
        # Even valid checksums cannot make a newer artifact discard old attempts.
        shutil.rmtree(newer / "runs" / self.run.name / "cases/827/baseline")
        seal_evidence(newer)
        with self.assertRaisesRegex(ValueError, "discarded"):
            self.restore()

    def test_profiles_are_recreated_and_must_match_before_any_model_execution(self):
        self.result("zvec-grep", "completed", response="reference")
        seal_evidence(self.evidence)
        restored = self.restore()
        query_path = self.artifacts / "source/browsecomp_plus_decrypted.jsonl"
        query_path.parent.mkdir()
        queries = [
            {
                "query_id": case,
                "query": "Question",
                "answer": "reference",
                "evidence_docs": [],
                "gold_docs": [],
            }
            for case in ("628", "700", "966", "827", "241")
        ]
        query_path.write_text("".join(json.dumps(row) + "\n" for row in queries))
        metadata = read_json(restored / "run.json")
        metadata.update(
            suite_selection=select_case(load_suite("ci", queries), "827").to_dict(),
            model=self.config.run.model,
            reasoning_effort=self.config.run.reasoning_effort,
            profiles_manifest=str((restored / "profiles/manifest.json").resolve()),
        )
        write_json(restored / "run.json", metadata)
        with ExitStack() as stack:
            for name in ("prepared_dataset", "prepared_corpus", "prepared_index"):
                stack.enter_context(patch("zg_bench.runner." + name, return_value=True))
            stack.enter_context(
                patch(
                    "zg_bench.runner.resolve_executable", return_value=Path("/fake/bin")
                )
            )
            stack.enter_context(
                patch(
                    "zg_bench.runner.run_command",
                    return_value=SimpleNamespace(ok=True, stdout="1.18.4"),
                )
            )
            prepare = stack.enter_context(patch("zg_bench.runner.prepare_profiles"))
            stack.enter_context(
                patch(
                    "zg_bench.runner.validate_profiles",
                    return_value={"fingerprint": "changed-profile-hash"},
                )
            )
            execute = stack.enter_context(patch("zg_bench.runner.run_attempt"))
            with self.assertRaisesRegex(RuntimeError, "profile fingerprint"):
                run_benchmark(
                    self.config,
                    self.artifacts,
                    suite="ci",
                    run_id=restored.name,
                    fresh_profiles=True,
                )
            prepare.assert_called_once()
            execute.assert_not_called()
            stack.enter_context(
                patch(
                    "zg_bench.runner.validate_profiles",
                    return_value={"fingerprint": "fresh-profile-hash"},
                )
            )
            actual = stack.enter_context(
                patch(
                    "zg_bench.runner._run_protocol", return_value=metadata["protocol"]
                )
            )
            stack.enter_context(patch("zg_bench.report.generate_report"))
            run_benchmark(
                self.config,
                self.artifacts,
                suite="ci",
                run_id=restored.name,
                fresh_profiles=True,
            )
            self.assertEqual(prepare.call_count, 2)
            actual.assert_called_once()
            execute.assert_not_called()
            self.assertEqual(read_json(restored / "run.json")["status"], "completed")


if __name__ == "__main__":
    unittest.main()
