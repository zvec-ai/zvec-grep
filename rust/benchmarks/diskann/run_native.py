"""Fresh zvec DiskANN/HNSW indexes, independent tuning, paired RSS/QPS runs.

Resume only completed stages with identical inputs. Partial indexes are retained
for diagnosis and require a new output directory; they are never silently reused.
"""
import argparse
from datetime import datetime
import hashlib
import json
import mmap
import os
import pathlib
import re
import shutil
import statistics
import subprocess
from zoneinfo import ZoneInfo

COUNTS = [200000, 400000, 600000, 800000, 1000000]
ENGINES = ["hnsw", "diskann"]
GRID = [10, 12, 16, 20, 24, 32, 40, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048]


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def save(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def now():
    return datetime.now(ZoneInfo("Asia/Shanghai")).isoformat()


def verify_data(data, output):
    import numpy as np

    manifest = json.loads((data / "manifest.json").read_text())
    validation = json.loads((data / "validation/manifest.json").read_text())
    assert manifest["counts"] == validation["counts"] == COUNTS
    assert not set(manifest["query_source_row_ids"]) & set(validation["query_source_row_ids"])
    assert validation["source_dataset_manifest_sha256"] == sha(data / "manifest.json")
    for folder, meta in [(data, manifest), (data / "validation", validation)]:
        for name, info in meta["files"].items():
            path = folder / name
            assert path.stat().st_size == info["bytes"] and sha(path) == info["sha256"], path
    verification = output / "data-audit.json"
    if verification.exists():
        assert json.loads(verification.read_text())["manifest_sha256"] == sha(data / "manifest.json")
        return
    # Recompute exact GT for the SAME nested corpus, with a different block size
    # from the original preparation. Only the 200 measured queries enter Recall.
    queries = np.concatenate([np.fromfile(folder / "queries.f32", dtype="<f4").reshape(220, 768)[20:]
                              for folder in [data, data / "validation"]]).astype(np.float64)
    queries /= np.linalg.norm(queries, axis=1)[:, None]
    best_scores = np.full((400, 10), -np.inf)
    best_ids = np.full((400, 10), -1, dtype=np.int64)
    base = np.memmap(data / "base.f32", dtype="<f4", mode="r", shape=(1000000, 768))
    audits = []
    for first in range(0, 1000000, 2000):
        block = base[first:first + 2000].astype(np.float64)
        block /= np.linalg.norm(block, axis=1)[:, None]
        scores = queries @ block.T
        for qi, row in enumerate(scores):
            threshold = np.partition(row, -10)[-10]
            candidates = np.flatnonzero(row >= threshold)
            candidate_scores = np.concatenate([best_scores[qi], row[candidates]])
            candidate_ids = np.concatenate([best_ids[qi], candidates + first])
            order = np.lexsort((candidate_ids, -candidate_scores))[:10]
            best_scores[qi] = candidate_scores[order]
            best_ids[qi] = candidate_ids[order]
        count = first + 2000
        if count in COUNTS:
            for group, folder in enumerate([data, data / "validation"]):
                truth = [[f"v{i:08}" for i in ids] for ids in best_ids[group * 200:(group + 1) * 200]]
                assert truth == json.loads((folder / f"truth-{count}.json").read_text()), (count, group)
            audits.append(dict(count=count, test_and_validation_gt_match=True))
            print("GT VERIFIED", count, flush=True)
    save(verification, dict(manifest_sha256=sha(data / "manifest.json"),
         all_input_hashes_verified=True, independent_validation_queries=True,
         method="recomputed exhaustive FP64 cosine, 2000-row blocks, deterministic ID tie breaks",
         counts=audits, finished=now()))
    shutil.copy2(data / "manifest.json", output / "dataset.json")
    shutil.copy2(data / "validation/manifest.json", output / "validation-dataset.json")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=pathlib.Path, required=True)
    parser.add_argument("--data", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--native-library", type=pathlib.Path, required=True)
    parser.add_argument("--source-root", type=pathlib.Path, required=True)
    args = parser.parse_args()
    binary, data, output = args.binary.resolve(), args.data.resolve(), args.output.resolve()
    library = args.native_library.resolve()
    source = args.source_root.resolve()
    source_commit = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    source_diff = subprocess.check_output(["git", "-C", str(source), "diff", "HEAD", "--", ".", ":(exclude)thirdparty"])
    assert not source_diff, "Benchmark requires a clean local zvec source checkout"
    assert library.name == "libzvec_c_api.dylib" and library.is_file()
    build_cache = library.parent.parent / "CMakeCache.txt"
    cache_text = build_cache.read_text()
    cache = dict(re.findall(r"^([^/#\r\n][^:=\r\n]*):[^=\r\n]+=([^\r\n]*)$", cache_text, re.MULTILINE))
    assert pathlib.Path(cache["CMAKE_HOME_DIRECTORY"]).resolve() == source
    assert cache["CMAKE_BUILD_TYPE"] == "Release"
    submodules = subprocess.check_output(["git", "-C", str(source), "submodule", "status"], text=True)
    assert all(line.startswith(" ") for line in submodules.splitlines()), "Source submodules must match pinned commits"
    assert sha(binary.parent / library.name) == sha(library), "Harness must carry the locally built native library"
    linked = subprocess.check_output(["otool", "-L", str(binary)], text=True)
    dependencies = [line.strip().split(" (compatibility", 1)[0] for line in linked.splitlines()[1:]]
    assert any(pathlib.Path(path).is_absolute() and pathlib.Path(path).resolve() == library for path in dependencies), "Link native library by absolute path to prevent SDK fallback"
    native_environment = {key: value for key, value in os.environ.items()
                          if key not in {"DYLD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES"}}
    output.mkdir(parents=True, exist_ok=True)
    for folder in ["indexes", "builds", "validation", "measurements"]:
        (output / folder).mkdir(exist_ok=True)
    config = dict(binary_sha256=sha(binary), data_manifest_sha256=sha(data / "manifest.json"),
                  script_sha256=sha(pathlib.Path(__file__)), counts=COUNTS, rust_binding_version="0.8.0",
                  zvec_source_commit=source_commit, native_library_sha256=sha(library), local_source_build=True,
                  native_library_linkage="absolute Mach-O dependency, verified with otool",
                  native_build_cache_sha256=sha(build_cache),
                  harness_source_sha256=sha(pathlib.Path(__file__).parent / "harness/src/bin/native-bench.rs"),
                  precision="FP16", metric="cosine", top_k=10, query_threads=1, build_threads=8,
                  hnsw=dict(m=50, ef_construction=500), diskann=dict(max_degree=32, build_list=100, pq_chunks=64),
                  candidate_grid=GRID, validation_threshold=0.92, test_target=0.90,
                  warmup_queries=20, measured_distinct_queries=200, measured_passes=5, repeats=6)
    config_file = output / "configuration.json"
    if config_file.exists():
        assert json.loads(config_file.read_text()) == config, "Inputs changed; use a fresh output directory"
    save(config_file, config)
    save(output / "native-build.json", dict(source_commit=source_commit, submodule_status=submodules,
         cmake_cache={key: value for key, value in cache.items() if key.startswith(("CMAKE_C_", "CMAKE_CXX_", "BUILD_", "USE_OSS", "ZVEC_ENABLE_LTO", "AUTO_DETECT_ARCH"))},
         compiler=subprocess.check_output([cache["CMAKE_CXX_COMPILER"], "--version"], text=True).strip()))

    def status(stage, **details):
        value = dict(stage=stage, updated=now(), **details)
        save(output / "status.json", value)
        print(json.dumps(value), flush=True)

    status("verify_data_and_exact_gt")
    verify_data(data, output)
    builds = []
    for count in COUNTS:
        for engine in ENGINES:
            name = f"{engine}-{count}"
            index = output / "indexes" / name
            marker = output / "builds" / f"{name}.json"
            if not marker.exists():
                assert not index.exists(), f"Incomplete index retained: {index}; use a new output directory"
                assert shutil.disk_usage(output).free > 6 * 2**30
                status("build", engine=engine, count=count)
                with (output / "builds" / f"{name}.jsonl").open("w") as out, (output / "builds" / f"{name}.stderr").open("w") as err:
                    subprocess.run(["/usr/bin/time", "-l", str(binary), "build", engine, str(index),
                                    str(data / "base.f32"), str(count), "0", "1"], stdout=out, stderr=err, check=True, env=native_environment)
                stages = [json.loads(line) for line in (output / "builds" / f"{name}.jsonl").read_text().splitlines()]
                assert stages[-1]["stage"] == "built" and stages[-1]["count"] == count
                files = [p for p in index.rglob("*") if p.is_file()]
                native_files = []
                if engine == "diskann":
                    for path in files:
                        if path.suffix == ".proxima":
                            with path.open("rb") as stream, mmap.mmap(stream.fileno(), 0, access=mmap.ACCESS_READ) as mapping:
                                if mapping.find(b"diskann.pq_data") >= 0 and mapping.find(b"diskann.vector") >= 0:
                                    native_files.append(str(path.relative_to(index)))
                    assert native_files, "Expected native DiskANN/PQ segments, not a flat-only collection"
                save(marker, dict(**stages[-1], total_file_bytes=sum(p.stat().st_size for p in files),
                                  native_diskann_pq_files=native_files, binary_sha256=config["binary_sha256"]))
            assert index.is_dir()
            builds.append(json.loads(marker.read_text()))
    save(output / "build-audit.json", builds)

    def query(engine, count, effort, folder, name, queries, passes):
        with (folder / f"{name}.jsonl").open("w") as out, (folder / f"{name}.stderr").open("w") as err:
            subprocess.run(["/usr/bin/time", "-l", str(binary), "query", engine,
                            str(output / "indexes" / f"{engine}-{count}"), str(queries), str(count), str(effort), str(passes)],
                           stdout=out, stderr=err, check=True, env=native_environment)
        stages = [json.loads(line) for line in (folder / f"{name}.jsonl").read_text().splitlines()]
        result = next(s for s in stages if s["stage"] == "queried")
        found = next(s["ids"] for s in stages if s["stage"] == "results")
        truth = json.loads((queries.parent / f"truth-{count}.json").read_text())
        assert len(found) == len(truth) == 200
        assert all(len(row) == len(set(row)) == 10 for row in found)
        assert all(0 <= int(doc.removeprefix("v")) < count for row in found for doc in row)
        result["recall_at_10"] = sum(len(set(a) & set(b)) for a, b in zip(found, truth)) / 2000
        stderr = (folder / f"{name}.stderr").read_text()
        result["rss_mib"] = int(re.search(r"(\d+)\s+maximum resident set size", stderr).group(1)) / 2**20
        result["raw_file"] = str((folder / f"{name}.jsonl").relative_to(output))
        result["finished"] = now()
        return result

    selection_file = output / "selection.json"
    selection = json.loads(selection_file.read_text()) if selection_file.exists() else dict(trials=[], selected=[])
    for count in COUNTS:
        for engine in ENGINES:
            if any(r["engine"] == engine and r["count"] == count for r in selection["selected"]):
                continue
            for effort in GRID:
                status("tune_validation", engine=engine, count=count, effort=effort)
                row = query(engine, count, effort, output / "validation", f"{engine}-{count}-{effort}", data / "validation/queries.f32", 1)
                selection["trials"].append(row)
                if row["recall_at_10"] >= .92:
                    selection["selected"].append(row)
                    save(selection_file, selection)
                    break
                save(selection_file, selection)
            else:
                raise RuntimeError(f"No parameter meets validation threshold: {engine}/{count}")
    selection["frozen_before_test"] = True
    save(selection_file, selection)
    status("parameters_frozen")
    runs_file = output / "runs.json"
    runs = json.loads(runs_file.read_text()) if runs_file.exists() else []
    # Rotate scale order as well as engine order. No build or GT computation overlaps.
    for repeat in range(6):
        for count in (COUNTS if repeat % 2 == 0 else list(reversed(COUNTS))):
            for engine in (ENGINES if repeat % 2 == 0 else list(reversed(ENGINES))):
                if any(r["engine"] == engine and r["count"] == count and r["repeat"] == repeat for r in runs):
                    continue
                effort = next(r["effort"] for r in selection["selected"] if r["engine"] == engine and r["count"] == count)
                status("measure", engine=engine, count=count, repeat=repeat, effort=effort)
                row = query(engine, count, effort, output / "measurements", f"{engine}-{count}-{repeat}", data / "queries.f32", 5)
                row["repeat"] = repeat
                runs.append(row)
                save(runs_file, runs)
                assert row["recall_at_10"] >= .90, "Held-out target missed; do not retune on test queries"
    summary = []
    for count in COUNTS:
        for engine in ENGINES:
            group = [r for r in runs if r["count"] == count and r["engine"] == engine]
            assert len(group) == 6
            row = dict(count=count, engine=engine, effort=group[0]["effort"], repeats=6)
            for metric in ["qps", "rss_mib", "recall_at_10", "mean_ms", "p50_ms", "p95_ms"]:
                row[metric] = statistics.median(r[metric] for r in group)
                row[metric + "_min"] = min(r[metric] for r in group)
                row[metric + "_max"] = max(r[metric] for r in group)
            summary.append(row)
    save(output / "summary.json", summary)
    assert sha(library) == config["native_library_sha256"], "Native library changed during experiment"
    assert sha(build_cache) == config["native_build_cache_sha256"]
    assert subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip() == source_commit
    assert not subprocess.check_output(["git", "-C", str(source), "diff", "HEAD", "--", ".", ":(exclude)thirdparty"])
    save(output / "environment.json", dict(cpu=subprocess.check_output(["sysctl", "-n", "machdep.cpu.brand_string"], text=True).strip(),
         memory_bytes=int(subprocess.check_output(["sysctl", "-n", "hw.memsize"], text=True)),
         os=subprocess.check_output(["sw_vers"], text=True).strip(), finished=now(),
         file_cache="not purged", query_concurrency=1, query_threads=1, measurement="fresh process /usr/bin/time -l peak RSS, includes open/warmup/query",
         qps="1000 sequential API calls / measured seconds; input query file read before timing",
         order="six rounds; reverse scale and engine order on odd rounds", build_and_query_overlap=False))
    status("complete")


if __name__ == "__main__":
    main()
