#!/usr/bin/env python3
"""Catalog-driven local model matrix. Setup and inference are separate processes."""
import argparse
import copy
import json
import random
import time
from pathlib import Path
import subprocess

from evidence import missing_round_evidence, save
from report import generate
from run import ROOT, environment, inventory, run
from measurement import case_identity, case_path, experiment_config, fingerprint


def bind_manifest(output, configuration):
    """Bind every resumed result to the complete performance experiment configuration."""
    identity = fingerprint(configuration)
    manifest = output/"manifest.json"
    if manifest.exists():
        previous = json.loads(manifest.read_text())
        if (previous.get("fingerprint") != identity
                or fingerprint(previous.get("configuration")) != identity):
            raise RuntimeError("Experiment configuration changed; use a new output directory (no results were reused)")
    elif any(output.rglob("result.json")) or any(output.rglob("case-status.json")):
        raise RuntimeError("Unidentified existing results cannot be resumed; use a new output directory")
    else:
        save(manifest, dict(fingerprint=identity, configuration=configuration))
    return identity


def unsupported_case(output, case, metadata, identity):
    """Skip only a catalog-proven capability mismatch, never an observed fallback."""
    path = case_path(output,case)/"case-status.json"
    unsupported = metadata["backend"] == "model2vec" and case["device"] != "cpu"
    if path.exists():
        record = json.loads(path.read_text())
        if (not unsupported or record.get("status") != "unsupported"
                or record.get("case_key") != case
                or record.get("experiment_fingerprint") != identity
                or record.get("case_fingerprint") != case_identity(identity,case,None)):
            raise RuntimeError(f"Cannot resume a different capability result: {path}")
        if any(path.parent.glob("r*/result.json")):
            raise RuntimeError(f"Capability skip conflicts with measured rounds: {path.parent}")
        return True
    if not unsupported:
        return False
    if any(path.parent.glob("r*/result.json")):
        raise RuntimeError(f"Cannot replace measured rounds with a capability skip: {path.parent}")
    path.parent.mkdir(parents=True,exist_ok=True)
    save(path,dict(schema_version=5,status="unsupported",reason="model2vec supports CPU execution only",
                   case_key=case,experiment_fingerprint=identity,
                   case_fingerprint=case_identity(identity,case,None),
                   requested_device=case["device"]))
    return True


def prepare(args, models, env, info):
    target = args.output/"setup"
    target.mkdir(parents=True, exist_ok=True)
    records = json.loads((target/"results.json").read_text()) if (target/"results.json").exists() else []
    for model in models:
        tag = model["model"].split("/", 1)[1]
        command = list(info["command"])
        command[1] = "models::tests::model_micro_bench::support::prepare_artifacts"
        case_env = dict(env, ZG_MODEL_BENCH_MODEL=model["model"], ZG_MODEL_BENCH_CACHE=str(args.cache.resolve()),
                        ZG_MODEL_BENCH_DEVICE="cpu", ZG_MODEL_BENCH_CONCURRENCY="1")
        print(f"Preparing {model['model']}", flush=True)
        try:
            with (target/f"{tag}.stdout.log").open("w") as out, (target/f"{tag}.stderr.log").open("w") as err:
                proc = subprocess.run(command, cwd=ROOT, env=case_env, stdout=out, stderr=err, timeout=7200)
            record = dict(model=model["model"], exit_code=proc.returncode)
        except subprocess.TimeoutExpired:
            record = dict(model=model["model"], exit_code=None, error="setup timeout")
        record["binary_sha256"] = info["binary_sha256"]
        record["source_fingerprint"] = fingerprint(info["source_sha256"])
        records = [r for r in records if r["model"] != record["model"]]+[record]
        save(target/"results.json", records)
        print(json.dumps({"model":record["model"],"exit_code":record["exit_code"]}), flush=True)
    return records


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-json", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--models", nargs="+", help="Default: all local catalog models")
    parser.add_argument("--devices", nargs="+", choices=["cpu", "metal", "cuda"], default=["cpu", "metal"])
    parser.add_argument("--embedding-concurrency", nargs="+", default=["default", "1", "2", "4", "8"])
    parser.add_argument("--lengths", nargs="+", type=int, default=[32,256,1024],
                        help="Input length L in whitespace-separated words; capped to catalog limit, then deduplicated")
    parser.add_argument("--batch-sizes", nargs="+", type=int, default=[1,4,16],
                        help="Inputs per batch; capped to each model limit, then deduplicated")
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--smoke", action="store_true")
    parser.add_argument("--timeout", type=int, default=1800)
    parser.add_argument("--clients", type=int, help="Fixed client count independent of runtime admission")
    parser.add_argument("--server-concurrency", type=int,
                        help="Use resident model pools with this server capacity; omitted: direct mode")
    parser.add_argument("--ort-thread-budget", type=int,
                        help="Global ORT intra-op threads; omitted: production automatic default")
    parser.add_argument("--rounds", type=int, default=1, help="Independent rounds per case (default: 1)")
    parser.add_argument("--seed", type=int, default=1729)
    parser.add_argument("--cooldown", type=float, default=2, help="Idle seconds between measured processes")
    args = parser.parse_args()
    if args.rounds < 1 or args.cooldown < 0:
        parser.error("rounds must be positive and cooldown nonnegative")
    if any(value is not None and value < 1 for value in (args.server_concurrency, args.ort_thread_budget, args.clients)):
        parser.error("server concurrency, thread budget and clients must be positive")
    if len(set(args.devices)) != len(args.devices) or len(set(args.embedding_concurrency)) != len(args.embedding_concurrency):
        parser.error("devices and concurrency values must not contain duplicates")
    if any(v != "default" and (not v.isdecimal() or int(v) <= 0) for v in args.embedding_concurrency):
        parser.error("embedding concurrency must be default or a positive integer")
    for name,values in (("lengths",args.lengths),("batch sizes",args.batch_sizes)):
        if any(v < 1 for v in values) or len(set(values)) != len(values):
            parser.error(f"{name} must be distinct positive integers")
    env, info = environment(args.build_json.resolve())
    models = inventory(env, info)
    if args.models:
        unknown = set(args.models)-{m["model"] for m in models}
        if unknown:
            parser.error(f"Unknown local models: {sorted(unknown)}")
        models = [m for m in models if m["model"] in args.models]
    if "cuda" in args.devices and "cuda" not in info["features"]:
        parser.error("CUDA requires a --features cuda build")
    args.output.mkdir(parents=True, exist_ok=True)
    if args.prepare_only:
        results = prepare(args, models, env, info)
        raise SystemExit(0 if all(r["exit_code"] == 0 for r in results) else 1)
    outcomes = []
    configuration = experiment_config(args, info, models)
    identity = bind_manifest(args.output,configuration)
    save(args.output/"inventory.json", models)
    metadata = {model["model"]:model for model in models}
    supported = [case for case in configuration["cases"]
                 if not unsupported_case(args.output,case,metadata[case["model"]],identity)]
    generate(args.output)
    for round_index in range(1, args.rounds+1):
        plan = list(supported)
        random.Random(args.seed+round_index).shuffle(plan)
        for entry in plan:
            case = copy.copy(args)
            case.model, case.device, case.metadata = entry["model"], entry["device"], metadata[entry["model"]]
            case.input_tokens,case.batch_size = entry["input_tokens"],entry["batch_size"]
            budget = entry["budget"]
            case.round_index = round_index
            case.experiment_fingerprint = identity
            case.case_fingerprint = case_identity(identity,entry,round_index)
            destination = case_path(args.output,entry,round_index)
            if (destination/"result.json").exists():
                saved = json.loads((destination/"result.json").read_text())
                if saved.get("case_fingerprint") != case.case_fingerprint or saved.get("experiment_fingerprint") != identity:
                    raise RuntimeError(f"Cannot resume a different case configuration: {destination}")
                missing = missing_round_evidence(destination, saved["protocol_pass"])
                if missing:
                    raise RuntimeError(f"Incomplete round {destination}: missing evidence {', '.join(missing)}")
                outcomes.append(saved["protocol_pass"])
                continue
            time.sleep(args.cooldown)
            print(json.dumps(dict(round=round_index, rounds=args.rounds, model=case.model,
                                  device=case.device,concurrency=budget,input_tokens=case.input_tokens,
                                  batch_size=case.batch_size)), flush=True)
            outcomes.append(run(case, budget, env, info))
            generate(args.output)
    generate(args.output)
    raise SystemExit(0 if all(outcomes) else 1)


if __name__ == "__main__":
    main()
