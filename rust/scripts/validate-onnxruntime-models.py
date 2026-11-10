#!/usr/bin/env python3
"""Validate shipped ONNX graphs and compare CPU runtimes with one release binary."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import sys

SPEC = importlib.util.spec_from_file_location("prepare_ort", Path(__file__).with_name("prepare-onnxruntime.py"))
ort = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ort)

MODELS = ["local/bge-small-en-v1.5", "local/all-minilm-l6-v2", "local/multilingual-e5-small",
          "local/jina-embeddings-v2-base-code", "local/gte-modernbert-base", "local/nomic-embed-text-v1.5"]
BASELINES = {
    "x86_64-unknown-linux-gnu": ("linux-x64", "a3e1b79d7bb1bf09696ce675f49e4064e6c81f6202b8225624fff0e93f8d6407"),
    "aarch64-unknown-linux-gnu": ("linux-aarch64", "e15ff8b5d85afe6c144d97c6fd432254bf76a219daaf17658087d6ecb3e8f0bb"),
}


def run_test(binary, name, environment, log):
    result = subprocess.run([str(binary), name, "--ignored", "--exact", "--nocapture"],
                            env=environment, capture_output=True, text=True, timeout=1200)
    log.write_text(result.stdout + result.stderr)
    if result.returncode or "test result: ok. 1 passed; 0 failed;" not in result.stdout:
        raise RuntimeError(f"{log.name}: test failed ({result.returncode})\n{result.stderr[-3000:]}")
    return result.stdout


def validate(args):
    args.output.mkdir(parents=True, exist_ok=True)
    report = {"target": args.target, "cpu": platform.processor(), "logical_cpus": os.cpu_count(),
              "binary_sha256": ort.digest(args.test_binary), "catalog": [], "benchmark": [], "errors": []}
    candidate_dir = args.output / "runtime-1.21"
    ort.prepare(args.cache_dir, candidate_dir, args.target)
    filename = "libonnxruntime.1.dylib" if "apple" in args.target else "libonnxruntime.so.1"
    runtimes = {"1.21.0": candidate_dir / filename}
    environment = dict(os.environ, ZVEC_GREP_MODEL_CACHE=str(args.model_cache), ZVEC_GREP_SMOKE_DEVICE="cpu")
    # Cargo normally supplies these paths when it launches a test binary. This
    # runner launches the same binary directly to swap only the ORT runtime.
    library_path = "DYLD_FALLBACK_LIBRARY_PATH" if "apple" in args.target else "LD_LIBRARY_PATH"
    paths = [str(args.test_binary.parent), str(args.test_binary.parent.parent)]
    if args.cargo_messages:
        records = [json.loads(line) for line in args.cargo_messages.read_text().splitlines()]
        for record in records:
            if record.get("reason") == "build-script-executed":
                for linked in record["linked_paths"]:
                    kind, separator, path = linked.partition("=")
                    if not separator or kind in ("native", "all"):
                        paths.append(path if separator else linked)
    if environment.get(library_path):
        paths.append(environment[library_path])
    environment[library_path] = os.pathsep.join(paths)
    environment.pop("ZVEC_GREP_NATIVE_LIB_DIR", None)
    environment["ORT_DYLIB_PATH"] = str(runtimes["1.21.0"])
    subprocess.run([sys.executable, str(Path(__file__).with_name("check-onnxruntime.py")),
                    str(runtimes["1.21.0"])], check=True)
    for model in MODELS:
        try:
            text = run_test(args.test_binary,
                "models::tests::real_model_smoke::selected_local_model_downloads_loads_and_embeds",
                {**environment, "ZVEC_GREP_SMOKE_MODEL": model}, args.output / f"{model.split('/')[-1]}.log")
            evidence = next(line for line in text.splitlines() if line.startswith("MODEL_SMOKE_OK "))
            report["catalog"].append({"model": model, "result": "passed", "evidence": evidence})
            print(evidence, flush=True)
        except (RuntimeError, subprocess.TimeoutExpired) as error:
            report["errors"].append(str(error))
            print(error, flush=True)
    if not args.catalog_only:
        name, checksum = BASELINES[args.target]
        archive = f"onnxruntime-{name}-1.28.0.tgz"
        baseline_manifest = {"version": "1.28.0", "targets": {args.target: {"cpu": ["baseline"]}},
            "artifacts": {"baseline": {"archive": archive, "sha256": checksum,
                "url": f"https://github.com/microsoft/onnxruntime/releases/download/v1.28.0/{archive}",
                "files": {f"{archive[:-4]}/lib/libonnxruntime.so.1.28.0": filename}}}}
        baseline_dir = args.output / "runtime-1.28"
        ort.prepare(args.cache_dir, baseline_dir, args.target, manifest=baseline_manifest)
        runtimes["1.28.0"] = baseline_dir / filename
        subprocess.run([sys.executable, str(Path(__file__).with_name("check-onnxruntime.py")),
                        str(runtimes["1.28.0"]), "--expected", "1.28.0"], check=True)
        for model in ("local/all-minilm-l6-v2", "local/nomic-embed-text-v1.5"):
            samples = {version: [] for version in runtimes}
            # Alternate order to reduce systematic warm-cache/temperature bias.
            # Both versions use the same binary, binding, graph, inputs and CPU.
            for index, version in enumerate(("1.28.0", "1.21.0", "1.21.0", "1.28.0")):
                bench_env = dict(environment, ORT_DYLIB_PATH=str(runtimes[version]),
                    ZG_MODEL_BENCH_MODEL=model, ZG_MODEL_BENCH_DEVICE="cpu", ZG_MODEL_BENCH_CACHE=str(args.model_cache),
                    ZG_MODEL_BENCH_BATCH="2", ZG_MODEL_BENCH_CONCURRENCY="1", ZG_MODEL_BENCH_VECTORS="128",
                    ZG_MODEL_BENCH_ROUNDS="5", ZG_MODEL_BENCH_WARMUP_WAVES="4")
                try:
                    text = run_test(args.test_binary, "models::tests::model_layer_bench::model_layer_throughput",
                                    bench_env, args.output / f"bench-{model.split('/')[-1]}-{index}-{version}.log")
                    sample = json.loads(next(line.split("=", 1)[1] for line in text.splitlines()
                                             if line.startswith("ZG_MODEL_BENCH_JSON=")))
                    samples[version].append(sample)
                except (RuntimeError, subprocess.TimeoutExpired) as error:
                    report["errors"].append(str(error))
                    print(error, flush=True)
            if all(len(values) == 2 for values in samples.values()):
                medians = {version: statistics.median(v for run in runs for v in run["vectors_per_second"])
                           for version, runs in samples.items()}
                report["benchmark"].append({"model": model, "samples": samples, "median_vectors_per_second": medians,
                                            "ratio_1_21_to_1_28": medians["1.21.0"] / medians["1.28.0"]})
                print(f"{model}: {medians}; 1.21/1.28={medians['1.21.0'] / medians['1.28.0']:.3f}", flush=True)
    (args.output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
    if report["errors"]:
        raise RuntimeError("\n".join(report["errors"]))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--test-binary", required=True, type=Path)
    parser.add_argument("--cargo-messages", type=Path, help="Cargo JSON containing native dependency search paths")
    parser.add_argument("--target", required=True)
    parser.add_argument("--model-cache", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--cache-dir", type=Path, default=Path("target/ort-prebuilt"))
    parser.add_argument("--catalog-only", action="store_true")
    args = parser.parse_args()
    for name in ("test_binary", "model_cache", "output", "cache_dir"):
        setattr(args, name, getattr(args, name).resolve())
    validate(args)
