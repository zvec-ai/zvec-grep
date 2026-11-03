"""Run existing native indexes on macOS; never creates or rebuilds an index."""
import argparse
import csv
import hashlib
import json
import pathlib
import re
import statistics
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=pathlib.Path, required=True)
parser.add_argument("--index-roots", type=pathlib.Path, nargs="+", required=True)
parser.add_argument("--truth-root", type=pathlib.Path, required=True)
parser.add_argument("--output", type=pathlib.Path, required=True)
parser.add_argument("--counts", type=int, nargs="+", default=[2000, 10000, 20000, 50000, 100000])
parser.add_argument("--query-file", type=pathlib.Path)
parser.add_argument("--diskann-list-size", type=int)
parser.add_argument("--diskann-precision", choices=["fp32", "fp16"], default="fp32")
parser.add_argument("--hnsw-ef", type=int, default=0)
parser.add_argument("--repeat-offset", type=int, default=0)
parser.add_argument("--require-recall-match", action="store_true")
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=True)
assert not (args.output / "results.json").exists(), "Use a fresh output directory"
records = []
for count in args.counts:
    truth = [set(row) for row in json.loads((args.truth_root / f"truth-{count}.json").read_text())]
    assert len(truth) == 200 and all(len(row) == 10 for row in truth)
    for repeat in range(args.repeat_offset, args.repeat_offset + 3):
        # Alternate order to reduce systematic cache/thermal bias.
        for engine in (["hnsw", "diskann"] if repeat % 2 == 0 else ["diskann", "hnsw"]):
            mode = "hnsw_fp16" if engine == "hnsw" else "diskann"
            if engine == "diskann" and args.diskann_precision == "fp16":
                mode = "diskann_fp16"
            index = next(root / f"{mode}-{count}-768/index" for root in args.index_roots
                         if (root / f"{mode}-{count}-768/index").exists())
            effort = (512 if count <= 10000 else 768) if engine == "diskann" else args.hnsw_ef
            if engine == "diskann" and args.diskann_list_size is not None:
                effort = args.diskann_list_size
            name = f"{engine}-{count}-{repeat}"
            print("START", name, "L", effort, flush=True)
            with (args.output / f"{name}.jsonl").open("w") as out, (args.output / f"{name}.stderr").open("w") as err:
                command = ["/usr/bin/time", "-l", str(args.binary), engine, str(index), str(count), str(effort)]
                if args.query_file:
                    command.append(str(args.query_file))
                if engine == "diskann" and args.diskann_precision == "fp16":
                    assert args.query_file, "FP16 benchmark requires an explicit query file"
                    command.append("fp16")
                subprocess.run(command,
                               stdout=out, stderr=err, check=True)
            stages = [json.loads(line) for line in (args.output / f"{name}.jsonl").read_text().splitlines()]
            row = next(s for s in stages if s["stage"] == "queried")
            found = next(s["ids"] for s in stages if s["stage"] == "results")
            assert len(found) == len(truth)
            assert all(len(row) == 10 and len(set(row)) == 10 for row in found)
            assert all(0 <= int(document.removeprefix("v")) < count for row in found for document in row)
            row["recall_at_10"] = sum(len(set(a) & b) for a, b in zip(found, truth)) / 2000
            stderr = (args.output / f"{name}.stderr").read_text()
            row["rss_mib"] = int(re.search(r"(\d+)\s+maximum resident set size", stderr).group(1)) / 2**20
            row["repeat"] = repeat
            records.append(row)
            (args.output / "results.json").write_text(json.dumps(records, indent=2))
            print("DONE", json.dumps(row), flush=True)

summary = []
for count in args.counts:
    for engine in ["hnsw", "diskann"]:
        group = [r for r in records if r["count"] == count and r["engine"] == engine]
        row = dict(count=count, engine=engine, list_size=group[0]["list_size"], repeats=len(group))
        for metric in ["qps", "mean_ms", "p50_ms", "p95_ms", "rss_mib", "recall_at_10"]:
            row[metric] = statistics.median(r[metric] for r in group)
            row[metric + "_min"] = min(r[metric] for r in group)
            row[metric + "_max"] = max(r[metric] for r in group)
        summary.append(row)
    if args.require_recall_match:
        assert summary[-1]["recall_at_10"] >= summary[-2]["recall_at_10"], "Recall target not met"
(args.output / "summary.json").write_text(json.dumps(summary, indent=2))
with (args.output / "summary.csv").open("w", newline="") as f:
    writer = csv.DictWriter(f, fieldnames=list(summary[0]), lineterminator="\n")
    writer.writeheader()
    writer.writerows(summary)
(args.output / "environment.json").write_text(json.dumps({
    "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
    "cpu": subprocess.check_output(["sysctl", "-n", "machdep.cpu.brand_string"], text=True).strip(),
    "memory_bytes": int(subprocess.check_output(["sysctl", "-n", "hw.memsize"], text=True)),
    "os": subprocess.check_output(["sw_vers"], text=True).strip(),
    "counts": args.counts, "dimension": 768, "top_k": 10, "warmup_queries": 20,
    "measured_queries": 200, "repeats": 3, "query_concurrency": 1,
    "diskann_default_list_size": 512, "qps": "200 / measured_batch_seconds",
    "file_cache": "not purged", "index_roots": list(map(str, args.index_roots)),
    "truth_root": str(args.truth_root),
    "query_file_sha256": hashlib.sha256(args.query_file.read_bytes()).hexdigest() if args.query_file else None,
    "diskann_list_size_override": args.diskann_list_size,
    "hnsw_ef": args.hnsw_ef,
    "diskann_precision": args.diskann_precision,
}, indent=2))
print("FINISHED", args.output, flush=True)
