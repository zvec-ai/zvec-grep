#!/usr/bin/env python3
"""Compare origin/main and Rust model-backend throughput and process RSS."""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Any


RUST_TEST = "models::tests::model_layer_bench::model_layer_throughput"
JSON_PREFIX = "ZG_MODEL_BENCH_JSON="


def rss_bytes(pid: int) -> int | None:
    completed = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)],
        check=False,
        capture_output=True,
        text=True,
    )
    value = completed.stdout.strip()
    if not value:
        return None
    return int(value.splitlines()[0].strip()) * 1024


def measured_run(command: list[str], environment: dict[str, str]) -> dict[str, Any]:
    process = subprocess.Popen(
        command,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    peak = 0
    while process.poll() is None:
        current = rss_bytes(process.pid)
        if current is not None:
            peak = max(peak, current)
        time.sleep(0.01)
    stdout, stderr = process.communicate()
    current = rss_bytes(process.pid)
    if current is not None:
        peak = max(peak, current)
    if process.returncode != 0:
        print(stdout, file=sys.stderr)
        print(stderr, file=sys.stderr)
        raise RuntimeError(f"benchmark failed with exit code {process.returncode}")
    payloads = [
        json.loads(line.removeprefix(JSON_PREFIX))
        for line in stdout.splitlines()
        if line.startswith(JSON_PREFIX)
    ]
    if len(payloads) != 1:
        raise RuntimeError(f"expected one benchmark payload, got {len(payloads)}")
    return {**payloads[0], "peak_rss_bytes": peak}


def command_for(implementation: str, arguments: argparse.Namespace) -> list[str]:
    if implementation == "rust":
        return [
            str(arguments.rust_bin),
            "--exact",
            RUST_TEST,
            "--ignored",
            "--nocapture",
        ]
    return ["node", "--expose-gc", str(arguments.main_script)]


def environment_for(
    implementation: str,
    arguments: argparse.Namespace,
    *,
    baseline: bool,
    concurrency: int = 1,
) -> dict[str, str]:
    environment = dict(os.environ)
    environment.update(
        {
            "ZG_MODEL_BENCH_MAIN_ROOT": str(arguments.main_root),
            "ZG_MODEL_BENCH_CACHE": str(arguments.cache),
            "ZG_MODEL_BENCH_MODEL": arguments.model,
            "ZG_MODEL_BENCH_DEVICE": arguments.device,
            "ZG_MODEL_BENCH_BATCH": str(arguments.batch),
            "ZG_MODEL_BENCH_CONCURRENCY": str(concurrency),
            "ZG_MODEL_BENCH_VECTORS": str(arguments.vectors),
            "ZG_MODEL_BENCH_ROUNDS": str(arguments.rounds),
            "ZG_MODEL_BENCH_WARMUP_WAVES": str(arguments.warmup_waves),
        }
    )
    if baseline:
        environment["ZG_MODEL_BENCH_BASELINE"] = "1"
    else:
        environment.pop("ZG_MODEL_BENCH_BASELINE", None)
    if (
        sys.platform == "darwin"
        and environment.get("ZVEC_GREP_METAL_KEEP_RESIDENCY") != "1"
    ):
        environment.setdefault("GGML_METAL_NO_RESIDENCY", "1")
    return environment


def summarize(records: list[dict[str, Any]], baseline: int) -> dict[str, Any]:
    process_medians = [
        statistics.median(record["vectors_per_second"]) for record in records
    ]
    request_medians = [
        statistics.median(record["requests_per_second"]) for record in records
    ]
    peak_rss = [record["peak_rss_bytes"] for record in records]
    loaded_rss = [record["loaded_rss_bytes"] for record in records]
    checksums = [record["checksum"] for record in records]
    median_peak = int(statistics.median(peak_rss))
    median_loaded = int(statistics.median(loaded_rss))
    return {
        "vectors_per_second_median": statistics.median(process_medians),
        "vectors_per_second_process_medians": process_medians,
        "requests_per_second_median": statistics.median(request_medians),
        "peak_rss_bytes_median": median_peak,
        "peak_rss_bytes_samples": peak_rss,
        "loaded_rss_bytes_median": median_loaded,
        "loaded_rss_bytes_samples": loaded_rss,
        "baseline_rss_bytes_median": baseline,
        "incremental_loaded_rss_bytes": median_loaded - baseline,
        "incremental_model_rss_bytes": median_peak - baseline,
        "checksums": checksums,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rust-bin", type=Path, required=True)
    parser.add_argument("--main-root", type=Path, required=True)
    parser.add_argument(
        "--main-script",
        type=Path,
        default=Path(__file__).with_name("main.mjs"),
    )
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--model", default="local/potion-code-16m-v2")
    parser.add_argument("--device", default="cpu")
    parser.add_argument("--batch", type=int, default=256)
    parser.add_argument("--vectors", type=int, default=16_384)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--warmup-waves", type=int, default=2)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--concurrency", type=int, nargs="+", default=[1, 2])
    arguments = parser.parse_args()
    arguments.rust_bin = arguments.rust_bin.resolve()
    arguments.main_root = arguments.main_root.resolve()
    arguments.main_script = arguments.main_script.resolve()
    arguments.cache = arguments.cache.resolve()

    baselines: dict[str, list[int]] = {"rust": [], "main": []}
    for implementation in ("rust", "main"):
        for repeat in range(arguments.repeats):
            record = measured_run(
                command_for(implementation, arguments),
                environment_for(implementation, arguments, baseline=True),
            )
            baselines[implementation].append(record["peak_rss_bytes"])
            print(
                f"baseline implementation={implementation} repeat={repeat + 1} "
                f"peak_rss_bytes={record['peak_rss_bytes']}",
                flush=True,
            )

    records: dict[str, dict[int, list[dict[str, Any]]]] = {
        "rust": {value: [] for value in arguments.concurrency},
        "main": {value: [] for value in arguments.concurrency},
    }
    for concurrency in arguments.concurrency:
        for repeat in range(arguments.repeats):
            order = ("rust", "main") if repeat % 2 == 0 else ("main", "rust")
            for implementation in order:
                record = measured_run(
                    command_for(implementation, arguments),
                    environment_for(
                        implementation,
                        arguments,
                        baseline=False,
                        concurrency=concurrency,
                    ),
                )
                records[implementation][concurrency].append(record)
                print(
                    f"model implementation={implementation} concurrency={concurrency} "
                    f"repeat={repeat + 1} "
                    f"vectors_per_second={statistics.median(record['vectors_per_second']):.3f} "
                    f"peak_rss_bytes={record['peak_rss_bytes']}",
                    flush=True,
                )

    baseline_medians = {
        implementation: int(statistics.median(values))
        for implementation, values in baselines.items()
    }
    summary = {
        "settings": {
            "model": arguments.model,
            "device": arguments.device,
            "batch": arguments.batch,
            "vectors_per_round": arguments.vectors,
            "rounds": arguments.rounds,
            "warmup_waves": arguments.warmup_waves,
            "repeats": arguments.repeats,
        },
        "baseline_rss_bytes": baselines,
        "results": {
            implementation: {
                str(concurrency): summarize(
                    records[implementation][concurrency],
                    baseline_medians[implementation],
                )
                for concurrency in arguments.concurrency
            }
            for implementation in ("rust", "main")
        },
    }
    print(f"ZG_MODEL_BENCH_COMPARISON_JSON={json.dumps(summary, sort_keys=True)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
