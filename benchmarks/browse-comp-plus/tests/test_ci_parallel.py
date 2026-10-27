from __future__ import annotations

import contextlib
import hashlib
import io
import json
import os
import shutil
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from zg_bench import evaluate
from zg_bench.artifacts import read_json, sha256_file, write_json
from zg_bench.ci_parallel import (
    aggregate,
    create_plan,
    main,
    restore_queries,
    select_shards,
    validate_plan,
)
from zg_bench.ci_resume import seal_evidence
from zg_bench.config import load_config
from zg_bench.dataset import load_queries
from zg_bench.models import PROFILES
from zg_bench.runner import _write_pair, load_suite, select_case


class ParallelCIIntegrationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.artifacts = self.root / "prepared"
        self.shards = self.root / "shards"
        self.shards.mkdir()
        self.output = self.root / "aggregate"
        self.config = load_config()
        environment = patch.dict(
            os.environ,
            BROWSE_CANDIDATE_COMMIT="candidate-sha",
            BROWSE_HARNESS_COMMIT="harness-sha",
            GITHUB_RUN_ID="workflow-1",
            BROWSE_COMP_MODEL_BASE_URL="https://qwen.example.test/v1",
        )
        environment.start()
        self.addCleanup(environment.stop)
        self.queries = self.artifacts / "source/browsecomp_plus_decrypted.jsonl"
        self.queries.parent.mkdir(parents=True)
        self.queries.write_text(
            "".join(
                json.dumps(
                    {
                        "query_id": case,
                        "query": f"Question {case}",
                        "answer": "reference",
                        "evidence_docs": [],
                        "gold_docs": [],
                    }
                )
                + "\n"
                for case in ("628", "700", "966", "827", "241", "770")
            )
        )
        # An aggregation must never start OpenCode or any other child process.
        process = patch(
            "subprocess.Popen",
            side_effect=AssertionError("unexpected model/process call"),
        )
        process.start()
        self.addCleanup(process.stop)

    def plan(self, suite="ci"):
        with contextlib.redirect_stdout(io.StringIO()):
            self.plan_path = create_plan(self.artifacts, suite, self.root / "plan")
        self.plan_data = read_json(self.plan_path)
        return self.plan_data

    def shard(self, case, *, attempt=1, name=None):
        """Produce raw trial and judge artifacts in the actual on-disk layout."""
        evidence = self.shards / (name or f"case-{case}-attempt-{attempt}")
        run = evidence / "runs" / f"worker-{case}"
        selection = load_suite(self.plan_data["suite"], load_queries(self.queries))
        write_json(
            evidence / "ci.json",
            {
                "candidate_commit": "candidate-sha",
                "harness_commit": "harness-sha",
                "plan_sha256": sha256_file(self.plan_path),
                "workflow_run_attempt": str(attempt),
                "workflow_run_id": "workflow-1",
                "case_id": case,
            },
        )
        write_json(
            run / "run.json",
            {
                "run_id": run.name,
                "created_at": "2000-01-01T00:00:00Z",
                "finished_at": "2000-01-01T00:01:00Z",
                "query_ids": [case],
                "suite": self.plan_data["suite"],
                "suite_selection": select_case(selection, case).to_dict(),
                "trials_per_case": self.config.run.trials_per_case,
                "model": self.config.run.model,
                "reasoning_effort": self.config.run.reasoning_effort,
                "profile_orders": {case: self.plan_data["profile_orders"][case]},
                "corpus_fingerprint": "same-corpus",
                "index_fingerprint": "same-index",
                "index_statistics": {"documents": 100195},
                "index_build_wall_seconds": 9721.0,
                "index_bytes": 7897230894,
                "environment": {
                    "opencode_version": "1.18.4",
                    "model_base_url": "https://qwen.example.test/v1",
                    "available_cpu_count": 4,
                    "logical_cpu_count": 4,
                    "operating_system": "Ubuntu 24.04",
                    "platform": "Linux",
                    "machine": "x86_64",
                    "cpu_model": "fixture CPU",
                    "python": "3.12",
                    "opencode_bin": "/usr/bin/opencode",
                    "agent_isolation": "separate-home-and-workspace; no OS sandbox",
                    "web_search": "disabled",
                    "history_persistence": "none",
                    "query_dataset_repo": self.config.dataset.queries_repo,
                    "query_dataset_revision": self.config.dataset.queries_revision,
                    "query_dataset_split": self.config.dataset.queries_split,
                    "corpus_repo": self.config.dataset.corpus_repo,
                    "corpus_revision": self.config.dataset.corpus_revision,
                    "corpus_split": self.config.dataset.corpus_split,
                    "zg_version": "fixture candidate",
                    "embedding": self.config.zvec_grep.embedding,
                    "fts_tokenizer": "jieba",
                    "embedding_concurrency": self.config.zvec_grep.embedding_concurrency,
                    "embedding_device": self.config.zvec_grep.device,
                    "max_filesize": self.config.zvec_grep.max_filesize,
                    "mcp_transport": "http",
                    "mcp_tool_timeout_seconds": self.config.zvec_grep.mcp_tool_timeout_seconds,
                    "zg_server_url": "http://127.0.0.1:7999",
                },
                "runtime_setups": [
                    {
                        "total_wall_seconds": 10.0,
                        "server_start_wall_seconds": 1.0,
                        "profile_preparation_wall_seconds": 2.0,
                        "profile_install_wall_seconds": 3.0,
                        "warmup_wall_seconds": 4.0,
                    }
                ],
                "status": "completed",
            },
        )
        for trial in range(1, self.config.run.trials_per_case + 1):
            for profile in PROFILES:
                trial_root = (
                    run / "cases" / case / profile / "trials" / f"trial-{trial:03d}"
                )
                events = trial_root / "attempts/attempt-001/events.jsonl"
                events.parent.mkdir(parents=True)
                events.write_text(
                    json.dumps(
                        {
                            "type": "item.completed",
                            "item": {
                                "id": "tool-1",
                                "type": "command_execution",
                                "command": "cat document.md",
                                "aggregated_output": "Document contents",
                            },
                        }
                    )
                    + "\n"
                )
                result = {
                    "query_id": case,
                    "profile": profile,
                    "trial_index": trial,
                    "attempt": 1,
                    "status": "completed",
                    "wall_seconds": float(trial),
                    # Deliberately absent worker paths: reporting must use the
                    # relocated raw trace without rewriting this hashed result.
                    "paths": {
                        "events": f"/absent-worker/{case}/{profile}/{trial}/events.jsonl"
                    },
                    "trace": {
                        "final_response": "reference",
                        "usage": {
                            "input_tokens": trial * 100,
                            "output_tokens": trial * 10,
                        },
                        "tool_calls": [],
                        "observed_docids": [],
                    },
                }
                write_json(trial_root / "result.json", result)
                write_json(trial_root / "attempts/attempt-001/result.json", result)
                write_json(
                    run
                    / "evaluation/results"
                    / profile
                    / case
                    / f"trial-{trial:03d}.json",
                    {
                        "query_id": case,
                        "profile": profile,
                        "trial_index": trial,
                        "status": "completed",
                        "correct": True,
                        "candidate_sha256": hashlib.sha256(b"reference").hexdigest(),
                        "case_result_sha256": sha256_file(trial_root / "result.json"),
                        "judge_prompt_sha256": evaluate.JUDGE_PROMPT_SHA256,
                        "judge_schema_sha256": evaluate.JUDGE_SCHEMA_SHA256,
                        "evaluation_source_sha256": evaluate.EVALUATION_SOURCE_SHA256,
                        "usage": {"input_tokens": 10, "output_tokens": 1},
                        "wall_seconds": 0.5,
                    },
                )
            if self.plan_data["suite"] == "smoke":
                result = (
                    run
                    / "cases"
                    / case
                    / "zvec-grep/trials"
                    / f"trial-{trial:03d}/result.json"
                )
                write_json(
                    run
                    / "evaluation/usage-audit/results"
                    / case
                    / f"trial-{trial:03d}.json",
                    {
                        "query_id": case,
                        "trial_index": trial,
                        "status": "completed",
                        "correct_usage": True,
                        "used_zvec_grep": True,
                        "reasoning": "fixture audit",
                        "case_result_sha256": sha256_file(result),
                        "usage_audit_prompt_sha256": evaluate.USAGE_AUDIT_PROMPT_SHA256,
                        "usage_audit_schema_sha256": evaluate.USAGE_AUDIT_SCHEMA_SHA256,
                        "evaluation_source_sha256": evaluate.EVALUATION_SOURCE_SHA256,
                        "usage": {"input_tokens": 20, "output_tokens": 2},
                        "wall_seconds": 0.25,
                    },
                )
        self.assertTrue(_write_pair(run, case, self.config.run.trials_per_case))
        seal_evidence(evidence)
        return evidence, run

    def aggregate(self):
        # Mutated fixtures represent newly published raw evidence.
        for evidence in self.shards.iterdir():
            seal_evidence(evidence)
        return aggregate(self.output, self.plan_path, self.shards)

    @staticmethod
    def change(path, **values):
        contents = read_json(path)
        contents.update(values)
        write_json(path, contents)
        for parent in path.parents:
            if (parent / "evidence-manifest.json").is_file():
                seal_evidence(parent)
                break

    def test_ci_plan_preserves_global_balancing_and_case_selection(self):
        plan = self.plan()
        self.assertEqual(validate_plan(self.plan_path, self.artifacts), plan)
        self.assertEqual(plan["query_ids"], ["628", "700", "966", "827", "241"])
        selection = load_suite("ci", load_queries(self.queries))
        for case in plan["query_ids"]:
            selected = select_case(selection, case)
            self.assertEqual(selected.query_ids, (case,))
            self.assertEqual(selected.definition_sha256, selection.definition_sha256)
            self.assertEqual(selected.target_count, selection.target_count)
            self.assertEqual(selected.to_dict()["case_filter"], case)
            self.assertEqual(set(plan["profile_orders"][case]), {"1"})
        for trial in ("1",):
            baseline_first = sum(
                plan["profile_orders"][case][trial][0] == "baseline"
                for case in plan["query_ids"]
            )
            self.assertIn(baseline_first, (2, 3))
        with self.assertRaises(ValueError):
            select_case(selection, "770")

    def test_plan_rejects_changed_identity_query_set_and_trial_orders(self):
        original = self.plan()
        for changed in (
            {"candidate_commit": "other"},
            {"harness_commit": "other"},
            {"configuration_sha256": "other"},
            {"model_base_url": "https://different.example.test/v1"},
            {"query_set_sha256": "other"},
            {"query_ids": ["628"]},
            {
                "profile_orders": {
                    case: {"2": ["baseline", "zvec-grep"]}
                    for case in original["query_ids"]
                }
            },
        ):
            with self.subTest(changed=changed):
                write_json(self.plan_path, {**original, **changed})
                with self.assertRaises(ValueError):
                    validate_plan(self.plan_path, self.artifacts)

    def test_all_ci_raw_trials_aggregate_without_models_or_worker_paths(self):
        plan = self.plan()
        for case in plan["query_ids"]:
            self.shard(case)
        report = self.aggregate()
        summary = read_json(report / "summary.json")
        metadata = read_json(report.parent / "run.json")
        evaluation = read_json(report.parent / "evaluation/summary.json")
        self.assertTrue(read_json(report / "ci-completeness.json")["complete"])
        self.assertEqual(metadata["status"], "completed")
        self.assertTrue(metadata["aggregate_only"])
        self.assertEqual(metadata["execution"], "parallel_cases")
        self.assertNotEqual(metadata["created_at"], "2000-01-01T00:00:00Z")
        self.assertNotEqual(metadata["finished_at"], "2000-01-01T00:01:00Z")
        self.assertEqual(metadata["profile_orders"], plan["profile_orders"])
        self.assertEqual(
            summary["trials"],
            {"per_case": 1, "planned": 5, "persisted": 5, "completed": 5},
        )
        self.assertEqual(summary["cases"]["completed"], 5)
        self.assertEqual(
            summary["profiles"]["baseline"]["tokens"]["input"]["total"], 500
        )
        self.assertEqual(
            summary["profiles"]["baseline"]["tokens"]["input"]["mean"], 100
        )
        self.assertEqual(
            summary["tool_interaction_batches"]["baseline"]["batches"]["total"], 5
        )
        self.assertEqual(summary["runtime_preparation"]["sessions"], 5)
        self.assertEqual(evaluation["expected_answers"], 10)
        self.assertEqual(evaluation["evaluated_answers"], 10)
        self.assertEqual(evaluation["cost"]["total"]["input_tokens"], 100)
        self.assertEqual(evaluation["zvec_grep_usage"]["expected_trials"], 0)

    def test_smoke_keeps_single_trial_audit_and_original_result_hash(self):
        self.plan("smoke")
        _, worker = self.shard("770")
        original_hashes = {
            trial: sha256_file(
                worker / "cases/770/zvec-grep/trials" / f"trial-{trial:03d}/result.json"
            )
            for trial in (1,)
        }
        report = self.aggregate()
        self.assertTrue(read_json(report / "ci-completeness.json")["complete"])
        evaluation = read_json(report.parent / "evaluation/summary.json")
        self.assertEqual(
            evaluation["zvec_grep_usage"],
            {"expected_trials": 1, "evaluated_trials": 1, "correct_trials": 1},
        )
        self.assertEqual(evaluation["cost"]["total"]["completed_calls"], 3)
        self.assertEqual(
            read_json(report / "summary.json")["zvec_grep_usage_audit"]["status"],
            "scored",
        )
        for trial, digest in original_hashes.items():
            self.assertEqual(
                sha256_file(
                    report.parent
                    / "cases/770/zvec-grep/trials"
                    / f"trial-{trial:03d}/result.json"
                ),
                digest,
            )

    def test_missing_case_produces_partial_report(self):
        self.plan()
        self.shard("628")
        report = self.aggregate()
        completeness = read_json(report / "ci-completeness.json")
        self.assertFalse(completeness["complete"])
        self.assertIn("case 700: missing artifact", completeness["errors"])
        self.assertEqual(read_json(report.parent / "run.json")["status"], "partial")

    def test_mismatched_identity_is_never_used_as_current_evidence(self):
        plan = self.plan()
        evidence, _ = self.shard("628")
        for changed in (
            {"candidate_commit": "other"},
            {"harness_commit": "other"},
            {"plan_sha256": "other"},
        ):
            original = read_json(evidence / "ci.json")
            with self.subTest(changed=changed):
                self.change(evidence / "ci.json", **changed)
                self.assertEqual(select_shards(self.shards, self.plan_path, plan), [])
                write_json(evidence / "ci.json", original)

    def test_rerun_selects_newest_attempt_and_reuses_other_cases(self):
        plan = self.plan()
        old, _ = self.shard("628")
        new, _ = self.shard("628", attempt=2)
        other, _ = self.shard("700")
        selected = select_shards(self.shards, self.plan_path, plan)
        self.assertCountEqual(selected, [new, other])
        self.assertNotIn(old, selected)

    def test_raw_integrity_is_verified_before_aggregation(self):
        plan = self.plan()
        _, run = self.shard("628")
        (run / "run.json").write_text("{}")
        with self.assertRaisesRegex(ValueError, "integrity"):
            select_shards(self.shards, self.plan_path, plan)

    def test_duplicate_case_attempt_is_rejected(self):
        plan = self.plan()
        evidence, _ = self.shard("628")
        shutil.copytree(evidence, self.shards / "duplicate")
        with self.assertRaisesRegex(ValueError, "duplicate"):
            select_shards(self.shards, self.plan_path, plan)

    def test_new_failed_attempt_cannot_fall_back_to_old_success(self):
        self.plan("smoke")
        self.shard("770")
        _, latest = self.shard("770", attempt=2)
        self.change(
            latest / "run.json", status="failed", error={"message": "worker failed"}
        )
        report = self.aggregate()
        self.assertFalse(read_json(report / "ci-completeness.json")["complete"])
        source = read_json(report.parent / "shards/770.json")
        self.assertEqual(source["status"], "failed")
        self.assertEqual(source["error"], {"message": "worker failed"})
        self.assertNotIn("error", read_json(report.parent / "run.json"))

    def test_case_result_identity_cannot_be_relabelled_during_aggregation(self):
        self.plan("smoke")
        _, run = self.shard("770")
        self.change(
            run / "cases/770/baseline/trials/trial-001/result.json",
            query_id="628",
        )
        with self.assertRaisesRegex(RuntimeError, "trial identity mismatch"):
            self.aggregate()

    def test_unknown_case_is_rejected(self):
        plan = self.plan()
        evidence, _ = self.shard("628")
        self.change(evidence / "ci.json", case_id="770")
        with self.assertRaisesRegex(ValueError, "unexpected CI case"):
            select_shards(self.shards, self.plan_path, plan)

    def test_other_workflow_run_cannot_supply_case_results(self):
        plan = self.plan()
        evidence, _ = self.shard("628")
        self.change(evidence / "ci.json", workflow_run_id="workflow-2")
        with self.assertRaisesRegex(ValueError, "different workflow run"):
            select_shards(self.shards, self.plan_path, plan)

    def test_different_worker_binary_or_architecture_is_rejected(self):
        self.plan()
        self.shard("628")
        _, run = self.shard("700")
        original = read_json(run / "run.json")["environment"]
        for key in ("opencode_version", "zg_version", "machine"):
            with self.subTest(key=key):
                self.change(
                    run / "run.json", environment={**original, key: "different"}
                )
                with self.assertRaisesRegex(ValueError, "versions or architectures"):
                    self.aggregate()
                shutil.rmtree(self.output)

    def test_changed_trial_order_is_rejected(self):
        self.plan("smoke")
        _, run = self.shard("770")
        orders = read_json(run / "run.json")["profile_orders"]
        orders["770"]["1"].reverse()
        self.change(run / "run.json", profile_orders=orders)
        with self.assertRaisesRegex(ValueError, "protocol"):
            self.aggregate()

    def test_changed_prepared_index_is_rejected(self):
        self.plan()
        self.shard("628")
        _, run = self.shard("700")
        self.change(run / "run.json", index_fingerprint="other-index")
        with self.assertRaisesRegex(ValueError, "prepared corpora or indexes"):
            self.aggregate()

    def test_missing_raw_trial_is_not_hidden_by_completed_worker_status(self):
        self.plan("smoke")
        _, run = self.shard("770")
        (run / "cases/770/baseline/trials/trial-001/result.json").unlink()
        report = self.aggregate()
        self.assertFalse(read_json(report / "ci-completeness.json")["complete"])
        self.assertEqual(read_json(report / "summary.json")["trials"]["completed"], 0)

    def test_stale_judgement_is_not_counted(self):
        self.plan("smoke")
        _, run = self.shard("770")
        self.change(
            run / "evaluation/results/baseline/770/trial-001.json",
            candidate_sha256="stale",
        )
        report = self.aggregate()
        self.assertFalse(read_json(report / "ci-completeness.json")["complete"])
        self.assertEqual(
            read_json(report.parent / "evaluation/summary.json")["evaluated_answers"], 1
        )

    def test_stale_smoke_usage_audit_is_not_counted(self):
        self.plan("smoke")
        _, run = self.shard("770")
        self.change(
            run / "evaluation/usage-audit/results/770/trial-001.json",
            case_result_sha256="stale",
        )
        report = self.aggregate()
        self.assertFalse(read_json(report / "ci-completeness.json")["complete"])
        self.assertEqual(
            read_json(report.parent / "evaluation/summary.json")["zvec_grep_usage"][
                "evaluated_trials"
            ],
            0,
        )


class RestoreQueriesTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.artifacts = self.root / "prepared"
        self.plan_path = self.root / "old-plan/plan.json"
        self.source = self.plan_path.parent / "source/browsecomp_plus_decrypted.jsonl"
        self.source.parent.mkdir(parents=True)
        self.rows = [
            {
                "query_id": case,
                "query": f"Fixture question {case}: 测试",
                "answer": "reference",
                "evidence_docs": [{"docid": "evidence-id"}],
                "gold_docs": [{"docid": "gold-id"}],
            }
            for case in [
                "628",
                "700",
                "966",
                "827",
                "241",
                "770",
                *(str(number) for number in range(10000, 10824)),
            ]
        ]
        self.assertEqual(len(self.rows), load_config().dataset.expected_queries)
        self.plan = {
            "candidate_commit": "candidate-sha",
            "harness_commit": "old-harness",
            "configuration_sha256": "old-configuration",
            "suite": "old-suite",
            "profile_orders": {"old-orders": "must not be reused"},
        }
        self.write_source()
        environment = patch.dict(
            os.environ,
            BROWSE_CANDIDATE_COMMIT="candidate-sha",
            BROWSE_HARNESS_COMMIT="current-harness",
            BROWSE_COMP_MODEL_BASE_URL="https://qwen.example.test/v1",
        )
        environment.start()
        self.addCleanup(environment.stop)
        for name in ("subprocess.Popen", "urllib.request.urlopen"):
            no_external_calls = patch(
                name, side_effect=AssertionError("unexpected external call")
            )
            no_external_calls.start()
            self.addCleanup(no_external_calls.stop)

    def write_source(self, payload=None):
        if payload is None:
            payload = "".join(
                json.dumps(row, ensure_ascii=False) + "\n" for row in self.rows
            ).encode("utf-8")
        self.source.write_bytes(payload)
        self.plan["query_set_sha256"] = sha256_file(self.source)
        write_json(self.plan_path, self.plan)

    def assert_no_query_published(self):
        source = self.artifacts / "source"
        self.assertEqual(list(source.iterdir()) if source.exists() else [], [])

    def test_all_queries_are_copied_byte_for_byte_without_old_plan_metadata(self):
        restored = restore_queries(self.artifacts, self.plan_path)
        self.assertEqual(restored.read_bytes(), self.source.read_bytes())
        self.assertEqual(len(load_queries(restored)), 830)
        self.assertEqual(load_queries(restored), load_queries(self.source))
        self.assertFalse((self.artifacts / "plan.json").exists())
        self.assertFalse((self.artifacts / "workspaces").exists())

    def test_new_plan_uses_current_harness_configuration_suite_and_fresh_orders(self):
        restore_queries(self.artifacts, self.plan_path)
        fresh_orders = {"770": {"1": ["baseline", "zvec-grep"]}}
        with (
            patch(
                "zg_bench.ci_parallel._randomized_profile_orders",
                return_value=fresh_orders,
            ) as orders,
            contextlib.redirect_stdout(io.StringIO()),
        ):
            plan_path = create_plan(self.artifacts, "smoke", self.root / "new-plan")
        plan = read_json(plan_path)
        orders.assert_called_once_with(["770"], load_config().run.trials_per_case)
        self.assertEqual(plan["profile_orders"], fresh_orders)
        self.assertEqual(plan["harness_commit"], "current-harness")
        self.assertEqual(plan["configuration_sha256"], sha256_file(load_config().path))
        self.assertEqual(plan["suite"], "smoke")
        self.assertEqual(plan["candidate_commit"], "candidate-sha")
        self.assertEqual(plan["query_set_sha256"], self.plan["query_set_sha256"])

    def test_candidate_mismatch_fails_without_publishing(self):
        self.plan["candidate_commit"] = "different-candidate"
        write_json(self.plan_path, self.plan)
        with self.assertRaisesRegex(ValueError, "candidate identity"):
            restore_queries(self.artifacts, self.plan_path)
        self.assert_no_query_published()

    def test_checksum_mismatch_fails_without_publishing(self):
        self.source.write_bytes(self.source.read_bytes() + b"\n")
        with self.assertRaisesRegex(ValueError, "checksum"):
            restore_queries(self.artifacts, self.plan_path)
        self.assert_no_query_published()

    def test_missing_plan_and_query_files_fail(self):
        for path in (self.plan_path, self.source):
            with self.subTest(path=path.name):
                contents = path.read_bytes()
                path.unlink()
                with self.assertRaisesRegex(ValueError, "regular files"):
                    restore_queries(self.artifacts, self.plan_path)
                self.assert_no_query_published()
                path.write_bytes(contents)

    def test_symlinked_plan_query_or_source_directory_fails(self):
        for path in (
            self.plan_path,
            self.source,
            self.source.parent,
            self.plan_path.parent,
        ):
            with self.subTest(path=path.name):
                original = path.with_name(path.name + "-original")
                path.rename(original)
                path.symlink_to(original, target_is_directory=original.is_dir())
                with self.assertRaisesRegex(ValueError, "regular files"):
                    restore_queries(self.artifacts, self.plan_path)
                self.assert_no_query_published()
                path.unlink()
                original.rename(path)

    def test_malformed_plan_json_or_object_fails(self):
        for payload in ("not JSON", "[]"):
            with self.subTest(payload=payload):
                self.plan_path.write_text(payload, encoding="utf-8")
                with self.assertRaises(ValueError):
                    restore_queries(self.artifacts, self.plan_path)
                self.assert_no_query_published()

    def test_valid_checksum_does_not_accept_invalid_query_json_or_rows(self):
        for payload in (
            b"not JSON\n",
            b'"not a row"\n',
            b'{"query_id":"x"}\n',
            b"\xff\n",
        ):
            with self.subTest(payload=payload):
                self.write_source(payload)
                with self.assertRaisesRegex(ValueError, "query set is invalid"):
                    restore_queries(self.artifacts, self.plan_path)
                self.assert_no_query_published()

    def test_wrong_count_fails_without_publishing_or_partial_files(self):
        self.rows.pop()
        self.write_source()
        with self.assertRaisesRegex(ValueError, "wrong query count"):
            restore_queries(self.artifacts, self.plan_path)
        self.assert_no_query_published()

    def test_duplicate_ids_in_full_query_set_fail(self):
        self.rows[-1] = self.rows[0]
        self.write_source()
        with self.assertRaisesRegex(ValueError, "query set is invalid"):
            restore_queries(self.artifacts, self.plan_path)
        self.assert_no_query_published()

    def test_unsafe_query_ids_fail(self):
        self.rows[-1]["query_id"] = "../escape"
        self.write_source()
        with self.assertRaisesRegex(ValueError, "query set is invalid"):
            restore_queries(self.artifacts, self.plan_path)
        self.assert_no_query_published()

    def test_existing_or_symlinked_destination_is_preserved(self):
        destination = self.artifacts / "source" / self.source.name
        destination.parent.mkdir(parents=True)
        destination.write_bytes(b"existing")
        with self.assertRaisesRegex(ValueError, "unused regular destination"):
            restore_queries(self.artifacts, self.plan_path)
        self.assertEqual(destination.read_bytes(), b"existing")
        destination.unlink()
        destination.symlink_to(self.source)
        before = self.source.read_bytes()
        with self.assertRaisesRegex(ValueError, "unused regular destination"):
            restore_queries(self.artifacts, self.plan_path)
        self.assertEqual(self.source.read_bytes(), before)
        self.assertTrue(destination.is_symlink())

    def test_cli_uses_default_artifacts_and_only_restores_queries(self):
        with (
            patch(
                "sys.argv",
                ["ci_parallel", "restore-queries", "--plan", str(self.plan_path)],
            ),
            patch("zg_bench.ci_parallel.DEFAULT_ARTIFACTS_DIR", self.artifacts),
        ):
            self.assertEqual(main(), 0)
        self.assertEqual(
            (self.artifacts / "source" / self.source.name).read_bytes(),
            self.source.read_bytes(),
        )


if __name__ == "__main__":
    unittest.main()
