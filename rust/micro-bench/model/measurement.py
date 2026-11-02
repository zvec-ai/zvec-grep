"""Pure measurement policy; raw evidence is retained separately from validity."""
import hashlib
import json
import os

PHASES = ("embedding",)
POLICY = dict(schema_version=5, cpu_window_seconds=1.0, maximum_sample_gap_seconds=0.25,
              background_threshold_cores=1.0, background_warning_fraction=0.1,
              minimum_batches=100, minimum_warmup_worker_waves=3,
              minimum_p95_samples=100, minimum_p99_samples=1000,
              isolation="fresh process and runtime manager per round",
              input_format="fixed Document template; nominal input size, no tokenizer or output correctness checks",
              input_length_semantics="nominal",
              corpus_schedule="deterministic inputs with L whitespace-separated words; closed-loop shared work queue")


def profile(smoke):
    return dict(name="smoke" if smoke else "baseline", duration_seconds=1 if smoke else 15,
                maximum_duration_seconds=10 if smoke else 120,
                minimum_batches=3 if smoke else POLICY["minimum_batches"])


def matrix_cases(models, devices, budgets, lengths, batch_sizes):
    """Resolve configured input scales and batch sizes from catalog limits."""
    if not lengths or not batch_sizes or any(n <= 0 for n in [*lengths,*batch_sizes]):
        raise ValueError("nominal input lengths and batch sizes must be positive")
    cases = []
    for model in models:
        token_limit, batch_limit = model["max_input_tokens"], model["max_batch_size"]
        if token_limit <= 0 or batch_limit <= 0:
            raise ValueError("catalog limits must be positive")
        for device in devices:
            for length in sorted({min(n,token_limit) for n in lengths}):
                for batch in sorted({min(n,batch_limit) for n in batch_sizes}):
                    for budget in budgets:
                        cases.append(dict(model=model["model"],device=device,budget=str(budget),
                                          input_tokens=length,batch_size=batch))
    return cases


def case_path(root, case, round_index=None):
    path = root/case["model"].split("/",1)[1]/case["device"]/f'l{case["input_tokens"]}'/f'b{case["batch_size"]}'/f'c{case["budget"]}'
    return path/f"r{round_index:02d}" if round_index is not None else path


def case_identity(experiment, case, round_index):
    return fingerprint(dict(experiment=experiment,**case,round_index=round_index))


def fingerprint(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def experiment_config(args, info, models):
    return dict(policy=POLICY, profile=profile(args.smoke), rounds=args.rounds, seed=args.seed,
                cooldown_seconds=args.cooldown, timeout_seconds=args.timeout,
                budgets=args.embedding_concurrency, devices=args.devices, models=models,
                lengths=args.lengths, batch_sizes=args.batch_sizes,
                cases=matrix_cases(models,args.devices,args.embedding_concurrency,args.lengths,args.batch_sizes),
                cache=str(args.cache.resolve()),
                provenance={key: info[key] for key in ("binary_sha256", "source_sha256", "cargo_lock_sha256",
                    "features", "build_profile", "rustc", "build_environment", "ort", "machine")},
                thread_environment={key: os.environ.get(key) for key in
                    ("OMP_NUM_THREADS", "OMP_THREAD_LIMIT", "OMP_DYNAMIC", "OMP_WAIT_POLICY", "KMP_BLOCKTIME",
                     "MKL_NUM_THREADS", "OPENBLAS_NUM_THREADS", "VECLIB_MAXIMUM_THREADS",
                     "RAYON_NUM_THREADS", "TOKENIZERS_PARALLELISM", "CUDA_VISIBLE_DEVICES")})


def resource_summary(samples, logical_cpus):
    result = {}
    for phase in sorted({s["phase"] for s in samples}):
        values = sorted((s for s in samples if s["phase"] == phase), key=lambda s:s["at_seconds"])
        elapsed = values[-1]["at_seconds"]-values[0]["at_seconds"]
        cpu = values[-1]["cpu_seconds"]-values[0]["cpu_seconds"]
        mean = 100*cpu/elapsed if elapsed > 0 else None
        windows, rejected = [], []
        # Non-overlapping, exact one-second windows. Interpolate cumulative CPU
        # only across adequately sampled endpoints; never bridge a long gap.
        origin = values[0]["at_seconds"]
        edge = origin
        cumulative = values[0]["cpu_seconds"]
        i = 0
        while edge + POLICY["cpu_window_seconds"] <= values[-1]["at_seconds"]:
            target = edge + POLICY["cpu_window_seconds"]
            gaps = []
            while i+1 < len(values) and values[i+1]["at_seconds"] < target:
                gaps.append(values[i+1]["at_seconds"]-values[i]["at_seconds"])
                i += 1
            a, b = values[i], values[i+1]
            span = b["at_seconds"]-a["at_seconds"]
            gaps.append(span)
            next_cpu = a["cpu_seconds"]+(b["cpu_seconds"]-a["cpu_seconds"])*(target-a["at_seconds"])/span
            percent = 100*(next_cpu-cumulative)/POLICY["cpu_window_seconds"]
            if max(gaps) > POLICY["maximum_sample_gap_seconds"] or not 0 <= percent <= logical_cpus*100:
                rejected.append(dict(start_seconds=edge, percent=percent, maximum_gap_seconds=max(gaps)))
            else:
                windows.append(percent)
            edge, cumulative = target, next_cpu
        mean_valid = mean is not None and 0 <= mean <= logical_cpus*100
        footprints = [v["physical_footprint_bytes"] for v in values if v["physical_footprint_bytes"] is not None]
        result[phase] = dict(samples=len(values), sampled_peak_rss_bytes=max(v["rss_bytes"] for v in values),
            last_rss_bytes=values[-1]["rss_bytes"], sampled_peak_physical_footprint_bytes=max(footprints, default=None),
            last_physical_footprint_bytes=values[-1]["physical_footprint_bytes"],
            mean_cpu_cores=mean/100 if mean_valid else None,
            mean_cpu_percent_one_core=mean if mean_valid else None, raw_mean_cpu_percent=mean,
            peak_cpu_percent_one_core=max(windows, default=None) if not rejected else None,
            cpu_peak_window_seconds=POLICY["cpu_window_seconds"], cpu_valid_windows=len(windows),
            cpu_rejected_windows=rejected, cpu_mean_valid=mean_valid)
    return result


def background_summary(intervals, events, started):
    result = {}
    for before, after in zip(events, events[1:]):
        phase = before["phase"]
        if phase not in PHASES:
            continue
        start, end = before["observed_monotonic"]-started, after["observed_monotonic"]-started
        covered = flagged = core_seconds = 0.0
        for sample in intervals:
            overlap = max(0, min(end, sample["end_seconds"])-max(start, sample["start_seconds"]))
            covered += overlap
            core_seconds += overlap*sample["aggregate_cpu_cores"]
            if sample["aggregate_cpu_cores"] > POLICY["background_threshold_cores"]:
                flagged += overlap
        fraction = flagged/covered if covered else None
        result[phase] = dict(duration_seconds=end-start, covered_seconds=covered,
            above_threshold_seconds=flagged, above_threshold_fraction=fraction,
            mean_external_cpu_cores=core_seconds/covered if covered else None,
            warning=fraction is not None and fraction >= POLICY["background_warning_fraction"],
            interpretation="observed external CPU load; impact on measured performance is unproven")
    return result
