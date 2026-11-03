"""Create independent Cohere validation queries and exact per-subset cosine GT."""
import argparse
import hashlib
import json
import pathlib
import numpy as np

parser = argparse.ArgumentParser()
parser.add_argument("--data", type=pathlib.Path, required=True)
parser.add_argument("--output", type=pathlib.Path, required=True)
parser.add_argument("--seed", type=int, default=20261104)
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=False)
manifest = json.loads((args.data / "manifest.json").read_text())
for name, info in manifest["files"].items():
    with (args.data / name).open("rb") as file:
        assert hashlib.file_digest(file, "sha256").hexdigest() == info["sha256"], name
base = np.fromfile(args.data / "base.f32", dtype="<f4").reshape(-1, 768)
source = np.fromfile(args.data / "downloads/queries.f32", dtype="<f4").reshape(5000, 768)
norms = np.linalg.norm(source.astype(np.float64), axis=1)
assert np.isfinite(source).all() and (norms > 0).all()
source = (source.astype(np.float64) / norms[:, None]).astype("<f4")
old_queries = np.fromfile(args.data / "queries.f32", dtype="<f4").reshape(220, 768)
old_ids = manifest["query_source_row_ids"]
assert np.array_equal(source[old_ids], old_queries)
seen = {hashlib.sha256(v.tobytes()).digest() for v in base}
seen.update(hashlib.sha256(v.tobytes()).digest() for v in old_queries)
selected = []
for row in np.random.default_rng(args.seed).permutation(5000):
    key = hashlib.sha256(source[row].tobytes()).digest()
    if key in seen:
        continue
    seen.add(key)
    selected.append(int(row))
    if len(selected) == 220:
        break
assert len(selected) == 220 and not set(selected).intersection(old_ids)
queries = source[selected]
queries.tofile(args.output / "queries.f32")
q = queries[20:].astype(np.float64)
q /= np.linalg.norm(q, axis=1)[:, None]
checks = []
for count in manifest["counts"]:
    b = base[:count].astype(np.float64)
    b /= np.linalg.norm(b, axis=1)[:, None]
    truth, gap = [], float("inf")
    for offset in range(0, 200, 20):
        for scores in q[offset:offset + 20] @ b.T:
            order = np.argsort(-scores, kind="stable")
            truth.append([f"v{i:08}" for i in order[:10]])
            gap = min(gap, float(scores[order[9]] - scores[order[10]]))
    for qi in [0, 199]:
        scalar = np.argsort(-np.sum(b * q[qi], axis=1), kind="stable")[:10]
        assert truth[qi] == [f"v{i:08}" for i in scalar]
    (args.output / f"truth-{count}.json").write_text(json.dumps(truth))
    checks.append(dict(count=count, minimum_top10_boundary_gap=gap,
                       scalar_reference_queries=[0, 199]))
    print("VALIDATION GT", count, flush=True)
metadata = dict(seed=args.seed, rng="PCG64", numpy_version=np.__version__,
                source_dataset_manifest_sha256=hashlib.sha256((args.data / "manifest.json").read_bytes()).hexdigest(),
                source_query_file_sha256=hashlib.sha256((args.data / "downloads/queries.f32").read_bytes()).hexdigest(),
                query_source_row_ids=selected, warmup=20, measured_queries=200,
                independent_of_test_queries=True, exact_base_matches_excluded=True,
                counts=manifest["counts"], gt_checks=checks, files={})
for path in args.output.iterdir():
    metadata["files"][path.name] = dict(bytes=path.stat().st_size,
        sha256=hashlib.sha256(path.read_bytes()).hexdigest())
(args.output / "manifest.json").write_text(json.dumps(metadata, indent=2))
print("VALIDATION READY", flush=True)
