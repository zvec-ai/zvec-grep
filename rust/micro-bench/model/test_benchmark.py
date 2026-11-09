"""Core measurement math, failure accounting and resume checks; no model inference."""
import copy
import errno
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import report
import suite
from evidence import publish_round, save
from execution import COUNTERS, validate_controls
from measurement import case_identity, case_path, checked_accounting, fingerprint, resource_summary, validate_accounting
from run import clock
from verify import check_table, verify


def execution_fixture(provider="cpu"):
    counters = COUNTERS + ("queue_wait_ns",)
    before = dict.fromkeys(counters, 10)
    before.update(actual_provider=provider,session_count=1,cpu_threads=2,
                  max_native_batch_rows=4,max_padded_tokens=128,native_inflight_peak=1)
    measured = dict(before, **{name:20 for name in counters})
    measured.update(native_runs=5,native_rows=20)
    after = dict(before, **{name:before[name]+measured[name] for name in counters})
    return dict(before=before,after=after,measured=measured,
                counter_scope="measured_embedding_phase",peak_scope="runtime_including_warmup")


class SamplingTests(unittest.TestCase):
    def test_worker_clock_has_same_epoch(self):
        before = clock()
        child = float(subprocess.check_output(
            [sys.executable, "-c", "from run import clock; print(clock())"],
            cwd=Path(__file__).parent, text=True))
        self.assertLessEqual(before, child)
        self.assertLessEqual(child, clock())

    def test_cpu_mean_and_fixed_one_second_peak(self):
        samples = [dict(phase="embedding",at_seconds=i/50,cpu_seconds=6*i/50,
                        rss_bytes=100,physical_footprint_bytes=90) for i in range(101)]
        result = resource_summary(samples,12)["embedding"]
        self.assertAlmostEqual(result["mean_cpu_percent_one_core"],600)
        self.assertAlmostEqual(result["peak_cpu_percent_one_core"],600)
        self.assertEqual(result["cpu_valid_windows"],2)

    def test_cpu_peak_is_unavailable_for_short_gapped_or_impossible_samples(self):
        for times,cores in (([0,0.2],1),([0,2],1),([i/50 for i in range(101)],13)):
            with self.subTest(times=times,cores=cores):
                samples = [dict(phase="embedding",at_seconds=t,cpu_seconds=t*cores,
                                rss_bytes=100,physical_footprint_bytes=90) for t in times]
                result = resource_summary(samples,12)["embedding"]
                self.assertIsNone(result["peak_cpu_percent_one_core"])


class AccountingTests(unittest.TestCase):
    def setUp(self):
        self.rows = {"embedding":[dict(id=i,scheduled_ns=100+i*100,issued_ns=100+i*100,
                     finished_ns=200+i*100,status="completed",vectors=4,input_indices=[0,1,0,1])
                     for i in range(100)]}
        self.elapsed = 10000/1e9
        self.result = dict(schema_version=5,input_tokens=32,batch_size=4,protocol_pass=True,
            provenance=dict(inputs=[dict(id=i) for i in range(2)]),
            embedding=dict(started_ns=100,finished_ns=10100,elapsed_seconds=self.elapsed,
                scheduled=100,issued=100,completed=100,timeout=0,errors=0,
                vectors_per_second=400/self.elapsed,nominal_tokens_per_second=12800/self.elapsed,
                call_latency=dict(n=100,p50_ms=0.0001,p95_ms=0.0001,p99_ms=None)))

    def validate(self):
        return validate_accounting(self.result,self.rows,32,4)

    def test_rates_count_returned_vectors_without_validating_model_output(self):
        self.assertEqual(self.validate(),[])
        self.rows["embedding"][0]["vectors"] = 0
        self.result["embedding"]["vectors_per_second"] = 396/self.elapsed
        self.assertEqual(self.validate(),[])
        self.result["embedding"]["nominal_tokens_per_second"] *= 4
        self.assertIn("embedding: incorrect elapsed time or throughput denominator",self.validate())

    def test_batch_percentiles_are_not_divided_and_require_enough_samples(self):
        self.result["embedding"]["call_latency"].update(p95_ms=0.0001/4,p99_ms=0.0001)
        errors = self.validate()
        self.assertIn("embedding: incorrect p95 latency or sample threshold",errors)
        self.assertIn("embedding: incorrect p99 latency or sample threshold",errors)

    def test_failures_and_timeouts_are_counted_without_hiding_them(self):
        self.rows["embedding"][0]["status"] = "error"
        self.rows["embedding"][1]["status"] = "timeout"
        self.result["protocol_pass"] = False
        self.result["embedding"].update(completed=98,errors=1,timeout=1,
            vectors_per_second=392/self.elapsed,nominal_tokens_per_second=12544/self.elapsed,
            call_latency=dict(n=98,p50_ms=0.0001,p95_ms=None,p99_ms=None))
        self.assertEqual(self.validate(),[])
        self.result["protocol_pass"] = True
        self.assertIn("embedding: failed calls marked as a successful round",self.validate())
        self.result["embedding"]["errors"] = 0
        self.assertIn("embedding: summary/raw accounting mismatch",self.validate())

    def test_duplicate_missing_and_invalid_timing_records_are_rejected(self):
        self.rows["embedding"][1]["id"] = 0
        self.assertIn("embedding: omitted or duplicated requests",self.validate())
        self.rows["embedding"].pop()
        self.assertIn("embedding: summary/raw accounting mismatch",self.validate())
        self.rows["embedding"][0]["finished_ns"] = 0
        self.assertIn("embedding: invalid timestamp order",self.validate())

    def test_missing_and_malformed_evidence_remain_explicit(self):
        self.assertEqual(checked_accounting(None,{},32,4),["missing benchmark result"])
        self.rows["embedding"][0].pop("finished_ns")
        self.assertIn("malformed benchmark evidence",checked_accounting(self.result,self.rows,32,4)[0])

    def test_execution_deltas_and_actual_admission_are_verified(self):
        self.result.update(effective_execution_concurrency=1,admission_concurrency=2,index_inflight_batches=2)
        self.result["embedding"]["execution"] = execution_fixture()
        self.assertEqual(self.validate(),[])
        # A lazily initialized second CUDA worker is a gauge change, not a
        # counter reset or an invalid measurement.
        for snapshot in ("after","measured"):
            self.result["embedding"]["execution"][snapshot]["session_count"] = 2
        self.assertEqual(self.validate(),[])
        self.result["embedding"]["execution"]["measured"]["native_rows"] += 1
        self.assertIn("execution: incorrect measured delta for native_rows",self.validate())
        self.result["index_inflight_batches"] = 8
        self.assertIn("execution: benchmark workers differ from runtime admission",self.validate())

    def test_fixed_batch_profile_rejects_hidden_coalescing(self):
        self.result.update(fixed_batch=True,effective_execution_concurrency=1,
                           admission_concurrency=2,index_inflight_batches=2)
        execution = execution_fixture("cuda")
        self.result["embedding"]["execution"] = execution
        self.assertEqual(self.validate(),[])
        for snapshot in ("before","after","measured"):
            execution[snapshot]["max_native_batch_rows"] = 8
        self.assertIn("execution: fixed-batch profile merged request batches",self.validate())

    def test_named_timing_requires_scope_and_keeps_historical_reports_readable(self):
        self.result.update(effective_execution_concurrency=1, admission_concurrency=2, index_inflight_batches=2)
        execution = execution_fixture()
        self.result["embedding"]["execution"] = execution
        self.assertEqual(self.validate(), [])
        for name in ("before", "after", "measured"):
            snapshot = execution[name]
            snapshot["backend_to_native_ns"] = snapshot.pop("queue_wait_ns")
            snapshot["pool_reconfigurations"] = {"before":1,"after":3,"measured":2}[name]
        self.assertIn("execution: missing backend-to-native timing scope", self.validate())
        execution["timing_scope"] = {"backend_to_native_ns":"includes preparation; excludes admission"}
        self.assertEqual(self.validate(), [])
        execution["measured"]["pool_reconfigurations"] = 3
        self.assertIn("execution: incorrect measured delta for pool_reconfigurations", self.validate())

    def test_fixed_pool_and_clients_are_verified_independently_of_admission(self):
        self.result.update(effective_execution_concurrency=1, admission_concurrency=3,
                           index_inflight_batches=8, clients=8, client_mode="fixed", ort_sessions=1)
        execution = execution_fixture()
        for snapshot in execution.values():
            if isinstance(snapshot, dict):
                snapshot["cpu_threads_total"] = 2
        self.result["embedding"]["execution"] = execution
        self.assertEqual(self.validate(), [])
        self.assertEqual(validate_controls(self.result, 8, 1), [])
        self.assertTrue(validate_controls(self.result, 4, 1))
        self.assertTrue(validate_controls(self.result, None, 1))
        self.result["ort_sessions"] = 2
        self.assertIn("execution: actual session count differs from configured fixed pool", self.validate())
        self.assertTrue(validate_controls(self.result, 8, 1))
        self.result["ort_sessions"] = 1
        execution["measured"]["cpu_threads_total"] = 3
        self.assertIn("execution: total native threads differ from session count times threads per session", self.validate())

    def test_global_threads_and_server_capacity_are_verified(self):
        self.result.update(runtime_mode="server",server_concurrency=4,hardware_threads=8,
                           ort_thread_budget=4,resolved_index_concurrency=8,
                           effective_execution_concurrency=4,admission_concurrency=6,
                           index_inflight_batches=6,client_mode="production")
        execution = execution_fixture("cuda")
        self.result["embedding"]["execution"] = execution
        execution["timing_scope"] = {"backend_to_native_ns":"includes preparation"}
        for name in ("before","after","measured"):
            snapshot = execution[name]
            snapshot.pop("cpu_threads")
            snapshot.update(global_intra_threads=4,session_count=4,pool_fallbacks=0,
                            session_initializations=0 if name == "measured" else 4)
            snapshot["backend_to_native_ns"] = snapshot.pop("queue_wait_ns")
        self.assertEqual(self.validate(), [])
        self.assertEqual(validate_controls(self.result,None,4), [])
        self.assertTrue(validate_controls(self.result,None,None))
        self.assertTrue(validate_controls(self.result,None,4,8))
        self.result["effective_execution_concurrency"] = 8
        self.assertIn("execution: effective concurrency differs from request capped to pool",self.validate())
        self.result["effective_execution_concurrency"] = 4
        execution["after"]["session_initializations"] += 1
        execution["measured"]["session_initializations"] = 1
        self.assertIn("execution: session pool was reloaded during measurement",self.validate())
        execution["after"]["session_initializations"] -= 1
        execution["measured"]["session_initializations"] = 0
        self.result.update(runtime_mode="direct",server_concurrency=None,resolved_index_concurrency=4)
        self.assertEqual(self.validate(), [])
        self.assertEqual(validate_controls(self.result,None,None,4), [])
        self.result["resolved_index_concurrency"] = 2
        self.assertIn("execution: actual session count differs from configured pool capacity",self.validate())


class WorkflowTests(unittest.TestCase):
    def test_report_preserves_gpu_memory_and_displays_actual_cpu_fallback(self):
        for device,provider,memory in (("metal","cpu",None),("cuda","cuda",512*1024**2),("cpu","cpu",None)):
            with self.subTest(device=device),tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                case = dict(model="local/test",device=device,budget="1",input_tokens=32,batch_size=4)
                folder = case_path(root,case,1)
                folder.mkdir(parents=True)
                record = dict(schema_version=5,experiment_fingerprint="experiment",
                    case_fingerprint=case_identity("experiment",case,1),protocol_pass=True,
                    benchmark=dict(effective_execution_concurrency=1,admission_concurrency=2,index_inflight_batches=2,
                        embedding=dict(issued=5,completed=5,errors=0,timeout=0,execution=execution_fixture(provider))),
                    gpu=dict(process_memory_peak_bytes=memory,
                             phases=dict(embedding=dict(mean_percent=20,peak_percent=30))))
                save(folder/"result.json",record)
                row,aggregate = report.summarize_case(root,case,1,"experiment")
                self.assertEqual(row[4],"512.00" if memory else "N/A")
                self.assertEqual(row[6],"20.00 / 30.00")
                self.assertIn(f"actual={provider}",row[0])
                self.assertIn("Ceff=1 · K=2 · Q=2 · sessions=1 · threads/session=2 · native threads=N/A · native B=4",row[0])
                self.assertEqual(aggregate["execution"][0]["inference_ns"],20)
                table = [dict(zip(report.HEADERS,row))]
                self.assertEqual(check_table([case],1,{fingerprint(case):[record]},set(),table,[aggregate]),[])
                report.render_html(root,"# Test\n\nMachine",[row],[aggregate])
                rendered = (root/"REPORT.html").read_text()
                self.assertEqual("CPU fallback" in rendered,device == "metal")
                aggregate["execution"][0]["session_count"] = 999
                self.assertTrue(check_table([case],1,{fingerprint(case):[record]},set(),table,[aggregate]))

    def test_table_verification_checks_raw_rounds_and_failure_mask(self):
        case = dict(model="local/test",device="cpu",budget="1",input_tokens=32,batch_size=1)
        for passed in (True,False):
            with self.subTest(passed=passed),tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                folder = case_path(root,case,1)
                folder.mkdir(parents=True)
                record = dict(schema_version=5,experiment_fingerprint="experiment",
                    case_fingerprint=case_identity("experiment",case,1),protocol_pass=passed,
                    benchmark=dict(embedding=dict(issued=4,completed=4,errors=0,timeout=0,
                        vectors_per_second=10,nominal_tokens_per_second=320,call_latency=dict(p50_ms=1.25))))
                save(folder/"result.json",record)
                row,aggregate = report.summarize_case(root,case,1,"experiment")
                table = [dict(zip(report.HEADERS,row))]
                def check(visible,metrics):
                    return check_table([case],1,{fingerprint(case):[record]},set(),visible,metrics)
                self.assertEqual(check(table,[aggregate]),[])
                wrong_case = copy.deepcopy(table)
                wrong_case[0][report.HEADERS[0]] = "another model"
                self.assertTrue(check(wrong_case,[aggregate]))
                wrong_metrics = copy.deepcopy(aggregate)
                wrong_table = copy.deepcopy(table)
                wrong_metrics["metrics"]["vectors_s"]["median"] = 999
                wrong_metrics["table_row"][2] = "999.00"
                wrong_table[0][report.HEADERS[2]] = "999.00"
                self.assertTrue(check(wrong_table,[wrong_metrics]))

    def test_failed_evidence_write_does_not_publish_result(self):
        with tempfile.TemporaryDirectory() as tmp:
            folder = Path(tmp)

            def fail_on_resources(path, value):
                if path.name == "resources.json":
                    raise OSError(errno.ENOSPC, "disk full")
                save(path, value)

            with patch("evidence.save", side_effect=fail_on_resources), self.assertRaises(OSError):
                publish_round(folder, {"protocol_pass": True}, [], [], {})
            self.assertTrue((folder / "events.json").exists())
            self.assertFalse((folder / "result.json").exists())

    def test_resume_preserves_failed_rounds_and_rejects_configuration_changes(self):
        info = dict(binary_sha256="binary",source_sha256={"source":"hash"},cargo_lock_sha256="lock",
                    features=[],build_profile={},rustc="rustc",build_environment={},ort={},machine={})
        models = [dict(model="local/test",backend="model2vec",max_input_tokens=32,max_batch_size=1)]
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            base = ["suite","--build-json",str(root/"build"),"--cache",str(root/"cache"),
                    "--output",str(root/"results"),"--devices","cpu","--embedding-concurrency","1",
                    "--lengths","32","--batch-sizes","1","--cooldown","0"]
            def fake_run(args,budget,env,info):
                self.assertEqual(args.rounds,1)
                case = dict(model=args.model,device=args.device,budget=budget,
                            input_tokens=args.input_tokens,batch_size=args.batch_size)
                dest = case_path(args.output,case,args.round_index)
                dest.mkdir(parents=True)
                for name in ("stdout.log", "stderr.log", "requests.jsonl", "background-scans.jsonl", "gpu.jsonl"):
                    (dest / name).touch()
                save(dest / "gpu-phases.json", [])
                record = dict(schema_version=5, protocol_pass=False, exit_code=1, provenance=info,
                              configuration=dict(**case, embedding_concurrency=budget, profile="smoke",
                                                 clients=args.clients, server_concurrency=args.server_concurrency, ort_thread_budget=args.ort_thread_budget),
                              experiment_fingerprint=args.experiment_fingerprint, case_fingerprint=args.case_fingerprint)
                publish_round(dest, record, [], [], {})
                return False
            with patch.object(suite,"environment",return_value=({},info)),patch.object(suite,"inventory",return_value=models),patch.object(suite,"run",side_effect=fake_run) as run,patch("builtins.print"):
                for _ in range(2):
                    with patch.object(sys,"argv",base+["--smoke"]),self.assertRaises(SystemExit) as caught:
                        suite.main()
                    self.assertEqual(caught.exception.code,1)
                self.assertEqual(run.call_count,1)
                output = root / "results"
                self.assertTrue(verify(output)["passed"])
                next(output.glob("**/r01/background.json")).unlink()
                verification = verify(output)
                self.assertFalse(verification["passed"])
                self.assertTrue(any("missing evidence background.json" in error for error in verification["errors"]))
                with patch.object(sys, "argv", base + ["--smoke"]), self.assertRaisesRegex(RuntimeError, "missing evidence background.json"):
                    suite.main()
                self.assertEqual(run.call_count, 1)

                changed_length = list(base)
                changed_length[changed_length.index("--lengths")+1] = "16"
                for command in (base,changed_length+["--smoke"],base+["--smoke","--fixed-batch"]):
                    with patch.object(sys,"argv",command),self.assertRaisesRegex(RuntimeError,"configuration changed"):
                        suite.main()
                info["source_sha256"]["source"] = "changed"
                with patch.object(sys,"argv",base+["--smoke"]),self.assertRaisesRegex(RuntimeError,"configuration changed"):
                    suite.main()
                self.assertEqual(run.call_count,1)

    def test_round_aggregation_keeps_hard_timeout_without_inventing_batch_counts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            case = dict(model="local/test",device="cpu",budget="1",input_tokens=32,batch_size=1)
            for index,rate in ((1,10),(2,20),(3,None)):
                folder = case_path(root,case,index)
                folder.mkdir(parents=True)
                benchmark = (dict(embedding=dict(issued=4,completed=4,errors=0,timeout=0,
                                                vectors_per_second=rate)) if rate is not None else None)
                save(folder/"result.json",dict(schema_version=5,experiment_fingerprint="experiment",
                    case_fingerprint=case_identity("experiment",case,index),protocol_pass=rate is not None,
                    hard_timeout=rate is None,benchmark=benchmark))
            row,aggregate = report.summarize_case(root,case,3,"experiment")
            self.assertEqual(row[1:],["-"]*(len(report.HEADERS)-1))
            self.assertEqual(aggregate["metrics"]["vectors_s"],dict(n=2,median=15,minimum=10,maximum=20))
            self.assertEqual(aggregate["completed_rounds"],3)
            self.assertEqual(aggregate["failed_rounds"],1)
            self.assertEqual(aggregate["hard_timeout_rounds"],1)
            self.assertEqual(aggregate["batch_counts"],dict(completed=8,errors=0,timeout=0))
            self.assertFalse(aggregate["batch_accounting_complete"])


if __name__ == "__main__":
    unittest.main()
