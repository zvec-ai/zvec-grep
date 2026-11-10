"""Check performance-data accounting and completeness, not model correctness."""
import argparse
from collections import Counter
import json
import math
from pathlib import Path
import statistics

from measurement import checked_accounting, case_identity, case_path, fingerprint, matrix_cases
from evidence import missing_round_evidence, save
from report import HEADERS, display
from execution import execution_label, validate_controls


def check_table(cases, rounds, records_by_case, skipped_cases, table, metrics):
    """Recompute displayed values from rounds independently of report aggregation."""
    errors = []
    if len(table) != len(cases) or any(list(row) != HEADERS for row in table):
        return ["table columns or rows do not match planned matrix"]
    by_case = {fingerprint(m.get("case_key")):m for m in metrics}
    if len(metrics) != len(cases) or set(by_case) != {fingerprint(c) for c in cases}:
        return ["table case identities incomplete or duplicated"]
    sources = {**{f"batch_{p}":("benchmark","embedding","call_latency",f"{p}_ms") for p in ("p50","p95","p99")},
        "vectors_s":("benchmark","embedding","vectors_per_second"),
        "nominal_tokens_s":("benchmark","embedding","nominal_tokens_per_second"),
        "rss_peak_mib":("resources","embedding","sampled_peak_rss_bytes"),
        "vram_peak_mib":("gpu","process_memory_peak_bytes"),
        "cpu_mean":("resources","embedding","mean_cpu_percent_one_core"),
        "cpu_peak_1s":("resources","embedding","peak_cpu_percent_one_core"),
        "gpu_mean":("gpu","phases","embedding","mean_percent"),
        "gpu_peak":("gpu","phases","embedding","peak_percent")}
    def value(record,path):
        for key in path:
            record = record.get(key) if isinstance(record,dict) else None
        return record
    for case,row in zip(cases,table):
        key = fingerprint(case)
        records = records_by_case[key]
        successful = [r for r in records if r.get("protocol_pass")]
        expected_metrics = {}
        for name,path in sources.items():
            values = [value(r,path) for r in successful]
            scale = 1/1024**2 if name.endswith("_mib") else 1
            values = [v*scale for v in values if isinstance(v,(int,float)) and not isinstance(v,bool) and math.isfinite(v)]
            expected_metrics[name] = (dict(n=len(values),median=statistics.median(values),minimum=min(values),maximum=max(values))
                                      if values else dict(n=0,median=None,minimum=None,maximum=None))
        counts = {name:sum(value(r,("benchmark","embedding",name)) or 0 for r in records)
                  for name in ("completed","errors","timeout")}
        complete = bool(records) and all(value(r,("benchmark","embedding",name)) is not None
            for r in records for name in ("issued","completed","errors","timeout"))
        label = (f'{case["model"].removeprefix("local/")} · L={case["input_tokens"]} '
                 f'· B={case["batch_size"]} · C={case["budget"]} · {case["device"].upper()}')
        execution = []
        for record in successful:
            benchmark = record.get("benchmark") or {}
            observed = value(benchmark,("embedding","execution"))
            if observed is None:
                continue
            entry = dict(observed["measured"])
            for name in ("effective_execution_concurrency", "admission_concurrency", "index_inflight_batches"):
                entry[name] = benchmark.get(name)
            entry["native_batch_rows_mean"] = entry["native_rows"]/entry["native_runs"] if entry["native_runs"] else None
            entry["counter_scope"],entry["peak_scope"] = observed["counter_scope"],observed["peak_scope"]
            execution.append(entry)
        if execution:
            label += " · " + execution_label(execution)
        expected_row = [label,display(expected_metrics["batch_p50"],3,False),
            display(expected_metrics["vectors_s"]),display(expected_metrics["rss_peak_mib"]),
            display(expected_metrics["vram_peak_mib"]),
            " / ".join(display(expected_metrics[n]) for n in ("cpu_mean","cpu_peak_1s")),
            " / ".join(display(expected_metrics[n]) for n in ("gpu_mean","gpu_peak"))]
        if (key in skipped_cases or len(records) != rounds or len(successful) != len(records)
                or any(r.get("hard_timeout") for r in records) or counts["errors"] or counts["timeout"] or not complete):
            expected_row[1:] = ["-"]*(len(HEADERS)-1)
        aggregate = by_case[key]
        if aggregate.get("execution", []) != execution:
            errors.append(f"{label}: aggregate execution diagnostics disagree with raw rounds")
        if aggregate.get("metrics") != expected_metrics:
            errors.append(f"{label}: aggregate metrics disagree with raw rounds")
        if aggregate.get("batch_counts") != counts or aggregate.get("batch_accounting_complete") != complete:
            errors.append(f"{label}: aggregate batch counts disagree with raw rounds")
        if list(row.values()) != expected_row or aggregate.get("table_row") != expected_row:
            errors.append(f"{label}: displayed case or metrics disagree with raw rounds")
    return errors


def verify(root, require_complete=True):
    manifest = json.loads((root/"manifest.json").read_text())
    config = manifest["configuration"]
    errors, observed, request_count, skipped = [], [], 0, 0
    if config["policy"]["schema_version"] != 5:
        raise ValueError("This verifier requires a v5 performance dataset")
    cases = matrix_cases(config["models"],config["devices"],config["budgets"],config["lengths"],config["batch_sizes"])
    records_by_case = {fingerprint(case):[] for case in cases}
    skipped_cases = set()
    metadata = {model["model"]:model for model in config["models"]}
    if config["cases"] != cases or fingerprint(config) != manifest["fingerprint"]:
        errors.append("matrix or manifest fingerprint mismatch")
    for case in cases:
        folder = case_path(root,case)
        status_file = folder/"case-status.json"
        if status_file.exists():
            status = json.loads(status_file.read_text())
            if metadata[case["model"]].get("backend") != "model2vec" or case["device"] == "cpu":
                errors.append(f"{folder.relative_to(root)}: unsupported claim contradicts catalog capability")
            if (status.get("status") != "unsupported" or status.get("case_key") != case
                    or status.get("experiment_fingerprint") != manifest["fingerprint"]
                    or status.get("case_fingerprint") != case_identity(manifest["fingerprint"],case,None)
                    or not status.get("reason")):
                errors.append(f"{folder.relative_to(root)}: invalid skipped case evidence")
            if list(folder.glob("r*/result.json")):
                errors.append(f"{folder.relative_to(root)}: skipped case has measured rounds")
            skipped += 1
            skipped_cases.add(fingerprint(case))
            continue
        for round_index in range(1,config["rounds"]+1):
            path = case_path(root,case,round_index)/"result.json"
            key = str(path.relative_to(root))
            if not path.exists():
                if require_complete: errors.append(f"missing round: {key}")
                continue
            record = json.loads(path.read_text())
            missing = missing_round_evidence(path.parent, record.get("protocol_pass"))
            if missing:
                errors.append(f"{key}: missing evidence {', '.join(missing)}")
            observed.append(record)
            records_by_case[fingerprint(case)].append(record)
            expected = case_identity(manifest["fingerprint"],case,round_index)
            if (record.get("schema_version") != 5 or record.get("case_fingerprint") != expected
                    or record.get("experiment_fingerprint") != manifest["fingerprint"]):
                errors.append(f"{key}: wrong case identity")
            actual = record.get("configuration",{})
            if any(actual.get(k) != case[k] for k in ("model","device","input_tokens","batch_size")) or str(actual.get("embedding_concurrency")) != case["budget"]:
                errors.append(f"{key}: wrong recorded shape or concurrency")
            if actual.get("fixed_batch",False) != config["profile"].get("fixed_batch",False):
                errors.append(f"{key}: wrong recorded batching policy")
            for name in ("binary_sha256","source_sha256"):
                if record.get("provenance",{}).get(name) != config["provenance"][name]:
                    errors.append(f"{key}: provenance mismatch: {name}")
            benchmark = record.get("benchmark")
            if "benchmark_controls" in config:
                controls = config["benchmark_controls"]
                if any(actual.get(name) != value for name, value in controls.items()):
                    errors.append(f"{key}: wrong recorded benchmark controls")
                errors.extend(f"{key}: {error}" for error in validate_controls(
                    benchmark, controls.get("clients"),
                    controls.get("server_concurrency"),
                    controls.get("ort_thread_budget")))
            if not benchmark:
                if record.get("protocol_pass") or (record.get("exit_code") == 0 and not record.get("hard_timeout")):
                    errors.append(f"{key}: missing successful result")
                continue
            requests = {}
            raw_file = path.parent/"requests.jsonl"
            if raw_file.exists():
                for line in raw_file.read_text().splitlines():
                    value = json.loads(line)
                    if value["phase"] in requests:
                        errors.append(f"{key}: duplicate raw request phase")
                    requests[value["phase"]] = value["rows"]
            request_count += sum(len(v) for v in requests.values())
            accounting = checked_accounting(benchmark,requests,case["input_tokens"],case["batch_size"])
            if benchmark.get("fixed_batch",False) != config["profile"].get("fixed_batch",False):
                accounting.append("recorded batching policy differs from requested profile")
            if accounting != record.get("accounting_errors"):
                errors.append(f"{key}: collector/raw accounting disagreement")
            if record.get("protocol_pass") and accounting:
                errors.extend(f"{key}: {e}" for e in accounting)
            phase = benchmark.get("embedding",{})
            if not isinstance(phase,dict):
                if record.get("protocol_pass"): errors.append(f"{key}: malformed successful phase")
                continue
            for name in ("minimum_batches","maximum_duration_seconds"):
                if phase.get(name) != config["profile"][name]:
                    errors.append(f"{key}: workload differs from frozen profile: {name}")
            if phase.get("target_duration_seconds") != config["profile"]["duration_seconds"]:
                errors.append(f"{key}: target duration differs from frozen profile")
            # Allow 1 ms of scheduler bookkeeping between the final completion
            # and the decision to stop submitting; never allow a materially short run.
            elapsed = phase.get("elapsed_seconds",0)
            threshold = config["profile"]["maximum_duration_seconds"] if phase.get("stopped_by_cap") else config["profile"]["duration_seconds"]
            if record.get("protocol_pass") and elapsed+0.001 < threshold:
                errors.append(f"{key}: actual measurement ended before its duration target")
            if record.get("protocol_pass") and not phase.get("stopped_by_cap") and not phase.get("minimum_sample_met"):
                errors.append(f"{key}: uncapped round stopped before its sample target")
            if phase.get("minimum_sample_met") != (phase.get("completed",0) >= config["profile"]["minimum_batches"]):
                errors.append(f"{key}: incorrect sample sufficiency flag")
            peak = record.get("resources",{}).get("embedding",{}).get("peak_cpu_percent_one_core")
            if peak is not None and not 0 <= peak <= config["provenance"]["machine"]["logical_cpus"]*100:
                errors.append(f"{key}: impossible reported CPU peak")
    table = json.loads((root/"summary.json").read_text())
    metrics = json.loads((root/"metrics.json").read_text())
    errors.extend(check_table(cases,config["rounds"],records_by_case,skipped_cases,table,metrics))
    result = dict(passed=not errors,errors=errors,expected_cases=len(cases),table_rows=len(table),
        planned_rounds=len(cases)*config["rounds"],skipped_cases=skipped,skipped_rounds=skipped*config["rounds"],
        expected_rounds=(len(cases)-skipped)*config["rounds"],observed_rounds=len(observed),request_rows=request_count,
        protocol_pass_rounds=sum(bool(r.get("protocol_pass")) for r in observed),
        profiles=dict(Counter(r["configuration"]["profile"] for r in observed)),devices=config["devices"])
    save(root/"verification.json",result)
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root",type=Path)
    parser.add_argument("--allow-incomplete",action="store_true")
    args = parser.parse_args()
    result = verify(args.root,not args.allow_incomplete)
    print(json.dumps(result,ensure_ascii=False))
    raise SystemExit(0 if result["passed"] else 1)
