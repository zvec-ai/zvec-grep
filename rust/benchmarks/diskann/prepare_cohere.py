"""Sample real Cohere vectors via HTTP ranges and recompute per-subset cosine GT."""
import argparse
import concurrent.futures
import hashlib
import json
import pathlib
import re
import struct
import subprocess
import numpy as np

URL = "https://comp21storage.z5.web.core.windows.net/wiki-cohere-35M/"
parser = argparse.ArgumentParser()
parser.add_argument("output", type=pathlib.Path)
parser.add_argument("--seed", type=int, default=20261103)
args = parser.parse_args()
root = args.output
root.mkdir(parents=True, exist_ok=True)
assert not (root / "manifest.json").exists(), "Use a fresh output directory"
raw = root / "downloads"
raw.mkdir(exist_ok=True)

def fetch(name, start, end, destination, etag=None):
    headers = destination.with_suffix(destination.suffix + ".headers")
    command = ["curl", "--noproxy", "*", "--fail", "--silent", "--show-error",
               "--retry", "3", "--max-time", "180", "-H", f"Range: bytes={start}-{end}",
               "-D", str(headers), "-o", str(destination)]
    if etag:
        command += ["-H", f"If-Match: {etag}"]
    subprocess.run(command + [URL + name], check=True)
    text = headers.read_text()
    assert re.search(r"HTTP/\S+ 206", text), text
    assert f"bytes {start}-{end}/" in text, text
    assert destination.stat().st_size == end - start + 1
    current = re.findall(r"(?im)^ETag:\s*(.+)$", text)[-1].strip()
    if etag:
        assert current == etag
    return current

base_etag = fetch("wikipedia_base.bin", 0, 7, raw / "base-header.bin")
query_etag = fetch("wikipedia_query.bin", 0, 7, raw / "query-header.bin")
nb, dimension = struct.unpack("<II", (raw / "base-header.bin").read_bytes())
nq, qdim = struct.unpack("<II", (raw / "query-header.bin").read_bytes())
assert (nb, dimension, nq, qdim) == (35000000, 768, 5000, 768)
rng = np.random.default_rng(args.seed)
block_rows = 1000
# A probability sample of blocks across the entire source, not its first rows.
# Extra blocks allow removal of exact duplicate vectors without shrinking 100k.
blocks = rng.choice(nb // block_rows, size=120, replace=False)
print("Source headers verified; downloading 120 sampled blocks", flush=True)

def download_block(block):
    start = 8 + int(block) * block_rows * dimension * 4
    path = raw / f"block-{block}.f32"
    fetch("wikipedia_base.bin", start, start + block_rows * dimension * 4 - 1, path, base_etag)
    return path

with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
    paths = []
    for i, path in enumerate(pool.map(download_block, map(int, blocks))):
        paths.append(path)
        if (i + 1) % 10 == 0:
            print("Downloaded blocks", i + 1, "/", len(blocks), flush=True)
fetch("wikipedia_query.bin", 8, 8 + nq * dimension * 4 - 1, raw / "queries.f32", query_etag)

def normalize(matrix):
    assert np.isfinite(matrix).all()
    norm = np.linalg.norm(matrix.astype(np.float64), axis=1)
    assert (norm > 0).all()
    return (matrix.astype(np.float64) / norm[:, None]).astype("<f4")

pool = normalize(np.concatenate([np.fromfile(p, dtype="<f4").reshape(-1, dimension) for p in paths]))
source_ids = np.concatenate([np.arange(int(b) * block_rows, (int(b) + 1) * block_rows) for b in blocks])
seen = set()
selected = []
duplicates = 0
for i in rng.permutation(len(pool)):
    key = hashlib.sha256(pool[i].tobytes()).digest()
    if key in seen:
        duplicates += 1
        continue
    seen.add(key)
    selected.append(int(i))
    if len(selected) == 100000:
        break
assert len(selected) == 100000
base = pool[selected]
base.tofile(root / "base.f32")
np.save(root / "base-source-row-ids.npy", source_ids[selected])
query_pool = normalize(np.fromfile(raw / "queries.f32", dtype="<f4").reshape(nq, dimension))
query_rows = []
excluded = 0
for i in rng.permutation(nq):
    key = hashlib.sha256(query_pool[i].tobytes()).digest()
    if key in seen:
        excluded += 1
        continue
    seen.add(key)
    query_rows.append(int(i))
    if len(query_rows) == 220:
        break
assert len(query_rows) == 220
queries = query_pool[query_rows]
queries.tofile(root / "queries.f32")
np.save(root / "query-source-row-ids.npy", np.array(query_rows))

# Compute exact cosine from the FP32 bytes actually passed to both backends.
# Each smaller corpus is a prefix of the shuffled, de-duplicated sample.
q = queries[20:].astype(np.float64)
q /= np.linalg.norm(q, axis=1)[:, None]
truth_checks = []
for count in [2000, 10000, 20000, 50000, 100000]:
    b = base[:count].astype(np.float64)
    b /= np.linalg.norm(b, axis=1)[:, None]
    truth = []
    minimum_gap = float("inf")
    for offset in range(0, 200, 20):
        scores = q[offset:offset + 20] @ b.T
        for score in scores:
            # Stable sorting specifies ID order for exact score ties.
            order = np.argsort(-score, kind="stable")
            truth.append([f"v{i:08}" for i in order[:10]])
            minimum_gap = min(minimum_gap, float(score[order[9]] - score[order[10]]))
    assert len(truth) == 200 and all(len(set(row)) == 10 for row in truth)
    (root / f"truth-{count}.json").write_text(json.dumps(truth))
    # Independent scalar FP64 reference for two queries, full subset search.
    for qi in [0, 199]:
        reference = np.sum(b * q[qi], axis=1)
        ids = [f"v{i:08}" for i in np.argsort(-reference, kind="stable")[:10]]
        assert ids == truth[qi], (count, qi)
    truth_checks.append(dict(count=count, minimum_top10_boundary_gap=minimum_gap,
                             scalar_reference_queries=[0, 199]))
    print("Recomputed exact GT:", count, "minimum boundary gap", minimum_gap, flush=True)

manifest = dict(dataset="Wikipedia-Cohere / Big ANN Benchmarks", source_url=URL,
    numpy_version=np.__version__, rng="PCG64",
    source_definition="https://github.com/harsha-simhadri/big-ann-benchmarks/blob/main/benchmark/datasets.py",
    source_base_rows=nb, source_query_rows=nq, dimension=dimension, seed=args.seed,
    base_etag=base_etag, query_etag=query_etag, block_rows=block_rows, sampled_blocks=blocks.tolist(),
    sampling="120 uniformly sampled blocks of 1000 rows; shuffle; take first 100k unique normalized vectors",
    query_sampling="220 shuffled unique vectors from the separate Simple Wikipedia query file; exclude exact base matches",
    normalization="float64 norm and division, rounded to float32; exact cosine GT re-normalizes stored FP32 in float64",
    metric="cosine", source_benchmark_metric="inner product", counts=[2000,10000,20000,50000,100000],
    warmup=20, measured_queries=200, top_k=10, duplicates_skipped_before_100k=duplicates,
    query_duplicates_or_exact_base_matches_skipped=excluded, query_source_row_ids=query_rows,
    gt_checks=truth_checks, files={})
for path in [root / "base.f32", root / "queries.f32", root / "base-source-row-ids.npy", root / "query-source-row-ids.npy", *root.glob("truth-*.json")]:
    manifest["files"][path.name] = dict(bytes=path.stat().st_size, sha256=hashlib.sha256(path.read_bytes()).hexdigest())
(root / "manifest.json").write_text(json.dumps(manifest, indent=2))
print("READY", root, flush=True)
