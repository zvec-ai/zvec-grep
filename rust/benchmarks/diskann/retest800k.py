"""Repeat the original 800k protocol, then run six consecutive DiskANN processes."""
import argparse
import hashlib
import json
import pathlib
import subprocess
from datetime import datetime
from zoneinfo import ZoneInfo

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=pathlib.Path, required=True)
parser.add_argument("--data", type=pathlib.Path, required=True)
parser.add_argument("--experiment", type=pathlib.Path, required=True)
parser.add_argument("--output", type=pathlib.Path, required=True)
parser.add_argument("--finalize-existing", action="store_true",
                    help="Audit a fully measured run without repeating any queries")
args = parser.parse_args()
if not args.finalize_existing:
    args.output.mkdir(parents=True, exist_ok=False)

def read(path):
    return json.loads(path.read_text())

def sha(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()

def write(name, data):
    (args.output / name).write_text(json.dumps(data, indent=2) + "\n")

def now():
    return datetime.now(ZoneInfo("Asia/Shanghai")).isoformat()

def index_files():
    files = {}
    for mode in ["diskann_fp16", "hnsw_fp16"]:
        root = args.experiment / "indexes" / f"{mode}-800000-768" / "index"
        for path in ([root] if root.is_file() else sorted(root.rglob("*"))):
            if path.is_file():
                stat = path.stat()
                files[str(path.relative_to(args.experiment))] = dict(bytes=stat.st_size, mtime_ns=stat.st_mtime_ns)
    assert files
    return files

snapshots = []
def snapshot(stage):
    snapshots.append(dict(stage=stage, time=now(),
        vm_stat=subprocess.check_output(["vm_stat"], text=True),
        swap_usage=subprocess.check_output(["sysctl", "vm.swapusage"], text=True)))
    write("system-snapshots.json", snapshots)
    write("status.json", dict(stage=stage, updated=now()))
    print(stage, flush=True)

def finalize(before, after):
    # Preserve metadata even if a check fails. A read-only API does not guarantee
    # byte-identical files: this HNSW library modified its mapped index in a probe.
    write("index-files-after.json", after)
    assert before.keys() == after.keys(), "Index file set changed"
    assert all(before[p]["bytes"] == after[p]["bytes"] for p in before), "Index size changed"
    assert all(before[p] == after[p] for p in before if "diskann_fp16" in p), "DiskANN metadata changed"
    changes = {p: dict(before=before[p], after=after[p]) for p in before if before[p] != after[p]}
    assert read(args.output / "alternating/selection.json") == selection
    assert len(read(args.output / "alternating/results.json")) == 12
    for repeat in range(6):
        stem = args.output / "diskann-only" / f"diskann-800000-{repeat}"
        rows = [json.loads(line) for line in stem.with_suffix(".jsonl").read_text().splitlines()]
        assert next(r for r in rows if r["stage"] == "queried")["queries"] == 200
        assert "maximum resident set size" in stem.with_suffix(".stderr").read_text()
    write("index-metadata-audit.json", dict(file_set_and_sizes_unchanged=True,
        diskann_metadata_unchanged=True, hnsw_metadata_changes=changes,
        initial_content_hash_not_recorded=True,
        scope="Metadata only; HNSW content invariance is not established",
        finalized_without_retiming=args.finalize_existing))
    write("status.json", dict(stage="complete", updated=now(),
        hnsw_index_changes_observed=bool(changes)))
    print("RETEST COMPLETE; index metadata changes:", list(changes), flush=True)

original = read(args.experiment / "configuration.json")
selection = read(args.experiment / "results/800000/selection.json")
assert {r["engine"]: r["effort"] for r in selection["selected"]} == {"hnsw":48, "diskann":48}
assert sha(args.binary) == original["binary_sha256"]
assert sha(args.data / "manifest.json") == original["data_manifest_sha256"]
for name, digest in original["measurement_scripts_sha256"].items():
    assert sha(pathlib.Path(__file__).with_name(name)) == digest, name
if args.finalize_existing:
    assert read(args.output / "status.json")["stage"] == "after_diskann_only"
    config = read(args.output / "configuration.json")
    assert config["binary_sha256"] == sha(args.binary)
    assert config["data_manifest_sha256"] == sha(args.data / "manifest.json")
    # Use the snapshot taken immediately after measurement, before diagnostic probes.
    finalize(config["index_files_before"], read(args.output / "index-files-after.json"))
    raise SystemExit(0)
before = index_files()
write("configuration.json", dict(count=800000, repeats_per_engine_and_protocol=6,
    hnsw_ef=48, diskann_list_size=48, precision="FP16", warmup_queries=20,
    measured_queries=200, query_concurrency=1, binary_sha256=sha(args.binary),
    data_manifest_sha256=sha(args.data / "manifest.json"),
    original_selection_sha256=sha(args.experiment / "results/800000/selection.json"),
    original_measurement_scripts_sha256=original["measurement_scripts_sha256"],
    schedule=["original validation sequence + six alternating runs per engine",
              "six consecutive fresh DiskANN processes"],
    file_cache="not purged; state carries over from alternating to DiskANN-only phase",
    rebuilding=False, index_files_before=before))
snapshot("before_alternating")
with (args.output / "alternating.log").open("w") as log:
    subprocess.run(["python3", str(pathlib.Path(__file__).with_name("run_recall90.py")),
        "--binary", str(args.binary), "--indexes", str(args.experiment / "indexes"),
        "--data", str(args.data), "--validation", str(args.data / "validation"),
        "--output", str(args.output / "alternating"), "--diskann-precision", "fp16",
        "--counts", "800000"], stdout=log, stderr=subprocess.STDOUT, check=True)
assert read(args.output / "alternating/selection.json") == selection, "Validation/parameters changed"
snapshot("after_alternating_before_diskann_only")
solo = args.output / "diskann-only"
solo.mkdir()
for repeat in range(6):
    print("DISKANN ONLY", repeat, flush=True)
    with (solo / f"diskann-800000-{repeat}.jsonl").open("w") as out, (solo / f"diskann-800000-{repeat}.stderr").open("w") as err:
        subprocess.run(["/usr/bin/time", "-l", str(args.binary), "diskann",
            str(args.experiment / "indexes/diskann_fp16-800000-768/index"), "800000", "48",
            str(args.data / "queries.f32"), "fp16"], stdout=out, stderr=err, check=True)
snapshot("after_diskann_only")
after = index_files()
finalize(before, after)
