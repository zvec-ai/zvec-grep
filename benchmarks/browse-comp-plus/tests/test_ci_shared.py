from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from contextlib import ExitStack
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from zg_bench.artifacts import read_json, sha256_file, write_json
from zg_bench.ci_parallel import aggregate
from zg_bench.ci_resume import (
    evidence_roots,
    restore_case,
    seal_evidence,
    verify_evidence,
)
from zg_bench.ci_shared import group_previous, run_shared, validate_runtime
from zg_bench.config import load_config
from zg_bench.models import AttemptResult, TraceSummary
from zg_bench.runner import RunTerminated, _needs_run, run_benchmark


class SharedProcessTests(unittest.TestCase):
    """Real child processes exercise scheduling, failure isolation, and cancellation."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.artifacts = self.root / "artifacts"
        self.plan_path = self.root / "plan.json"
        self.plan = {
            "query_ids": ["628", "700", "966", "827", "241"],
            "execution": "shared_job",
        }
        write_json(self.plan_path, self.plan)
        self.manifest = self.root / "candidate.json"
        write_json(self.manifest, {"commit": "candidate"})
        self.fixture = self.root / "worker.py"
        self.fixture.write_text("""
import json, os, sys, time
from pathlib import Path
from zg_bench import ci_shared
from zg_bench.artifacts import write_json

ci_shared.validate_plan = lambda path, artifacts: json.loads(path.read_text())
def auth(home):
    home.mkdir(parents=True)
    write_json(home / 'auth.json', {'token': 'fixture-private-credential'})
def run_case(artifacts, plan, case, previous, *, shared_runtime):
    runtime = json.loads((artifacts / 'state/runtime.json').read_text())
    assert runtime == shared_runtime
    root = artifacts / 'runs' / ('case-' + case)
    write_json(root / 'run.json', {'query_ids': [case], 'run_id': root.name})
    write_json(artifacts / 'observed' / (case + '.json'), {
        'case': case, 'start': time.time(), 'pid': os.getpid(),
        'home': os.environ['OPENCODE_HOME'], 'tmp': os.environ['TMPDIR'],
        'shared_runtime_id': shared_runtime['shared_runtime_id'],
    })
    time.sleep(float(os.environ.get('FIXTURE_WORK_SECONDS', '0.2')))
    data = json.loads((artifacts / 'observed' / (case + '.json')).read_text())
    data['end'] = time.time()
    write_json(artifacts / 'observed' / (case + '.json'), data)
    return 1 if case == '700' else 0
def judge(artifacts, root, **kwargs):
    write_json(root / 'judged.json', {'done': True})
ci_shared.configure_auth = auth
ci_shared.run_case = run_case
ci_shared.evaluate_run = judge
ci_shared.evaluation_complete = lambda root: True
raise SystemExit(ci_shared.main())
""")
        environment = patch.dict(
            os.environ,
            {
                "GITHUB_RUN_ID": "workflow",
                "GITHUB_RUN_ATTEMPT": "1",
                "BROWSE_COMP_MODEL_BASE_URL": "https://qwen.example.test/v1",
                "PYTHONPATH": str(Path(__file__).resolve().parents[1]),
            },
        )
        environment.start()
        self.addCleanup(environment.stop)
        self.children = []
        original = subprocess.Popen

        def spawn(command, **kwargs):
            process = original(
                [sys.executable, str(self.fixture), *command[3:]], **kwargs
            )
            self.children.append(process)
            return process

        for target, options in (
            ("validate_plan", {"return_value": self.plan}),
            ("index_is_ready", {"return_value": True}),
            ("ensure_server", {}),
            (
                "prepare_search_runtime",
                {
                    "return_value": {
                        "root": str((self.artifacts / "workspaces/b").resolve()),
                        "server_url": "http://127.0.0.1:7999/mcp",
                        "warmup_wall_seconds": 0.1,
                    }
                },
            ),
            ("stop_server", {}),
            ("subprocess.Popen", {"side_effect": spawn}),
        ):
            mocked = patch("zg_bench.ci_shared." + target, **options)
            setattr(self, target.split(".")[-1], mocked.start())
            self.addCleanup(mocked.stop)

    def run_pool(self, parallel=2):
        return run_shared(
            self.artifacts,
            self.plan_path,
            parallel,
            self.root / "previous",
            self.root / "evidence",
            self.root / "reports",
            self.manifest,
        )

    def test_real_workers_share_one_daemon_but_keep_homes_results_and_failures_separate(
        self,
    ):
        self.assertEqual(self.run_pool(), 1)
        self.ensure_server.assert_called_once()
        self.prepare_search_runtime.assert_called_once()
        self.index_is_ready.assert_called_once()
        self.stop_server.assert_called_once()
        observations = [
            read_json(path) for path in (self.artifacts / "observed").glob("*.json")
        ]
        self.assertEqual(len(observations), 5)
        self.assertEqual(len({row["home"] for row in observations}), 5)
        self.assertEqual(len({row["tmp"] for row in observations}), 5)
        self.assertEqual(
            {row["shared_runtime_id"] for row in observations}, {"workflow-1"}
        )
        events = sorted(
            [(row["start"], 1) for row in observations]
            + [(row["end"], -1) for row in observations]
        )
        active = peak = 0
        for _, delta in events:
            active += delta
            peak = max(active, peak)
        self.assertEqual(peak, 2)
        for case in self.plan["query_ids"]:
            root = self.root / "evidence" / case
            verify_evidence(root)
            self.assertEqual(read_json(root / "ci.json")["case_id"], case)
            self.assertEqual(
                [path.name for path in (root / "runs").iterdir()], ["case-" + case]
            )
            self.assertFalse(list(root.rglob("auth.json")))
            self.assertFalse((self.artifacts / "runtime/case-homes" / case).exists())
        outcomes = read_json(self.artifacts / "state/shared-execution.json")["cases"]
        self.assertEqual(outcomes["700"], 1)
        self.assertEqual(sum(value == 0 for value in outcomes.values()), 4)

    def test_cancel_stops_every_child_before_the_shared_daemon_and_does_not_judge(self):
        real_sleep = __import__("time").sleep

        def interrupt(_):
            deadline = __import__("time").monotonic() + 10
            while len(list((self.artifacts / "observed").glob("*.json"))) != 2:
                if __import__("time").monotonic() > deadline:
                    self.fail("workers did not start")
                real_sleep(0.02)
            raise RunTerminated(15)

        def stopped(*_):
            self.assertTrue(all(child.poll() is not None for child in self.children))

        self.stop_server.side_effect = stopped
        with (
            patch.dict(os.environ, FIXTURE_WORK_SECONDS="60"),
            patch("zg_bench.ci_shared.sleep", side_effect=interrupt),
            self.assertRaises(RunTerminated),
        ):
            self.run_pool()
        self.assertEqual(len(self.children), 2)
        for case in ("628", "700"):
            verify_evidence(self.root / "evidence" / case)
            self.assertFalse(
                (self.artifacts / "runs" / ("case-" + case) / "judged.json").exists()
            )
        self.stop_server.assert_called_once()

    def test_invalid_index_does_not_launch_workers_or_rebuild(self):
        self.index_is_ready.return_value = False
        with self.assertRaisesRegex(ValueError, "never rebuild"):
            self.run_pool()
        self.Popen.assert_not_called()
        self.ensure_server.assert_not_called()

    def test_parent_preserves_raw_evidence_after_a_worker_crashes_before_export(self):
        script = self.fixture.read_text().replace(
            "time.sleep(float(os.environ.get('FIXTURE_WORK_SECONDS', '0.2')))",
            "\n    if case == '700':\n        os._exit(17)\n    time.sleep(0.1)",
        )
        self.fixture.write_text(script)
        self.assertEqual(self.run_pool(), 1)
        evidence = self.root / "evidence/700"
        verify_evidence(evidence)
        self.assertEqual(read_json(evidence / "ci.json")["case_id"], "700")
        self.assertTrue((evidence / "runs/case-700/run.json").is_file())
        self.assertEqual(
            read_json(self.artifacts / "state/shared-execution.json")["cases"]["700"],
            17,
        )
        self.assertEqual(len(evidence_roots(self.root / "evidence")), 5)

    def test_invalid_runtime_owner_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "identity changed"):
            validate_runtime(self.artifacts, {"shared_runtime_id": "another-run"})


class SharedBundleTests(unittest.TestCase):
    def setUp(self):
        # Reuse the raw answer/judge fixture, without inheriting its test methods.
        import test_ci_parallel

        self.fixture = test_ci_parallel.ParallelCIIntegrationTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        f = self.fixture
        f.plan("iteration")
        f.plan_data["execution"] = "shared_job"
        write_json(f.plan_path, f.plan_data)
        self.bundles = f.root / "downloaded-bundles"
        for case in f.plan_data["query_ids"]:
            evidence, run = f.shard(case)
            runtime = read_json(run / "run.json")["runtime_setups"][0]
            runtime["shared_runtime_id"] = "workflow-1-1"
            f.change(run / "run.json", execution="shared_job", runtime_setups=[runtime])
            seal_evidence(evidence)
            shutil.copytree(evidence, self.bundles / "bundle-attempt-1" / case)

    def test_nested_bundles_aggregate_without_models_and_count_shared_warmup_once(self):
        f = self.fixture
        report = aggregate(f.output, f.plan_path, self.bundles)
        summary = read_json(report / "summary.json")
        self.assertTrue(summary["ci"]["complete"])
        self.assertEqual(summary["ci"]["execution"], "shared_job")
        self.assertEqual(summary["runtime_preparation"]["total_wall_seconds"], 10.0)
        self.assertEqual(
            read_json(report.parent / "evaluation/summary.json")["evaluated_answers"], 4
        )

    def test_flat_bundle_groups_each_case_for_strict_same_plan_resume(self):
        f = self.fixture
        with patch.dict(os.environ, GITHUB_RUN_ATTEMPT="2"):
            grouped = f.root / "grouped"
            group_previous(
                self.bundles / "bundle-attempt-1", grouped, f.plan_path, f.plan_data
            )
        for case in ("628", "241"):
            roots = evidence_roots(grouped / case)
            self.assertEqual(len(roots), 1)
            verify_evidence(roots[0])
            self.assertEqual(read_json(roots[0] / "ci.json")["case_id"], case)

    def test_corrupt_bundle_is_rejected_before_restoring_any_trial(self):
        f = self.fixture
        evidence = self.bundles / "bundle-attempt-1/628"
        (evidence / "ci.json").write_text("{}")
        with (
            patch.dict(os.environ, GITHUB_RUN_ATTEMPT="2"),
            self.assertRaisesRegex(ValueError, "integrity"),
        ):
            group_previous(self.bundles, f.root / "grouped", f.plan_path, f.plan_data)

    def test_conflicting_shared_runtime_measurements_cannot_be_double_counted(self):
        f = self.fixture
        evidence = self.bundles / "bundle-attempt-1/241"
        run = next((evidence / "runs").iterdir())
        metadata = read_json(run / "run.json")
        metadata["runtime_setups"][0]["total_wall_seconds"] += 1
        write_json(run / "run.json", metadata)
        seal_evidence(evidence)
        with self.assertRaisesRegex(ValueError, "measurements changed"):
            aggregate(f.output, f.plan_path, self.bundles)


class SharedRunnerOwnershipTests(unittest.TestCase):
    def test_case_runner_never_starts_warms_or_stops_the_parent_daemon(self):
        with tempfile.TemporaryDirectory() as directory, ExitStack() as stack:
            stack.enter_context(
                patch.dict(
                    os.environ, BROWSE_COMP_MODEL_BASE_URL="https://model.test/v1"
                )
            )
            artifacts = Path(directory)
            query_path = artifacts / "source/browsecomp_plus_decrypted.jsonl"
            query_path.parent.mkdir()
            query_path.write_text(
                "".join(
                    json.dumps(
                        {"query_id": case, "query": "Question", "answer": "answer"}
                    )
                    + "\n"
                    for case in ("628", "241")
                )
            )
            write_json(artifacts / "state/corpus.json", {"fingerprint": "corpus"})
            write_json(
                artifacts / "state/index.json",
                {
                    "fingerprint": "index",
                    "index_bytes": 1,
                    "statistics": {},
                    "build_wall_seconds": 1,
                },
            )
            for name in (
                "prepared_dataset",
                "prepared_corpus",
                "prepared_index",
                "validate_model",
                "prepare_profiles",
            ):
                stack.enter_context(patch("zg_bench.runner." + name, return_value=True))
            stack.enter_context(
                patch(
                    "zg_bench.runner.resolve_executable",
                    return_value=Path(sys.executable),
                )
            )
            stack.enter_context(
                patch(
                    "zg_bench.runner.run_command",
                    return_value=SimpleNamespace(ok=True, stdout="1.18.4"),
                )
            )
            stack.enter_context(
                patch(
                    "zg_bench.runner.validate_profiles",
                    return_value={
                        "fingerprint": "profiles",
                        "preparation_wall_seconds": 0.1,
                        "install_wall_seconds": 0.01,
                    },
                )
            )
            stack.enter_context(patch("zg_bench.runner._run_protocol", return_value={}))
            stack.enter_context(patch("zg_bench.report.generate_report"))
            owner_calls = [
                stack.enter_context(patch("zg_bench.runner." + name))
                for name in ("ensure_server", "prepare_search_runtime", "stop_server")
            ]

            def answer(
                config, artifacts, root, query, profile, trial, *args, shared_runtime
            ):
                self.assertTrue(shared_runtime)
                result = AttemptResult(
                    query_id=query["query_id"],
                    profile=profile,
                    trial_index=trial,
                    status="completed",
                    attempt=1,
                    started_at="start",
                    finished_at="finish",
                    wall_seconds=1,
                    exit_code=0,
                    infrastructure_failure=False,
                    trace=TraceSummary(
                        "session", "answer", "answer", True, None, (), ()
                    ),
                )
                write_json(
                    root
                    / "cases"
                    / query["query_id"]
                    / profile
                    / "trials/trial-001/result.json",
                    result.to_dict(),
                )
                return result

            execute = stack.enter_context(
                patch("zg_bench.runner._run_profile", side_effect=answer)
            )
            root = run_benchmark(
                load_config(),
                artifacts,
                suite="iteration",
                case_id="628",
                shared_runtime={"shared_runtime_id": "workflow-1"},
            )
            self.assertEqual(execute.call_count, 2)
            self.assertEqual(read_json(root / "run.json")["status"], "completed")
            for call in owner_calls:
                call.assert_not_called()


class SharedResumeTests(unittest.TestCase):
    def test_bundle_resume_preserves_completed_wrong_answers_and_existing_judgement(
        self,
    ):
        import test_ci_resume

        f = test_ci_resume.RawCaseResumeTests()
        f.setUp()
        self.addCleanup(f.doCleanups)
        f.plan.update(query_ids=["827"], execution="shared_job")
        write_json(f.plan_path, f.plan)
        identity = read_json(f.evidence / "ci.json")
        identity["plan_sha256"] = sha256_file(f.plan_path)
        write_json(f.evidence / "ci.json", identity)
        shutil.copyfile(f.plan_path, f.evidence / "plan.json")
        f.result("zvec-grep", "completed", response="reference")
        seal_evidence(f.evidence)
        bundle = f.root / "bundle/attempt-1/827"
        shutil.copytree(f.evidence, bundle)
        grouped = f.root / "grouped"
        group_previous(f.root / "bundle", grouped, f.plan_path, f.plan)
        run_id = restore_case(
            f.artifacts, grouped / "827", f.plan_path, f.plan, "827", f.config
        )
        restored = f.artifacts / "runs" / run_id
        for profile in ("baseline", "zvec-grep"):
            selected = f.trial(restored, profile) / "result.json"
            self.assertFalse(_needs_run(f.config, selected))
            self.assertEqual(
                selected.read_bytes(),
                (f.trial(f.run, profile) / "result.json").read_bytes(),
            )
            self.assertEqual(len(list((selected.parent / "attempts").iterdir())), 1)
        judgement = Path("evaluation/results/baseline/827/trial-001.json")
        self.assertEqual(
            (restored / judgement).read_bytes(), (f.run / judgement).read_bytes()
        )
        self.assertFalse(read_json(restored / judgement)["correct"])
        self.assertFalse((restored / "profiles").exists())


if __name__ == "__main__":
    unittest.main()
