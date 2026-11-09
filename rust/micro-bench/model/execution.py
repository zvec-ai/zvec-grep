"""Optional runtime diagnostics; absent observations never imply a provider."""

COUNTERS = ("prepared_batches", "native_runs", "native_rows", "padded_tokens",
            "preparation_ns", "inference_ns", "postprocessing_ns")
PEAKS = ("max_native_batch_rows", "max_padded_tokens", "native_inflight_peak")


def validate_controls(benchmark, clients, server_concurrency=None, ort_thread_budget=None):
    if not benchmark:
        return []
    errors = []
    if "runtime_mode" in benchmark:
        mode = "server" if server_concurrency is not None else "direct"
        if benchmark.get("runtime_mode") != mode or benchmark.get("server_concurrency") != server_concurrency:
            errors.append("recorded runtime mode or server capacity differs from requested setting")
        hardware = benchmark.get("hardware_threads", 1)
        expected = min(ort_thread_budget, hardware) if ort_thread_budget is not None else (hardware + 1) // 2
        if benchmark.get("ort_thread_budget") != expected:
            errors.append("recorded global ORT thread budget differs from requested setting")
    elif benchmark.get("ort_sessions") != server_concurrency:
        # Historical fixed-session results remain independently verifiable.
        errors.append("recorded ORT pool setting differs from requested setting")
    if clients is None:
        if benchmark.get("client_mode") != "production":
            errors.append("recorded client mode differs from production admission")
    elif benchmark.get("client_mode") != "fixed" or benchmark.get("clients") != clients:
        errors.append("recorded client count differs from requested setting")
    return errors


def validate_execution(benchmark):
    execution = benchmark.get("embedding", {}).get("execution")
    if execution is None:
        return []  # Historical v5 results and backends without diagnostics.
    before, after, measured = (execution[name] for name in ("before", "after", "measured"))
    errors = []
    # Historical reports retain their original field; do not relabel it as
    # pure queue time. Newly generated reports must declare the timing scope.
    timing = "backend_to_native_ns" if "backend_to_native_ns" in measured else "queue_wait_ns"
    shared_pool = "global_intra_threads" in measured
    thread_gauge = "global_intra_threads" if shared_pool else "cpu_threads"
    counters = COUNTERS + (timing,)
    if timing == "backend_to_native_ns":
        counters += ("pool_fallbacks", "session_initializations") if shared_pool else ("pool_reconfigurations",)
        if not execution.get("timing_scope", {}).get(timing):
            errors.append("execution: missing backend-to-native timing scope")
    for name in counters + PEAKS + ("session_count", thread_gauge):
        if any(type(snapshot.get(name)) is not int or snapshot[name] < 0
               for snapshot in (before, after, measured)):
            errors.append(f"execution: invalid {name}")
    if errors:
        return errors
    for name in counters:
        if after[name] < before[name] or measured[name] != after[name] - before[name]:
            errors.append(f"execution: incorrect measured delta for {name}")
    if any(before[name] != after[name] or measured[name] != after[name] for name in ("actual_provider", thread_gauge)):
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
    if benchmark.get("client_mode") == "fixed":
        if type(benchmark.get("clients")) is not int or benchmark["clients"] < 1 or benchmark["clients"] != benchmark.get("index_inflight_batches"):
            errors.append("execution: invalid fixed client count")
    elif benchmark.get("admission_concurrency") != benchmark.get("index_inflight_batches"):
        errors.append("execution: benchmark workers differ from runtime admission")
    if shared_pool and measured["actual_provider"] in ("cpu", "cuda"):
        hardware = benchmark.get("hardware_threads", 1)
        requested = benchmark.get("resolved_index_concurrency")
        capacity = benchmark.get("server_concurrency") if benchmark.get("runtime_mode") == "server" else requested
        capacity = min(max(capacity or 1, 1), hardware)
        if any(snapshot["session_count"] != capacity for snapshot in (before, after, measured)):
            errors.append("execution: actual session count differs from configured pool capacity")
        if benchmark.get("effective_execution_concurrency") != min(max(requested or capacity, 1), capacity):
            errors.append("execution: effective concurrency differs from request capped to pool")
        if measured["global_intra_threads"] != benchmark.get("ort_thread_budget"):
            errors.append("execution: global thread configuration differs from benchmark provenance")
        if measured["session_initializations"] or measured["pool_fallbacks"]:
            errors.append("execution: session pool was reloaded during measurement")
    if "ort_sessions" in benchmark and measured["actual_provider"] in ("cpu", "cuda"):
        if any(snapshot["session_count"] != benchmark["ort_sessions"] for snapshot in (before, after, measured)):
            errors.append("execution: actual session count differs from configured fixed pool")
    if "cpu_threads_total" in measured:
        if any(snapshot.get("cpu_threads_total") != snapshot["cpu_threads"] * snapshot["session_count"]
               for snapshot in (before, after, measured)):
            errors.append("execution: total native threads differ from session count times threads per session")
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
    threads = (f'global intra-op={values("global_intra_threads")}' if all("global_intra_threads" in r for r in records)
               else f'threads/session={values("cpu_threads")} · native threads={values("cpu_threads_total")}')
    return (f'actual={values("actual_provider")} · Ceff={values("effective_execution_concurrency")} '
            f'· K={values("index_inflight_batches")} · Q={values("admission_concurrency")} · sessions={values("session_count")} '
            f'· {threads} '
            f'· native B={values("native_batch_rows_mean")}')
