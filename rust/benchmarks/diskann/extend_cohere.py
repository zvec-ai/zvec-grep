"""Extend the existing 100k sample to one million; preserve queries and recompute GT."""
import argparse
import concurrent.futures
import hashlib
import json
import pathlib
import re
import shutil
import subprocess
import numpy as np

COUNTS = [200000, 400000, 600000, 800000, 1000000]
parser = argparse.ArgumentParser()
parser.add_argument("--previous-data", type=pathlib.Path, required=True)
parser.add_argument("--previous-validation", type=pathlib.Path, required=True)
parser.add_argument("--output", type=pathlib.Path, required=True)
parser.add_argument("--seed", type=int, default=20261105)
parser.add_argument("--download-workers", type=int, default=12)
args = parser.parse_args()
assert 1 <= args.download_workers <= 16
root = args.output
root.mkdir(parents=True, exist_ok=True)
assert not (root / "manifest.json").exists(), "Completed data is immutable; use another output directory"
old = json.loads((args.previous_data / "manifest.json").read_text())
old_val = json.loads((args.previous_validation / "manifest.json").read_text())
assert old["dimension"] == 768 and max(old["counts"]) == 100000
for folder, manifest in [(args.previous_data, old), (args.previous_validation, old_val)]:
    for name, info in manifest["files"].items():
        with (folder / name).open("rb") as file:
            assert hashlib.file_digest(file, "sha256").hexdigest() == info["sha256"], name
raw = root / "downloads"
raw.mkdir(exist_ok=True)
validation = root / "validation"
validation.mkdir(exist_ok=True)
shutil.copy2(args.previous_data / "queries.f32", root / "queries.f32")
shutil.copy2(args.previous_data / "query-source-row-ids.npy", root / "query-source-row-ids.npy")
shutil.copy2(args.previous_validation / "queries.f32", validation / "queries.f32")
rng = np.random.default_rng(args.seed)
available = np.setdiff1d(np.arange(35000), old["sampled_blocks"])
blocks = rng.choice(available, size=1100, replace=False)
plan = dict(seed=args.seed, blocks=blocks.tolist(), base_etag=old["base_etag"],
            previous_manifest_sha256=hashlib.sha256((args.previous_data / "manifest.json").read_bytes()).hexdigest())
plan_path = root / "download-plan.json"
if plan_path.exists():
    assert json.loads(plan_path.read_text()) == plan, "Resume requires the identical sampling plan"
plan_path.write_text(json.dumps(plan, indent=2))

def fetch(block):
    start = 8 + int(block) * 1000 * 768 * 4
    end = start + 1000 * 768 * 4 - 1
    path = raw / f"block-{block}.f32"
    headers = path.with_suffix(".headers")
    if not (path.exists() and headers.exists() and path.stat().st_size == end-start+1):
        subprocess.run(["curl", "--noproxy", "*", "--fail", "--silent", "--show-error",
            "--retry", "3", "--max-time", "180", "-H", f"Range: bytes={start}-{end}",
            "-H", f"If-Match: {old['base_etag']}", "-D", str(headers), "-o", str(path),
            old["source_url"] + "wikipedia_base.bin"], check=True)
    text = headers.read_text()
    assert re.search(r"HTTP/\S+ 206", text) and f"bytes {start}-{end}/" in text
    assert re.findall(r"(?im)^ETag:\s*(.+)$", text)[-1].strip() == old["base_etag"]
    assert path.stat().st_size == end-start+1
    return path

print("DOWNLOAD", len(blocks), "new source blocks", flush=True)
with concurrent.futures.ThreadPoolExecutor(max_workers=args.download_workers) as workers:
    paths = []
    for i, path in enumerate(workers.map(fetch, map(int, blocks))):
        paths.append(path)
        if (i+1) % 25 == 0:
            print("DOWNLOADED", i+1, "/", len(blocks), flush=True)
pool = np.memmap(root / "candidate-pool.f32", mode="w+", dtype="<f4", shape=(len(blocks)*1000, 768))
for i, path in enumerate(paths):
    values = np.fromfile(path, dtype="<f4").reshape(1000, 768).astype(np.float64)
    norms = np.linalg.norm(values, axis=1)
    assert np.isfinite(values).all() and (norms > 0).all()
    pool[i*1000:(i+1)*1000] = (values / norms[:, None]).astype("<f4")
pool.flush()
seen = set()
with (args.previous_data / "base.f32").open("rb") as file:
    while chunk := file.read(768*4):
        seen.add(hashlib.sha256(chunk).digest())
query_keys = set()
for folder in [root, validation]:
    for row in np.fromfile(folder / "queries.f32", dtype="<f4").reshape(220, 768):
        query_keys.add(hashlib.sha256(row.tobytes()).digest())
assert not seen.intersection(query_keys)
source_ids = np.load(args.previous_data / "base-source-row-ids.npy").tolist()
shutil.copyfile(args.previous_data / "base.f32", root / "base.f32")
duplicates, query_matches = 0, 0
with (root / "base.f32").open("ab") as output:
    for i in rng.permutation(len(pool)):
        vector = pool[i].tobytes()
        key = hashlib.sha256(vector).digest()
        if key in query_keys:
            query_matches += 1
            continue
        if key in seen:
            duplicates += 1
            continue
        seen.add(key)
        output.write(vector)
        source_ids.append(int(blocks[i // 1000]) * 1000 + int(i % 1000))
        if len(source_ids) % 100000 == 0:
            print("SELECTED", len(source_ids), flush=True)
        if len(source_ids) == 1000000:
            break
assert len(source_ids) == 1000000
np.save(root / "base-source-row-ids.npy", np.asarray(source_ids, dtype=np.int64))
del pool, seen, source_ids
with (root / "base.f32").open("rb") as file:
    prefix_hash = hashlib.sha256(file.read(100000*768*4)).hexdigest()
assert prefix_hash == old["files"]["base.f32"]["sha256"]

# One streaming exhaustive search, checkpointed at every requested prefix.
# Keep the best eleven scores/IDs, using ordinal IDs as deterministic tie breaks.
queries = np.concatenate([np.fromfile(folder / "queries.f32", dtype="<f4").reshape(220,768)[20:]
                          for folder in [root, validation]]).astype(np.float64)
queries /= np.linalg.norm(queries, axis=1)[:, None]
scores_best = [np.empty(0) for _ in queries]
ids_best = [np.empty(0, dtype=np.int64) for _ in queries]
reference_queries = [0, 199, 200, 399]
ref_scores = {i: np.empty(0) for i in reference_queries}
ref_ids = {i: np.empty(0, dtype=np.int64) for i in reference_queries}
checks = []

def merge_top(scores, ids, previous_scores, previous_ids):
    threshold = np.partition(scores, -11)[-11]
    local = np.flatnonzero(scores >= threshold)
    local = local[np.lexsort((ids[local], -scores[local]))[:11]]
    combined_scores = np.concatenate([previous_scores, scores[local]])
    combined_ids = np.concatenate([previous_ids, ids[local]])
    order = np.lexsort((combined_ids, -combined_scores))[:11]
    return combined_scores[order], combined_ids[order]

with (root / "base.f32").open("rb") as file:
    for offset in range(0, 1000000, 4000):
        b = np.fromfile(file, dtype="<f4", count=4000*768).reshape(4000,768).astype(np.float64)
        b /= np.linalg.norm(b, axis=1)[:, None]
        scores = queries @ b.T
        ids = np.arange(offset, offset+4000)
        for qi, score in enumerate(scores):
            scores_best[qi], ids_best[qi] = merge_top(score, ids, scores_best[qi], ids_best[qi])
        for qi in reference_queries:
            scalar = np.sum(b * queries[qi], axis=1)
            ref_scores[qi], ref_ids[qi] = merge_top(scalar, ids, ref_scores[qi], ref_ids[qi])
        count = offset+4000
        if count % 100000 == 0:
            print("EXHAUSTIVE GT SCANNED", count, flush=True)
        if count not in [100000, *COUNTS]:
            continue
        for qi in reference_queries:
            assert np.array_equal(ref_ids[qi][:10], ids_best[qi][:10]), (count, qi)
        for group, (folder, previous) in enumerate([(root,args.previous_data),(validation,args.previous_validation)]):
            truth = [[f"v{i:08}" for i in ids_best[qi][:10]] for qi in range(group*200,(group+1)*200)]
            assert len(truth)==200 and all(len(set(row))==10 for row in truth)
            if count == 100000:
                assert truth == json.loads((previous / "truth-100000.json").read_text()), "GT disagrees with original 100k corpus"
            (folder / f"truth-{count}.json").write_text(json.dumps(truth))
        checks.append(dict(count=count, minimum_top10_boundary_gap=min(s[9]-s[10] for s in scores_best),
                           scalar_reference_queries=reference_queries, original_100k_gt_matches=count==100000))
        print("EXACT GT VERIFIED", count, flush=True)

def file_info(path):
    with path.open("rb") as f:
        digest = hashlib.file_digest(f,"sha256").hexdigest()
    return dict(bytes=path.stat().st_size, sha256=digest)

manifest = dict(dataset=old["dataset"], source_url=old["source_url"], source_definition=old["source_definition"],
    source_base_rows=35000000, source_query_rows=5000, dimension=768, metric="cosine",
    source_benchmark_metric="inner product", base_etag=old["base_etag"], query_etag=old["query_etag"],
    seed=args.seed, numpy_version=np.__version__, rng="PCG64", block_rows=1000,
    preserved_prefix_count=100000, preserved_prefix_sha256=prefix_hash,
    previous_manifest_sha256=plan["previous_manifest_sha256"],
    previous_sampled_blocks=old["sampled_blocks"], additional_sampled_blocks=blocks.tolist(),
    sampling="preserve original 100k; sample 1100 new blocks excluding old blocks; globally shuffle additional candidates; append 900k unique normalized vectors",
    query_sampling="unchanged original test and validation queries; exclude exact query matches from added base vectors",
    duplicates_skipped_in_extension=duplicates, query_matches_skipped_in_extension=query_matches,
    normalization=old["normalization"], counts=COUNTS, warmup=20, measured_queries=200, top_k=10,
    query_source_row_ids=old["query_source_row_ids"],
    gt_method="exhaustive FP64 cosine in 4000-row blocks; exact top-11 merge with ID tie breaks; checkpoint each prefix",
    gt_checks=checks, files={})
for path in [root/"base.f32",root/"queries.f32",root/"base-source-row-ids.npy",root/"query-source-row-ids.npy",*root.glob("truth-*.json")]:
    manifest["files"][path.name] = file_info(path)
(root/"manifest.json").write_text(json.dumps(manifest,indent=2))
vm = dict(seed=old_val["seed"], query_source_row_ids=old_val["query_source_row_ids"],
    source_dataset_manifest_sha256=hashlib.sha256((root/"manifest.json").read_bytes()).hexdigest(),
    original_validation_manifest_sha256=hashlib.sha256((args.previous_validation/"manifest.json").read_bytes()).hexdigest(),
    counts=COUNTS, warmup=20, measured_queries=200, independent_of_test_queries=True,
    gt_checks=checks, files={p.name:file_info(p) for p in [validation/"queries.f32",*validation.glob("truth-*.json")]})
(validation/"manifest.json").write_text(json.dumps(vm,indent=2))
print("MILLION DATA AND GT READY",flush=True)
