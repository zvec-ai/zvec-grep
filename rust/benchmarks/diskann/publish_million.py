"""Audit raw million-scale measurements and export small, portable report artifacts."""
import argparse
import csv
import hashlib
import json
import math
import pathlib
import re
import shutil
import statistics

parser = argparse.ArgumentParser()
parser.add_argument("--data", type=pathlib.Path, required=True)
parser.add_argument("--experiment", type=pathlib.Path, required=True)
parser.add_argument("--output", type=pathlib.Path, required=True)
parser.add_argument("--completed-only", action="store_true",
                    help="Audit completed scales while later scales are still running")
args = parser.parse_args()

def read(path):
    return json.loads(path.read_text())

def sha(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()

def write(name, value):
    (args.output / name).write_text(json.dumps(value, indent=2) + "\n")

dataset = read(args.data / "manifest.json")
validation = read(args.data / "validation/manifest.json")
requested_counts = [200000, 400000, 600000, 800000, 1000000]
assert dataset["counts"] == requested_counts
assert validation["source_dataset_manifest_sha256"] == sha(args.data / "manifest.json")
assert not set(dataset["query_source_row_ids"]).intersection(validation["query_source_row_ids"])
for folder, manifest in [(args.data, dataset), (args.data / "validation", validation)]:
    for name, info in manifest["files"].items():
        assert (folder / name).stat().st_size == info["bytes"]
        assert sha(folder / name) == info["sha256"], name
if not args.completed_only:
    assert read(args.experiment / "status.json")["stage"] == "complete"
configuration = read(args.experiment / "configuration.json")
assert configuration["data_manifest_sha256"] == sha(args.data / "manifest.json")
for name, digest in configuration["measurement_scripts_sha256"].items():
    assert sha(pathlib.Path(__file__).with_name(name)) == digest, name
runs = read(args.experiment / "results.json")
summary = read(args.experiment / "summary.json")
counts = sorted({row["count"] for row in runs})
assert counts and counts == requested_counts[:len(counts)]
if not args.completed_only:
    assert counts == requested_counts
assert len(runs) == len(counts) * 12 and len(summary) == len(counts) * 2
selections, environments, evidence, diagnostics = [], [], [], []

def raw_recall(path, truth, count):
    stages = [json.loads(line) for line in path.read_text().splitlines()]
    ids = next(row["ids"] for row in stages if row["stage"] == "results")
    assert len(ids) == len(truth) == 200
    assert all(len(row) == len(set(row)) == 10 for row in ids)
    assert all(re.fullmatch(r"v\d{8}", doc) and int(doc[1:]) < count for row in ids for doc in row)
    recall = sum(len(set(found).intersection(expected)) for found, expected in zip(ids, truth)) / 2000
    evidence.append(dict(path=str(path.relative_to(args.experiment)), sha256=sha(path)))
    return recall, stages

for count in counts:
    root = args.experiment / "results" / str(count)
    selection = read(root / "selection.json")
    assert selection["validation_threshold"] == .92 and selection["test_recall_target"] == .90
    selections.append(selection)
    validation_truth = read(args.data / "validation" / f"truth-{count}.json")
    truth = read(args.data / f"truth-{count}.json")
    for trial in selection["trials"]:
        assert trial["count"] == count
        path = root / "validation" / f"{trial['engine']}-{count}-{trial['effort']}.jsonl"
        recall, _ = raw_recall(path, validation_truth, count)
        assert recall == trial["validation_recall_at_10"]
    for engine in ["hnsw", "diskann"]:
        trials = [row for row in selection["trials"] if row["engine"] == engine]
        chosen = [row for row in selection["selected"] if row["engine"] == engine]
        assert len(chosen) == 1 and trials[-1] == chosen[0]
        assert [row["effort"] for row in trials] == selection["candidate_grid"][engine][:len(trials)]
        assert all(row["validation_recall_at_10"] < .92 for row in trials[:-1])
        assert chosen[0]["validation_recall_at_10"] >= .92
        group = [row for row in runs if row["count"] == count and row["engine"] == engine]
        assert sorted(row["repeat"] for row in group) == list(range(6))
        for run in group:
            repeat = run["repeat"]
            run_root = root / (str(count) if repeat < 3 else f"{count}-extra")
            stem = f"{engine}-{count}-{repeat}"
            recall, stages = raw_recall(run_root / f"{stem}.jsonl", truth, count)
            measured = next(row for row in stages if row["stage"] == "queried")
            assert all(run[key] == value for key, value in measured.items())
            assert run["recall_at_10"] == recall
            assert run["storage_precision"] == "fp16" and run["queries"] == 200
            assert run["list_size"] == chosen[0]["effort"]
            assert math.isclose(run["qps"], 200000 / run["batch_ms"], rel_tol=1e-12)
            err = run_root / f"{stem}.stderr"
            stderr = err.read_text()
            rss = int(re.search(r"(\d+)\s+maximum resident set size", stderr)[1]) / 2**20
            assert rss == run["rss_mib"]
            elapsed = re.search(r"([\d.]+)\s+real\s+([\d.]+)\s+user\s+([\d.]+)\s+sys", stderr)
            diagnostics.append(dict(count=count, engine=engine, repeat=repeat,
                process_real_s=float(elapsed[1]), process_user_s=float(elapsed[2]), process_sys_s=float(elapsed[3]),
                page_faults=int(re.search(r"(\d+)\s+page faults", stderr)[1]),
                page_reclaims=int(re.search(r"(\d+)\s+page reclaims", stderr)[1])))
            evidence.append(dict(path=str(err.relative_to(args.experiment)), sha256=sha(err)))
        aggregate = [row for row in summary if row["count"] == count and row["engine"] == engine]
        assert len(aggregate) == 1
        aggregate = aggregate[0]
        assert aggregate["repeats"] == 6 and aggregate["list_size"] == chosen[0]["effort"]
        for metric in ["qps", "mean_ms", "p50_ms", "p95_ms", "rss_mib", "recall_at_10"]:
            values = [row[metric] for row in group]
            assert aggregate[metric] == statistics.median(values)
            assert aggregate[metric + "_min"] == min(values)
            assert aggregate[metric + "_max"] == max(values)
    env = read(root / "environment.json")
    assert env["binary_sha256"] == configuration["binary_sha256"]
    assert env["precision"] == {"hnsw": "FP16", "diskann": "FP16"}
    assert env["query_file_sha256"] == dataset["files"]["queries.f32"]["sha256"]
    env.update(index_roots=["million-scale-indexes"], truth_root="million-scale-data")
    environments.append(env)

selection = {k: v for k, v in selections[0].items() if k not in ["trials", "selected"]}
assert all({k: v for k, v in s.items() if k not in ["trials", "selected"]} == selection for s in selections)
selection.update(trials=[row for s in selections for row in s["trials"]],
                 selected=[row for s in selections for row in s["selected"]],
                 freeze_protocol="fixed grid and rule for all scales; freeze both engines at each scale before its test measurements")
build_audit = read(args.experiment / "build-audit.json")
assert [row["count"] for row in build_audit] == counts
assert all(row["all_stored_vectors_match_numpy_fp16"] and row["precision"] == "FP16" for row in build_audit)
audit = dict(verified_test_runs=len(runs), verified_validation_trials=len(selection["trials"]),
             all_requested_scales_complete=counts == requested_counts,
             both_engines_storage_precision="FP16", all_runs_use_frozen_parameters=True,
             recall_recomputed_from_raw_ids=True, qps_formula_verified=True, rss_from_time_verified=True,
             all_summary_median_min_max_verified=True, all_dataset_file_hashes_verified=True,
             ground_truth="recomputed exhaustive FP64 cosine on original FP32 prefixes",
             all_test_groups_reach_90_percent=all(row["recall_at_10_min"] >= .90 for row in summary))
args.output.mkdir(parents=True, exist_ok=True)
# Keep the exact bytes: the validation manifest refers to the dataset manifest's SHA256.
shutil.copyfile(args.data / "manifest.json", args.output / "dataset.json")
shutil.copyfile(args.data / "validation/manifest.json", args.output / "validation-dataset.json")
write("runs.json", runs)
write("summary.json", summary)
write("selection.json", selection)
write("build-audit.json", build_audit)
write("audit.json", audit)
write("raw-evidence.json", evidence)
write("process-diagnostics.json", dict(
    scope="entire process, including index open and warmup; not restricted to measured queries",
    source="/usr/bin/time -l stderr; original file hashes are in raw-evidence.json",
    records=diagnostics))
write("environment.json", dict(
    implementation_commit="74c7d39", binary_sha256=configuration["binary_sha256"],
    data_manifest_sha256=configuration["data_manifest_sha256"],
    builder_sha256=configuration["builder_sha256"], counts=counts,
    measurement_scripts_sha256=configuration["measurement_scripts_sha256"],
    query_precision={"hnsw":"FP16", "diskann":"FP16 rounded, widened to FP32"},
    diskann_distance_arithmetic="FP32 decoded values with existing FP64 accumulators",
    ground_truth_precision="original FP32 inputs, exhaustive FP64 cosine",
    fixture_build="DiskANN snapshots extended from verified 100k FP16 prefix; HNSW freshly built per scale",
    build_and_query_overlap=False, per_scale=environments))
with (args.output / "summary.csv").open("w", newline="") as file:
    writer = csv.DictWriter(file, fieldnames=list(summary[0]), lineterminator="\n")
    writer.writeheader()
    writer.writerows(summary)
print(json.dumps(audit, indent=2))
