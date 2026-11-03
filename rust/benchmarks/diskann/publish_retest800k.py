"""Audit original and repeated 800k results; preserve the original experiment."""
import argparse
import csv
import hashlib
import json
import math
import pathlib
import re
import shutil
import sqlite3
import statistics

parser = argparse.ArgumentParser()
parser.add_argument("--data", type=pathlib.Path, required=True)
parser.add_argument("--experiment", type=pathlib.Path, required=True)
parser.add_argument("--retest", type=pathlib.Path, required=True)
parser.add_argument("--output", type=pathlib.Path, required=True)
args = parser.parse_args()

def read(path):
    return json.loads(path.read_text())

def sha(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()

def write(name, data):
    (args.output / name).write_text(json.dumps(data, indent=2) + "\n")

assert read(args.retest / "status.json")["stage"] == "complete"
config = read(args.retest / "configuration.json")
assert config["data_manifest_sha256"] == sha(args.data / "manifest.json")
before, after = config["index_files_before"], read(args.retest / "index-files-after.json")
assert before.keys() == after.keys()
assert all(before[p]["bytes"] == after[p]["bytes"] for p in before)
assert all(before[p] == after[p] for p in before if "diskann_fp16" in p)
metadata_audit = read(args.retest / "index-metadata-audit.json")
assert metadata_audit["hnsw_metadata_changes"] == {
    p: dict(before=before[p], after=after[p]) for p in before if before[p] != after[p]}
for name, digest in config["original_measurement_scripts_sha256"].items():
    assert sha(pathlib.Path(__file__).with_name(name)) == digest
for folder in [args.data, args.data / "validation"]:
    for name, info in read(folder / "manifest.json")["files"].items():
        assert sha(folder / name) == info["sha256"], name
truth = read(args.data / "truth-800000.json")
validation_truth = read(args.data / "validation/truth-800000.json")
assert len(truth) == len(validation_truth) == 200
assert all(len(set(row)) == 10 for row in [*truth, *validation_truth])
evidence, diagnostics, records = [], [], []
original_ids = {}
same_ids = []

def results(path, ground_truth, scope, root):
    stages = [json.loads(line) for line in path.read_text().splitlines()]
    ids = next(row["ids"] for row in stages if row["stage"] == "results")
    assert len(ids) == 200 and all(len(row) == len(set(row)) == 10 for row in ids)
    assert all(re.fullmatch(r"v\d{8}", doc) and int(doc[1:]) < 800000 for row in ids for doc in row)
    recall = sum(len(set(found).intersection(expected)) for found, expected in zip(ids, ground_truth)) / 2000
    evidence.append(dict(scope=scope, path=str(path.relative_to(root)), sha256=sha(path)))
    return stages, ids, recall

selection = read(args.retest / "alternating/selection.json")
assert selection == read(args.experiment / "results/800000/selection.json")
assert config["original_selection_sha256"] == sha(args.retest / "alternating/selection.json")
for trial in selection["trials"]:
    path = args.retest / "alternating/validation" / f"{trial['engine']}-800000-{trial['effort']}.jsonl"
    _, _, recall = results(path, validation_truth, "retest", args.retest)
    assert recall == trial["validation_recall_at_10"]
for protocol in ["original", "alternating", "diskann_only"]:
    for engine in (["diskann"] if protocol == "diskann_only" else ["hnsw", "diskann"]):
        for repeat in range(6):
            if protocol == "original":
                root = args.experiment
                folder = root / "results/800000" / ("800000" if repeat < 3 else "800000-extra")
            elif protocol == "alternating":
                root = args.retest
                folder = root / "alternating" / ("800000" if repeat < 3 else "800000-extra")
            else:
                root = args.retest
                folder = root / "diskann-only"
            stem = f"{engine}-800000-{repeat}"
            stages, ids, recall = results(folder / f"{stem}.jsonl", truth,
                                         "original" if protocol == "original" else "retest", root)
            row = next(s for s in stages if s["stage"] == "queried")
            assert (row["engine"], row["count"], row["queries"], row["list_size"], row["storage_precision"]) == (engine,800000,200,48,"fp16")
            assert math.isclose(row["qps"], 200000 / row["batch_ms"], rel_tol=1e-12)
            assert recall == (0.9615 if engine == "hnsw" else 0.965)
            if protocol == "original" and repeat == 0:
                original_ids[engine] = ids
            elif protocol != "original":
                same_ids.append(ids == original_ids[engine])
            err = folder / f"{stem}.stderr"
            stderr = err.read_text()
            rss = int(re.search(r"(\d+)\s+maximum resident set size", stderr)[1]) / 2**20
            row.update(protocol=protocol, repeat=repeat, recall_at_10=recall, rss_mib=rss)
            records.append(row)
            elapsed = re.search(r"([\d.]+)\s+real\s+([\d.]+)\s+user\s+([\d.]+)\s+sys", stderr)
            diagnostics.append(dict(protocol=protocol, engine=engine, repeat=repeat,
                process_real_s=float(elapsed[1]), process_user_s=float(elapsed[2]), process_sys_s=float(elapsed[3]),
                page_faults=int(re.search(r"(\d+)\s+page faults", stderr)[1]),
                page_reclaims=int(re.search(r"(\d+)\s+page reclaims", stderr)[1])))
            evidence.append(dict(scope="original" if protocol == "original" else "retest",
                                 path=str(err.relative_to(root)), sha256=sha(err)))

# Scan stored vectors only AFTER the timed runs, avoiding a cache-priming scan before the retest.
index = args.experiment / "indexes/diskann_fp16-800000-768/index"
db = sqlite3.connect(f"file:{index}?mode=ro", uri=True)
db.executescript("PRAGMA cache_size=-256; PRAGMA mmap_size=0;")
assert db.execute("SELECT version,dimension,metric FROM settings").fetchone() == (2,768,"cosine")
digest = hashlib.sha256()
visited = 0
for i,(node,document,blob) in enumerate(db.execute("SELECT id,document_id,vector FROM nodes WHERE live=1 ORDER BY id")):
    assert node == i+1 and document == f"v{i:08}" and len(blob) == 1536
    digest.update(blob)
    visited += 1
assert visited == 800000 and db.execute("SELECT COUNT(*) FROM nodes").fetchone()[0] == 800001
db.close()
expected = read(index.parent / "build-complete.json")
assert digest.hexdigest() == expected["live_vectors_sha256"]
summary = []
for protocol, engine in dict.fromkeys((r["protocol"],r["engine"]) for r in records):
    group = [r for r in records if (r["protocol"],r["engine"]) == (protocol,engine)]
    assert sorted(r["repeat"] for r in group) == list(range(6))
    row = dict(protocol=protocol, engine=engine, count=800000, list_size=48, repeats=6)
    for metric in ["qps","mean_ms","p50_ms","p95_ms","rss_mib","recall_at_10"]:
        values = [r[metric] for r in group]
        row.update({metric:statistics.median(values), metric+"_min":min(values), metric+"_max":max(values)})
    summary.append(row)
assert len(records) == 30 and len(summary) == 5 and len(same_ids) == 18
args.output.mkdir(parents=True, exist_ok=True)
write("runs.json", records)
write("summary.json", summary)
write("raw-evidence.json", evidence)
write("process-diagnostics.json", dict(scope="entire process, including index open and warmup",records=diagnostics))
write("system-snapshots.json", dict(scope="system-wide counters; not attributable to either engine alone",
    snapshots=read(args.retest / "system-snapshots.json")))
write("configuration.json", config)
write("index-files-after.json", after)
write("index-metadata-audit.json", metadata_audit)
for name in ["hnsw-content-probe.json", "hnsw-content-probe-diff.json"]:
    if (args.retest / name).exists():
        shutil.copyfile(args.retest / name, args.output / name)
env = read(args.retest / "alternating/environment.json")
env.update(index_roots=["original-million-scale-indexes"],truth_root="original-million-scale-data",
    original_measurement_commit="da1f7c8",schedule=config["schedule"],
    diskann_only_repeats=6, extra_warmup_or_cache_purge=False)
write("environment.json", env)
shutil.copyfile(args.retest / "alternating/selection.json", args.output / "selection.json")
audit = dict(verified_original_test_runs=12, verified_new_test_runs=18,
    verified_repeated_validation_trials=len(selection["trials"]),
    same_frozen_parameters=True, same_recall=True, all_new_result_ids_match_original=all(same_ids),
    data_file_hashes_verified=True, index_file_set_and_sizes_unchanged=True,
    diskann_index_metadata_unchanged=True,
    hnsw_metadata_changed_files=list(metadata_audit["hnsw_metadata_changes"]),
    hnsw_whole_retest_content_invariance_established=False,
    diagnostic_probe_excluded_from_timing=True,
    diskann_live_vectors_sha256=digest.hexdigest(), diskann_stored_vectors_match_original=True,
    recall_recomputed_from_raw_ids=True, qps_formula_verified=True, rss_from_time_verified=True,
    all_summary_median_min_max_recomputed=True, original_results_preserved=True)
write("audit.json", audit)
with (args.output / "summary.csv").open("w",newline="") as file:
    writer = csv.DictWriter(file,fieldnames=list(summary[0]),lineterminator="\n")
    writer.writeheader();writer.writerows(summary)
print(json.dumps(audit,indent=2))
for row in summary:
    print(row["protocol"],row["engine"],"QPS",row["qps"],"range",row["qps_min"],row["qps_max"],flush=True)
