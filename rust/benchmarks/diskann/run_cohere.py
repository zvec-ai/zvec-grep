"""Build dedicated benchmark indexes and query them sequentially, without overlap."""
import argparse
import csv
import hashlib
import json
import pathlib
import shutil
import sqlite3
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=pathlib.Path, required=True)
parser.add_argument("--builder", type=pathlib.Path, required=True)
parser.add_argument("--data", type=pathlib.Path, required=True)
parser.add_argument("--indexes", type=pathlib.Path, required=True)
parser.add_argument("--output", type=pathlib.Path, required=True)
args = parser.parse_args()
assert args.binary.is_file() and args.builder.is_file(), "Build both benchmark binaries first"
manifest = json.loads((args.data / "manifest.json").read_text())
assert manifest["dimension"] == 768
args.indexes.mkdir(parents=True, exist_ok=True)
args.output.mkdir(parents=True, exist_ok=True)
assert not (args.output / "results.json").exists()
all_runs, all_summary = [], []
previous = 0
for count in manifest["counts"]:
    for engine in ["hnsw", "diskann"]:
        mode = "hnsw_fp16" if engine == "hnsw" else "diskann"
        directory = args.indexes / f"{mode}-{count}-768"
        directory.mkdir()
        start = previous if engine == "diskann" else 0
        if start:
            shutil.copy2(args.indexes / f"diskann-{start}-768/index", directory / "index")
        print("BUILD", engine, count, "from", start, flush=True)
        with (directory / "build.jsonl").open("w") as out, (directory / "build.stderr").open("w") as err:
            subprocess.run(["/usr/bin/time", "-l", str(args.builder), engine, str(directory / "index"),
                            str(args.data / "base.f32"), str(count), str(start)],
                           stdout=out, stderr=err, check=True)
        if engine == "diskann":
            db = sqlite3.connect(f"file:{directory / 'index'}?mode=ro", uri=True)
            assert db.execute("SELECT COUNT(*) FROM nodes WHERE live=1").fetchone()[0] == count
            # Check ordinal IDs against bytes used to recompute ground truth.
            with (args.data / "base.f32").open("rb") as file:
                for i in [0, count // 2, count - 1]:
                    file.seek(i * 768 * 4)
                    expected = file.read(768 * 4)
                    actual = db.execute("SELECT vector FROM nodes WHERE document_id=? AND live=1", (f"v{i:08}",)).fetchone()[0]
                    assert actual == expected
            db.close()
        print("BUILT", engine, count, flush=True)
    previous = count
    output = args.output / str(count)
    subprocess.run(["python3", str(pathlib.Path(__file__).with_name("run.py")),
                    "--binary", str(args.binary), "--index-roots", str(args.indexes),
                    "--truth-root", str(args.data), "--output", str(output), "--counts", str(count),
                    "--query-file", str(args.data / "queries.f32"), "--diskann-list-size", "512"], check=True)
    all_runs.extend(json.loads((output / "results.json").read_text()))
    all_summary.extend(json.loads((output / "summary.json").read_text()))
    (args.output / "results.json").write_text(json.dumps(all_runs, indent=2))
    (args.output / "summary.json").write_text(json.dumps(all_summary, indent=2))
    with (args.output / "summary.csv").open("w", newline="") as file:
        writer = csv.DictWriter(file, fieldnames=list(all_summary[0]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(all_summary)
    print("COMPLETE SCALE", count, flush=True)
environment = json.loads((args.output / str(previous) / "environment.json").read_text())
environment.update(counts=manifest["counts"], dataset=manifest["dataset"], seed=manifest["seed"],
    dataset_manifest_sha256=hashlib.sha256((args.data / "manifest.json").read_bytes()).hexdigest(),
    fixture_build="production backend; nested DiskANN snapshots continue the identical insertion order; fresh HNSW builds",
    measurement_date_local="2026-11-03", timezone="Asia/Shanghai")
(args.output / "environment.json").write_text(json.dumps(environment, indent=2))
shutil.copy2(args.data / "manifest.json", args.output / "dataset.json")
print("ALL COHERE TESTS COMPLETE", flush=True)
