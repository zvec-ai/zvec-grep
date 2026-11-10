"""Restore the archived million-vector sample byte-for-byte, never resample.

The original data lived in /tmp. Use archived source ETags, RNG seeds, row IDs,
and SHA-256 digests to reconstruct it in a durable experiment directory.
"""
import argparse
import concurrent.futures
import gzip
import hashlib
import json
import pathlib
import re
import subprocess
import numpy as np

parser = argparse.ArgumentParser()
parser.add_argument("output", type=pathlib.Path)
parser.add_argument("--workers", type=int, default=12)
args = parser.parse_args()
root = args.output
root.mkdir(parents=True, exist_ok=True)
downloads = root / "downloads"
downloads.mkdir(exist_ok=True)
archive = pathlib.Path(__file__).resolve().parent / "results"
read = lambda name: json.loads((archive / name).read_text())
old = read("dataset.json")
million = read("million/dataset.json")
groups = read("scalar-cache/original-data-manifest.json")
holdout = read("scalar-cache/previous-holdout-manifest.json")

def digest(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()

def verify(path, expected):
    assert path.stat().st_size == expected["bytes"], path
    assert digest(path) == expected["sha256"], path

def fetch(name, start, length, path, etag):
    end = start + length - 1
    headers = path.with_suffix(".headers")
    if not (path.exists() and headers.exists() and path.stat().st_size == length):
        subprocess.run(["curl", "--fail", "--silent", "--show-error", "--retry", "3",
            "--max-time", "180", "-H", f"Range: bytes={start}-{end}",
            "-H", f"If-Match: {etag}", "-D", str(headers), "-o", str(path),
            old["source_url"] + name], check=True)
    text = headers.read_text()
    assert re.search(r"HTTP/\S+ 206", text) and f"bytes {start}-{end}/" in text
    assert re.findall(r"(?im)^ETag:\s*(.+)$", text)[-1].strip() == etag
    assert path.stat().st_size == length
    return path

def fetch_block(block):
    return fetch("wikipedia_base.bin", 8 + int(block)*3072000, 3072000,
                 downloads / f"block-{block}.f32", old["base_etag"])

def normalize(values):
    values = values.astype(np.float64)
    norms = np.linalg.norm(values, axis=1)
    assert np.isfinite(values).all() and (norms > 0).all()
    return (values / norms[:, None]).astype("<f4")

query_source = fetch("wikipedia_query.bin", 8, 5000*768*4,
                     downloads / "queries.f32", old["query_etag"])
assert digest(query_source) == groups["source_query_file_sha256"]
queries = normalize(np.fromfile(query_source, dtype="<f4").reshape(5000, 768))
all_groups = dict(groups["query_source_row_ids"])
all_groups["test-6"] = holdout["query_source_row_ids"]
unique_ids, unique32, unique16 = set(), set(), set()
for name, ids in all_groups.items():
    path = root / f"{name}.f32"
    queries[ids].tofile(path)
    expected = (holdout["files"]["queries.f32"] if name == "test-6"
                else groups["files"][path.name])
    verify(path, expected)
    for i in ids:
        assert i not in unique_ids
        unique_ids.add(i)
        for dtype, unique in [("<f4", unique32), ("<f2", unique16)]:
            key = hashlib.sha256(queries[i].astype(dtype).tobytes()).digest()
            assert key not in unique
            unique.add(key)
print("QUERIES VERIFIED: 600 warmup, 400 validation, 3500 test; disjoint FP32/FP16", flush=True)

if not (root / "base.f32").exists() or (root / "base.f32").stat().st_size != 3072000000:
    seen, source_ids = set(), []
    query_keys = {hashlib.sha256(queries[i].tobytes()).digest() for i in
                  old["query_source_row_ids"] + read("million/validation-dataset.json")["query_source_row_ids"]}
    with (root / "base.f32").open("wb") as output:
        for phase, target in [(0, 100000), (1, 1000000)]:
            rng = np.random.default_rng(old["seed"] if phase == 0 else million["seed"])
            population = 35000 if phase == 0 else np.setdiff1d(np.arange(35000), old["sampled_blocks"])
            blocks = rng.choice(population, 120 if phase == 0 else 1100, replace=False)
            assert blocks.tolist() == (old["sampled_blocks"] if phase == 0 else million["additional_sampled_blocks"])
            with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as workers:
                paths = []
                for i, path in enumerate(workers.map(fetch_block, map(int, blocks))):
                    paths.append(path)
                    if (i+1) % 25 == 0:
                        print("DOWNLOADED", phase, i+1, "/", len(blocks), flush=True)
            pool_path = root / "candidate-pool.f32"
            pool = np.memmap(pool_path, mode="w+", dtype="<f4", shape=(len(blocks)*1000, 768))
            for i, path in enumerate(paths):
                pool[i*1000:(i+1)*1000] = normalize(np.fromfile(path, dtype="<f4").reshape(1000, 768))
            pool.flush()
            duplicates = 0
            for i in rng.permutation(len(pool)):
                vector = pool[i].tobytes()
                key = hashlib.sha256(vector).digest()
                if key in seen or (phase == 1 and key in query_keys):
                    duplicates += 1
                    continue
                seen.add(key)
                output.write(vector)
                source_ids.append(int(blocks[i//1000])*1000 + int(i%1000))
                if len(source_ids) == target:
                    break
            assert len(source_ids) == target
            assert duplicates == (14 if phase == 0 else 144)
            output.flush()
            del pool
            pool_path.unlink()
            print("RESTORED BASE PREFIX", target, flush=True)
    np.save(root / "base-source-row-ids.npy", np.asarray(source_ids, dtype=np.int64))
    assert not seen.intersection(unique32)
verify(root / "base.f32", million["files"]["base.f32"])
verify(root / "base-source-row-ids.npy", million["files"]["base-source-row-ids.npy"])

# Restored bytes are identical, so the archived exhaustive ground truth remains
# exact. Check its serialized hash too; only the extra holdout's 200k/400k GT
# needs a new exhaustive scan.
with gzip.open(archive / "disjoint-comparison/raw-evidence.json.gz", "rt") as f:
    truth = json.load(f)["truth"]
for name, rows in truth.items():
    path = root / name
    expected = groups["files"][name]
    for rendered in [json.dumps(rows), json.dumps(rows, indent=2), json.dumps(rows, indent=2)+"\n"]:
        if hashlib.sha256(rendered.encode()).hexdigest() == expected["sha256"]:
            path.write_text(rendered)
            break
    verify(path, expected)
with gzip.open(archive / "recall-parity/raw-evidence.json.gz", "rt") as f:
    extra = {pathlib.Path(k).name: v for k,v in json.load(f)["truth"].items() if "/holdout/" in k}
for count in [600000, 800000, 1000000]:
    rows = extra[f"truth-{count}.json"]
    expected = holdout["files"][f"truth-{count}.json"]["sha256"]
    assert expected in {hashlib.sha256(s.encode()).hexdigest() for s in
                       [json.dumps(rows), json.dumps(rows, indent=2), json.dumps(rows, indent=2)+"\n"]}
    (root / f"test-6-truth-{count}.json").write_text(json.dumps(rows))

base = np.memmap(root / "base.f32", mode="r", dtype="<f4", shape=(1000000, 768))
q = queries[all_groups["test-6"]].astype(np.float64)
q /= np.linalg.norm(q, axis=1)[:,None]
best_scores = np.full((500, 10), -np.inf)
best_ids = np.full((500, 10), -1, dtype=np.int64)
for offset in range(0, 400000, 4000):
    b = base[offset:offset+4000].astype(np.float64)
    b /= np.linalg.norm(b, axis=1)[:,None]
    scores = q @ b.T
    ids = np.arange(offset, offset+len(b))
    for qi, row in enumerate(scores):
        local = np.flatnonzero(row >= np.partition(row, -10)[-10])
        s = np.concatenate([best_scores[qi], row[local]])
        ix = np.concatenate([best_ids[qi], ids[local]])
        order = np.lexsort((ix, -s))[:10]
        best_scores[qi], best_ids[qi] = s[order], ix[order]
    count = offset+len(b)
    if count in [200000, 400000]:
        # Independent scalar reduction against the whole subset for two rows.
        for qi in [0, 499]:
            ref = np.empty(count)
            for start in range(0, count, 4000):
                v = base[start:start+4000].astype(np.float64)
                v /= np.linalg.norm(v, axis=1)[:,None]
                ref[start:start+len(v)] = np.sum(v*q[qi], axis=1)
            assert np.array_equal(np.argsort(-ref, kind="stable")[:10], best_ids[qi])
        (root / f"test-6-truth-{count}.json").write_text(json.dumps([[f"v{i:08}" for i in row] for row in best_ids]))
        print("EXTRA HOLDOUT GT VERIFIED", count, flush=True)

manifest = dict(base_sha256=digest(root/"base.f32"), dimension=768,
    source_url=old["source_url"], base_etag=old["base_etag"], query_etag=old["query_etag"],
    restored_byte_identical=True, query_source_row_ids=all_groups,
    disjoint_by_source_fp32_fp16=True, counts=million["counts"],
    warmup_queries=600, validation_queries=400, test_shards=7, test_queries_per_shard=500,
    gt="archived exhaustive FP64 cosine for byte-identical corpus; test-6 200k/400k recomputed with scalar checks",
    files={p.name:dict(bytes=p.stat().st_size,sha256=digest(p)) for p in root.glob("*") if p.is_file() and p.name != "manifest.json"})
(root/"manifest.json").write_text(json.dumps(manifest,indent=2)+"\n")
print("RESTORATION COMPLETE", flush=True)
