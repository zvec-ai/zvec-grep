"""Optional runtime diagnostics; absent observations never imply a provider."""

COUNTERS = ("prepared_batches", "native_runs", "native_rows", "padded_tokens",
            "preparation_ns", "inference_ns", "postprocessing_ns", "queue_wait_ns")
PEAKS = ("max_native_batch_rows", "max_padded_tokens", "native_inflight_peak")
IDENTITY = ("actual_provider", "cpu_threads")


def validate_execution(benchmark):
    execution = benchmark.get("embedding", {}).get("execution")
    if execution is None:
        return []  # Historical v5 results and backends without diagnostics.
    before, after, measured = (execution[name] for name in ("before", "after", "measured"))
    errors = []
    for name in COUNTERS + PEAKS + ("session_count", "cpu_threads"):
        if any(type(snapshot.get(name)) is not int or snapshot[name] < 0
               for snapshot in (before, after, measured)):
            errors.append(f"execution: invalid {name}")
    for name in COUNTERS:
        if after[name] < before[name] or measured[name] != after[name] - before[name]:
            errors.append(f"execution: incorrect measured delta for {name}")
    if any(before[name] != after[name] or measured[name] != after[name] for name in IDENTITY):
        errors.append("execution: provider or native resource identity changed during measurement")
    if measured["session_count"] != after["session_count"]:
        errors.append("execution: session count is not the end-of-phase gauge")
    if not isinstance(measured["actual_provider"], str) or not measured["actual_provider"]:
        errors.append("execution: missing actual provider")
    if any(measured[name] != after[name] or after[name] < before[name] for name in PEAKS):
        errors.append("execution: incorrect runtime peaks")
    if (execution.get("counter_scope") != "measured_embedding_phase"
            or execution.get("peak_scope") != "runtime_including_warmup"):
        errors.append("execution: missing counter or peak scope")
    for name in ("effective_execution_concurrency", "admission_concurrency", "index_inflight_batches"):
        if type(benchmark.get(name)) is not int or benchmark[name] < 1:
            errors.append(f"execution: invalid {name}")
    if benchmark.get("admission_concurrency") != benchmark.get("index_inflight_batches"):
        errors.append("execution: benchmark workers differ from runtime admission")
    if benchmark.get("fixed_batch") and measured["max_native_batch_rows"] > benchmark["batch_size"]:
        errors.append("execution: fixed-batch profile merged request batches")
    return errors


def execution_records(records):
    result = []
    for record in records:
        benchmark = record.get("benchmark") or {}
        execution = benchmark.get("embedding", {}).get("execution")
        if execution is None:
            continue
        measured = execution["measured"]
        result.append(dict(measured,
            effective_execution_concurrency=benchmark.get("effective_execution_concurrency"),
            admission_concurrency=benchmark.get("admission_concurrency"),
            index_inflight_batches=benchmark.get("index_inflight_batches"),
            native_batch_rows_mean=(measured["native_rows"] / measured["native_runs"]
                                    if measured["native_runs"] else None),
            counter_scope=execution["counter_scope"], peak_scope=execution["peak_scope"]))
    return result


def execution_label(records):
    if not records:
        return ""
    def values(name):
        return "/".join(sorted({f"{r[name]:g}" if isinstance(r.get(name), (int, float))
                               else str(r.get(name) or "N/A") for r in records}))
    return (f'actual={values("actual_provider")} · Ceff={values("effective_execution_concurrency")} '
            f'· Q={values("index_inflight_batches")} · sessions={values("session_count")} '
            f'· threads={values("cpu_threads")} · native B={values("native_batch_rows_mean")}')
