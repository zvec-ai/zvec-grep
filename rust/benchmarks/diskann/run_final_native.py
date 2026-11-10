"""Final-branch native DiskANN/HNSW comparison; immutable one-pass test shards."""
import argparse
import hashlib
import json
import os
import re
import statistics
import subprocess
from datetime import datetime, timezone
from pathlib import Path

COUNTS = [200000, 400000, 600000, 800000, 1000000]
HERE = Path(__file__).resolve().parent

def sha(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()

def save(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")

def now():
    return datetime.now(timezone.utc).isoformat()

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("root", type=Path)
    parser.add_argument("--stage", choices=["build", "validation", "measure"], required=True)
    parser.add_argument("--source", type=Path, required=True)
    args = parser.parse_args()
    root = args.root.resolve()
    data = root / "data"
    binary = root / "native-target/release/native-compare"
    builder = root / "native-target/release/graph-build"
    library = root / "build/lib/libzvec_c_api.dylib"
    assert library.is_file() and binary.is_file() and builder.is_file()
    env = {k: v for k, v in os.environ.items() if not k.startswith("ZVEC_")}
    env.update(ZVEC_BENCH_EXPECT_LIBRARY=str(library), ZVEC_DISKANN_READ_CACHE_MIB="8",
               ZVEC_DISKANN_SCALAR_CACHE_MIB="1", ZVEC_DISKANN_READ_WORKERS="24",
               ZVEC_BENCH_BUILD_DEGREE="64", ZVEC_BENCH_BUILD_LIST="200")
    manifest = json.loads((data / "manifest.json").read_text())
    assert manifest["base_sha256"] == "cfdb2041e2bde4d93d5f2f75feb949701298817a51147512895da3c05ce03d2b"
    for name, info in manifest["files"].items():
        assert sha(data/name) == info["sha256"], name
    assert not subprocess.check_output(["git", "-C", str(args.source), "status", "--porcelain"], text=True).strip(), "Commit the final C++ source first"
    configuration = dict(source_commit=subprocess.check_output(["git", "-C", str(args.source), "rev-parse", "HEAD"], text=True).strip(),
        source_branch=subprocess.check_output(["git", "-C", str(args.source), "branch", "--show-current"], text=True).strip(),
        library=dict(path=str(library), sha256=sha(library)),
        binary=dict(path=str(binary), sha256=sha(binary)),
        builder=dict(path=str(builder), sha256=sha(builder)),
        runner_sha256=sha(Path(__file__)), manifest_sha256=sha(data/"manifest.json"),
        env={k:v for k,v in env.items() if k.startswith("ZVEC_")},
        counts=COUNTS, precision="FP16", metric="cosine", top_k=10,
        diskann_build=dict(degree=64, construction_list=200, pq_bytes=64),
        hnsw_build=dict(m=50, ef_construction=500),
        build_workers=8, query_workers=1, concurrent_queries=1,
        validation_recall_target=.92, formal_recall_floor=.90,
        warmup_queries=600, validation_queries=400, test_shards=7, test_queries_per_shard=500,
        test_passes=1, test_warmup_passes=0,
        cpu=subprocess.check_output(["sysctl", "-n", "machdep.cpu.brand_string"], text=True).strip(),
        physical_memory_bytes=int(subprocess.check_output(["sysctl", "-n", "hw.memsize"], text=True)),
        os=subprocess.check_output(["sw_vers"], text=True))
    config_path = root / "configuration.json"
    if config_path.exists():
        assert json.loads(config_path.read_text()) == configuration, "Frozen benchmark inputs changed"
    else:
        save(config_path, configuration)
    out = root / args.stage
    out.mkdir(exist_ok=True)
    (out / "raw").mkdir(exist_ok=True)
    indexes = root / "indexes"
    indexes.mkdir(exist_ok=True)

    def execute(command, name, status):
        log = out / "raw" / f"{name}.jsonl"
        err = log.with_suffix(".stderr")
        done = log.with_suffix(".complete.json")
        if done.exists():
            record = json.loads(done.read_text())
            assert sha(log) == record["stdout_sha256"] and sha(err) == record["stderr_sha256"]
            return log, err
        assert not log.exists(), f"Incomplete invocation preserved: {log}; do not silently repeat a test query"
        print(json.dumps(dict(started=now(), **status)), flush=True)
        save(root / "status.json", dict(stage=args.stage, started=now(), **status))
        with log.open("w") as stdout, err.open("w") as stderr:
            subprocess.run(["/usr/bin/time", "-l", *map(str, command)], env=env,
                           stdout=stdout, stderr=stderr, check=True)
        save(done, dict(stdout_sha256=sha(log), stderr_sha256=sha(err), finished=now()))
        return log, err

    def parse(log, err):
        records = [json.loads(s) for s in log.read_text().splitlines() if s.startswith("{")]
        loaded = next(r["path"] for r in records if r.get("stage") == "library")
        assert Path(loaded).resolve() == library.resolve()
        rss = int(re.search(r"(\d+)\s+maximum resident set size", err.read_text()).group(1))/2**20
        return records, rss

    if args.stage == "build":
        rows = []
        for n in COUNTS:
            for backend in ["diskann", "hnsw"]:
                name = f"{backend}-{n}"
                log, err = execute([builder, "build", backend, indexes/name, data/"base.f32", n, 80, 1], name,
                                   dict(backend=backend, count=n))
                records, rss = parse(log, err)
                row = next(r for r in records if r.get("stage") == "built")
                row.update(rss_mib=rss, raw_file=str(log.relative_to(root)))
                rows.append(row)
                save(out/"runs.json", rows)
        print("ALL INDEXES BUILT", flush=True)
        return

    def invoke(backend, n, effort, shard, phase):
        query_name = "validation" if phase == "validation" else f"test-{shard}"
        name = f"{backend}-{n}-{query_name}-L{effort}"
        log, err = execute([binary, backend, indexes/f"{backend}-{n}", data/"warmup.f32",
                            data/f"{query_name}.f32", n, effort], name,
                           dict(backend=backend, count=n, effort=effort, shard=shard))
        records, rss = parse(log, err)
        row = next(r for r in records if r.get("stage") == "queried")
        ids = next(r["ids"] for r in records if r.get("stage") == "results")
        truth = json.loads((data/f"{query_name}-truth-{n}.json").read_text())
        expected = 400 if phase == "validation" else 500
        assert row["test_passes"] == 1 and row["test_warmup_passes"] == 0 and row["warmup_queries"] == 600
        assert len(ids) == len(truth) == row["queries"] == expected
        assert all(len(set(x)) == 10 for x in ids)
        assert all(all(0 <= int(pk[1:]) < n for pk in x) for x in ids)
        hits = sum(len(set(a)&set(b)) for a,b in zip(ids, truth))
        row.update(recall_hits=hits, recall_at_10=hits/(expected*10), rss_mib=rss,
                   shard=shard, phase=phase, raw_file=str(log.relative_to(root)))
        return row

    if args.stage == "validation":
        rows, selection = [], {}
        for n in COUNTS:
            for backend in ["hnsw", "diskann"]:
                efforts = [16,24,32,48,64,80,96,128,192] if backend == "hnsw" else [32,48,64,80,96,112,128,160,192]
                for effort in efforts:
                    row = invoke(backend, n, effort, -1, "validation")
                    rows.append(row)
                    save(out/"runs.json", rows)
                    print("VALIDATION", backend, n, effort, row["recall_at_10"], row["qps"], flush=True)
                    if row["recall_at_10"] >= .92:
                        selection[f"{backend}-{n}"] = dict(effort=effort, recall_at_10=row["recall_at_10"],
                                                         rule="first ascending effort with validation Recall@10 >= 92%")
                        break
                else:
                    raise RuntimeError(f"No acceptable validation candidate: {backend} {n}")
        save(root/"selection.json", selection)
        print("PARAMETERS FROZEN", flush=True)
        return

    selection = json.loads((root/"selection.json").read_text())
    selection_sha = sha(root/"selection.json")
    inventory = lambda: {str(p.relative_to(indexes)): [p.stat().st_size,p.stat().st_mtime_ns]
                         for p in indexes.rglob("*") if p.is_file()}
    before = inventory()
    rows = []
    for shard in range(7):
        for i,n in enumerate(COUNTS if shard%2 == 0 else COUNTS[::-1]):
            for backend in (["diskann", "hnsw"] if (shard+i)%2 == 0 else ["hnsw", "diskann"]):
                row = invoke(backend, n, selection[f"{backend}-{n}"]["effort"], shard, "measure")
                rows.append(row)
                save(out/"runs.json", rows)
                print("MEASURED", backend, n, shard, row["qps"], row["rss_mib"], row["recall_at_10"], flush=True)
    summary = []
    for n in COUNTS:
        for backend in ["hnsw", "diskann"]:
            g = [r for r in rows if r["count"] == n and r["backend"] == backend]
            assert len(g) == 7 and len({r["shard"] for r in g}) == 7
            latencies = [t for r in g for t in r["latencies_ms"]]
            summary.append(dict(count=n, backend=backend, effort=selection[f"{backend}-{n}"]["effort"],
                queries=sum(r["queries"] for r in g), processes=len(g),
                qps=sum(r["queries"] for r in g)/sum(r["batch_s"] for r in g),
                qps_min=min(r["qps"] for r in g), qps_max=max(r["qps"] for r in g),
                rss_mib=statistics.median(r["rss_mib"] for r in g),
                rss_min=min(r["rss_mib"] for r in g), rss_max=max(r["rss_mib"] for r in g),
                recall_at_10=sum(r["recall_hits"] for r in g)/35000,
                recall_min=min(r["recall_at_10"] for r in g),
                p50_ms=statistics.median(latencies), p95_ms=sorted(latencies)[int(len(latencies)*.95)]))
    assert inventory() == before, "Read-only query changed an index"
    assert sha(library) == configuration["library"]["sha256"] and sha(binary) == configuration["binary"]["sha256"]
    assert sha(root/"selection.json") == selection_sha and sha(Path(__file__)) == configuration["runner_sha256"]
    save(out/"summary.json", summary)
    save(out/"audit.json", dict(finished=now(), invocations=len(rows), unique_test_queries_per_backend_scale=3500,
        all_shards_above_90=all(r["recall_at_10"] >= .90 for r in rows),
        indexes_unchanged=True, binary_library_unchanged=True, selection_sha256=selection_sha))
    save(root/"status.json", dict(stage="complete", finished=now()))
    print("MEASUREMENT COMPLETE", flush=True)

if __name__ == "__main__":
    main()
