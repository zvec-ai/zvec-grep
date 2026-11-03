"""Build real FP16 DiskANN indexes; reuse read-only HNSW fixtures for comparison."""
import argparse
import hashlib
import json
import pathlib
import shutil
import sqlite3
import subprocess
import numpy as np

parser = argparse.ArgumentParser()
parser.add_argument("--builder", type=pathlib.Path, required=True)
parser.add_argument("--data", type=pathlib.Path, required=True)
parser.add_argument("--previous-indexes", type=pathlib.Path, required=True)
parser.add_argument("--output", type=pathlib.Path, required=True)
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=False)
manifest = json.loads((args.data / "manifest.json").read_text())
with (args.data / "base.f32").open("rb") as file:
    assert hashlib.file_digest(file, "sha256").hexdigest() == manifest["files"]["base.f32"]["sha256"]
base = np.memmap(args.data / "base.f32", dtype="<f4", mode="r").reshape(-1, 768)
previous, records = 0, []
for count in manifest["counts"]:
    hnsw = args.previous_indexes / f"hnsw_fp16-{count}-768"
    assert (hnsw / "index").exists()
    (args.output / hnsw.name).symlink_to(hnsw.resolve(), target_is_directory=True)
    directory = args.output / f"diskann_fp16-{count}-768"
    directory.mkdir()
    if previous:
        shutil.copy2(args.output / f"diskann_fp16-{previous}-768/index", directory / "index")
    print("BUILD FP16", count, "from", previous, flush=True)
    with (directory / "build.jsonl").open("w") as out, (directory / "build.stderr").open("w") as err:
        subprocess.run(["/usr/bin/time", "-l", str(args.builder), "diskann", str(directory / "index"),
                        str(args.data / "base.f32"), str(count), str(previous), "fp16"],
                       stdout=out, stderr=err, check=True)
    db = sqlite3.connect(f"file:{directory / 'index'}?mode=ro", uri=True)
    assert db.execute("SELECT version,dimension,metric FROM settings").fetchone() == (2, 768, "cosine")
    actual, expected, visited = hashlib.sha256(), hashlib.sha256(), 0
    for i, (document, blob) in enumerate(db.execute("SELECT document_id,vector FROM nodes WHERE live=1 ORDER BY id")):
        assert document == f"v{i:08}" and len(blob) == 768 * 2
        actual.update(blob)
        visited += 1
    assert visited == count
    for offset in range(0, count, 1024):
        expected.update(base[offset:min(count, offset + 1024)].astype("<f2").tobytes())
    assert actual.digest() == expected.digest(), "FP16 bytes differ from independently quantized input"
    payload = db.execute("SELECT SUM(length(vector)) FROM nodes WHERE live=1").fetchone()[0]
    assert payload == count * 768 * 2
    db.close()
    records.append(dict(count=count, precision="FP16", live_vector_bytes=payload,
        database_bytes=(directory / "index").stat().st_size,
        previous_fp32_database_bytes=(args.previous_indexes / f"diskann-{count}-768/index").stat().st_size,
        all_stored_vectors_match_numpy_fp16=True, live_vectors_sha256=actual.hexdigest()))
    (args.output / "build-audit.json").write_text(json.dumps(records, indent=2))
    previous = count
    print("BUILT AND VERIFIED FP16", count, flush=True)
print("ALL FP16 INDEXES READY", flush=True)
