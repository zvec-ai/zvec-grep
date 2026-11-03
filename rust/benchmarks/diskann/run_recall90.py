"""Select effort on validation recall, then freeze it before held-out measurements."""
import argparse
import csv
import hashlib
import json
import pathlib
import shutil
import statistics
import subprocess
from datetime import datetime

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=pathlib.Path, required=True)
parser.add_argument("--indexes", type=pathlib.Path, required=True)
parser.add_argument("--data", type=pathlib.Path, required=True)
parser.add_argument("--validation", type=pathlib.Path, required=True)
parser.add_argument("--output", type=pathlib.Path, required=True)
parser.add_argument("--diskann-precision", choices=["fp32", "fp16"], default="fp32")
parser.add_argument("--counts", type=int, nargs="+")
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=False)
dataset = json.loads((args.data / "manifest.json").read_text())
validation = json.loads((args.validation / "manifest.json").read_text())
counts = args.counts or dataset["counts"]
assert counts == sorted(set(counts)) and set(counts).issubset(dataset["counts"])
assert not set(dataset["query_source_row_ids"]).intersection(validation["query_source_row_ids"])
for root, manifest in [(args.data, dataset), (args.validation, validation)]:
    for name, info in manifest["files"].items():
        with (root / name).open("rb") as file:
            assert hashlib.file_digest(file, "sha256").hexdigest() == info["sha256"], name
assert validation["source_dataset_manifest_sha256"] == hashlib.sha256((args.data / "manifest.json").read_bytes()).hexdigest()
target, margin = 0.90, 0.02
grid = {"hnsw": [10, 16, 24, 32, 48, 64, 96, 128, 192, 256, 512],
        "diskann": [20, 24, 32, 48, 64, 96, 128, 192, 256, 512]}
selection = dict(test_recall_target=target, validation_margin=margin,
                 validation_threshold=target + margin, candidate_grid=grid,
                 rule="first ascending effort with validation Recall@10 >= 92%; no test-set retuning",
                 trials=[], selected=[])
trials = args.output / "validation"
trials.mkdir()
# Freeze ALL selected parameters before inspecting any held-out test results.
for count in counts:
    truth = [set(x) for x in json.loads((args.validation / f"truth-{count}.json").read_text())]
    assert len(truth) == 200 and all(len(x) == 10 for x in truth)
    for engine in ["hnsw", "diskann"]:
        mode = "hnsw_fp16" if engine == "hnsw" else engine
        if engine == "diskann" and args.diskann_precision == "fp16":
            mode = "diskann_fp16"
        index = args.indexes / f"{mode}-{count}-768/index"
        for effort in grid[engine]:
            name = f"{engine}-{count}-{effort}"
            print("TUNE", name, flush=True)
            with (trials / f"{name}.jsonl").open("w") as out, (trials / f"{name}.stderr").open("w") as err:
                command = [str(args.binary), engine, str(index), str(count), str(effort),
                           str(args.validation / "queries.f32")]
                if engine == "diskann" and args.diskann_precision == "fp16":
                    command.append("fp16")
                subprocess.run(command, stdout=out, stderr=err, check=True)
            stages = [json.loads(line) for line in (trials / f"{name}.jsonl").read_text().splitlines()]
            found = next(row["ids"] for row in stages if row["stage"] == "results")
            assert len(found) == 200 and all(len(row) == len(set(row)) == 10 for row in found)
            assert all(0 <= int(doc.removeprefix("v")) < count for row in found for doc in row)
            recall = sum(len(set(a) & b) for a, b in zip(found, truth)) / 2000
            trial = dict(count=count, engine=engine, effort=effort, validation_recall_at_10=recall)
            selection["trials"].append(trial)
            print("VALIDATED", json.dumps(trial), flush=True)
            if recall >= target + margin:
                selection["selected"].append(trial)
                break
        else:
            raise RuntimeError(f"No qualifying parameter for {engine}/{count}")
        (args.output / "selection.json").write_text(json.dumps(selection, indent=2))
print("PARAMETERS FROZEN", flush=True)
all_runs, all_summary = [], []
for block, count in [(block, count) for block in range(2) for count in counts]:
    efforts = {row["engine"]: row["effort"] for row in selection["selected"] if row["count"] == count}
    output = args.output / (str(count) if block == 0 else f"{count}-extra")
    subprocess.run(["python3", str(pathlib.Path(__file__).with_name("run.py")),
                    "--binary", str(args.binary), "--index-roots", str(args.indexes),
                    "--truth-root", str(args.data), "--output", str(output), "--counts", str(count),
                    "--query-file", str(args.data / "queries.f32"),
                    "--diskann-list-size", str(efforts["diskann"]), "--hnsw-ef", str(efforts["hnsw"]),
                    "--diskann-precision", args.diskann_precision,
                    "--repeat-offset", str(block * 3)], check=True)
    all_runs.extend(json.loads((output / "results.json").read_text()))
    all_summary = []
    for n in sorted({r["count"] for r in all_runs}):
        for engine in ["hnsw", "diskann"]:
            group = [r for r in all_runs if r["count"] == n and r["engine"] == engine]
            row = dict(count=n, engine=engine, list_size=group[0]["list_size"], repeats=len(group))
            for metric in ["qps", "mean_ms", "p50_ms", "p95_ms", "rss_mib", "recall_at_10"]:
                row[metric] = statistics.median(r[metric] for r in group)
                row[metric + "_min"] = min(r[metric] for r in group)
                row[metric + "_max"] = max(r[metric] for r in group)
            all_summary.append(row)
    (args.output / "results.json").write_text(json.dumps(all_runs, indent=2))
    (args.output / "summary.json").write_text(json.dumps(all_summary, indent=2))
    with (args.output / "summary.csv").open("w", newline="") as file:
        writer = csv.DictWriter(file, fieldnames=list(all_summary[0]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(all_summary)
    print("MEASURED SCALE", count, flush=True)
environment = json.loads((output / "environment.json").read_text())
for key in ["diskann_list_size_override", "hnsw_ef"]:
    environment.pop(key)
environment.update(counts=counts, dataset=dataset["dataset"],
    measurement_date_local=datetime.now().astimezone().isoformat(),
    selection_file="selection.json", held_out_test_queries=True,
    production_defaults_changed=False, reused_indexes=True, repeats=6,
    precision={"hnsw": "FP16", "diskann": args.diskann_precision.upper()},
    effort_field="list_size means HNSW ef or DiskANN L",
    repeat_protocol="two blocks of 3 fresh processes per engine/scale; alternate engine order across all six repeats")
(args.output / "environment.json").write_text(json.dumps(environment, indent=2))
shutil.copy2(args.validation / "manifest.json", args.output / "validation-dataset.json")
print("COMPLETE; TEST TARGET STATUS", [(r["count"], r["engine"], r["recall_at_10"] >= target) for r in all_summary], flush=True)
