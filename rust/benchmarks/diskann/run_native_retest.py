"""Retest identical local indexes with full query-workload warmup and phase counters."""
import argparse
from datetime import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
from zoneinfo import ZoneInfo


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def save(path, value):
    temp = path.with_suffix(".tmp")
    temp.write_text(json.dumps(value, indent=2) + "\n")
    temp.replace(path)


def now():
    return datetime.now(ZoneInfo("Asia/Shanghai")).isoformat()


def inventory(root):
    return {str(p.relative_to(root)): [p.stat().st_size, p.stat().st_mtime_ns]
            for p in root.rglob("*") if p.is_file()}


def summarize(records):
    summaries = []
    for count, engine in sorted({(r["count"], r["engine"]) for r in records}):
        group = [r for r in records if r["count"] == count and r["engine"] == engine]
        assert len(group) == 6 and {r["repeat"] for r in group} == set(range(6))
        row = dict(count=count, engine=engine, effort=group[0]["effort"], repeats=6)
        for key in ["qps", "rss_mib", "recall_at_10", "mean_ms", "p50_ms", "p95_ms",
                    "measured_major_faults", "first_pass_qps", "first_pass_major_faults"]:
            row[key] = statistics.median(r[key] for r in group)
            row[key + "_min"] = min(r[key] for r in group)
            row[key + "_max"] = max(r[key] for r in group)
        summaries.append(row)
    return summaries


def main():
    parser = argparse.ArgumentParser()
    for name in ["binary", "data", "previous", "output", "source-root", "native-library"]:
        parser.add_argument("--" + name, required=True, type=Path)
    args = parser.parse_args()
    binary, data, previous, output = [p.resolve() for p in [args.binary, args.data, args.previous, args.output]]
    library, source = args.native_library.resolve(), args.source_root.resolve()
    assert not output.exists(), "Use a new output directory; preserve all trials"
    original = json.loads((previous / "configuration.json").read_text())
    assert original["local_source_build"] and json.loads((previous / "status.json").read_text())["stage"] == "complete"
    assert sha(library) == original["native_library_sha256"] == sha(binary.parent / library.name)
    linked = subprocess.check_output(["otool", "-L", str(binary)], text=True)
    paths = [line.strip().split(" (compatibility", 1)[0] for line in linked.splitlines()[1:]]
    assert any(Path(p).is_absolute() and Path(p).resolve() == library for p in paths)
    assert subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip() == original["zvec_source_commit"]
    assert not subprocess.check_output(["git", "-C", str(source), "diff", "HEAD", "--", ".", ":(exclude)thirdparty"])
    assert sha(data / "manifest.json") == original["data_manifest_sha256"]
    for folder in [data, data / "validation"]:
        manifest = json.loads((folder / "manifest.json").read_text())
        for name, meta in manifest["files"].items():
            assert sha(folder / name) == meta["sha256"]
    selection = json.loads((previous / "selection.json").read_text())
    assert selection["frozen_before_test"]
    selected = {(r["engine"], r["count"]): r["effort"] for r in selection["selected"]}
    counts = original["counts"]
    fixed_ef = max(selected["hnsw", count] for count in counts)
    indexes = previous / "indexes"
    before_inventory = inventory(indexes)
    output.mkdir(parents=True)
    (output / "raw").mkdir()
    config = dict(local_source_build=True, zvec_source_commit=original["zvec_source_commit"],
        native_library_sha256=sha(library), binary_sha256=sha(binary),
        harness_source_sha256=sha(Path(__file__).parent / "harness/src/bin/native-retest.rs"),
        script_sha256=sha(Path(__file__)), previous_configuration_sha256=sha(previous / "configuration.json"),
        selection_sha256=sha(previous / "selection.json"), data_manifest_sha256=sha(data / "manifest.json"),
        counts=counts, repeats=6, full_workload_warmup_passes=2, distinct_measured_queries=200,
        measured_passes=5, warmup_queries=20, query_threads=1, query_concurrency=1,
        fixed_hnsw_ef=fixed_ef, primary_parameters=selection["selected"],
        index_build="reuse identical read-only local source indexes; no rebuild or tuning",
        file_cache="not purged; complete query workload warmed twice in each fresh process",
        native_library_path=str(library), previous_experiment=str(previous), data_path=str(data),
        start=now(), plan="60 primary processes with unchanged parameters, then 30 HNSW controls at fixed ef=max(previous validation-selected ef)")
    save(output / "configuration.json", config)
    save(output / "index-inventory.json", before_inventory)
    environment = {k: v for k, v in os.environ.items() if k not in {
        "DYLD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES"}}
    runs = []

    def status(stage, **details):
        value = dict(stage=stage, updated=now(), **details)
        save(output / "status.json", value)
        print(json.dumps(value), flush=True)

    def measure(series, count, engine, repeat, effort):
        name = f"{series}-{engine}-{count}-{repeat}"
        status("measure", series=series, count=count, engine=engine, repeat=repeat, effort=effort)
        path = output / "raw" / f"{name}.jsonl"
        with path.open("w") as stdout, path.with_suffix(".stderr").open("w") as stderr:
            subprocess.run(["/usr/bin/time", "-l", str(binary), engine, str(indexes / f"{engine}-{count}"),
                            str(data / "queries.f32"), str(count), str(effort)], stdout=stdout, stderr=stderr,
                           env=environment, check=True)
        stages = [json.loads(line) for line in path.read_text().splitlines()]
        result = next(r for r in stages if r["stage"] == "queried")
        ids = next(r["ids"] for r in stages if r["stage"] == "results")
        truth = json.loads((data / f"truth-{count}.json").read_text())
        assert len(ids) == len(truth) == 200 and all(len(set(row)) == len(row) == 10 for row in ids)
        recall = sum(len(set(a) & set(b)) for a, b in zip(ids, truth)) / 2000
        assert recall >= .90
        phases = result["phases"]
        assert [r["phase"] for r in phases] == ["warmup"] * 2 + ["measured"] * 5
        assert result["queries"] == 1000 and result["passes"] == 5
        assert abs(result["batch_s"] - sum(r["wall_s"] for r in phases[2:])) < 1e-8
        result.update(series=series, repeat=repeat, recall_at_10=recall,
            rss_mib=int(re.search(r"(\d+)\s+maximum resident set size", path.with_suffix(".stderr").read_text()).group(1)) / 2**20,
            measured_major_faults=sum(r["resources"]["major_faults"] for r in phases[2:]),
            first_pass_major_faults=phases[0]["resources"]["major_faults"], first_pass_qps=phases[0]["qps"],
            raw_file=str(path.relative_to(output)), finished=now())
        runs.append(result)
        save(output / "runs.json", runs)

    for series in ["primary", "fixed-ef"]:
        for repeat in range(6):
            for count in counts if repeat % 2 == 0 else list(reversed(counts)):
                engines = ["hnsw", "diskann"] if series == "primary" else ["hnsw"]
                for engine in engines if repeat % 2 == 0 else list(reversed(engines)):
                    measure(series, count, engine, repeat, selected[engine, count] if series == "primary" else fixed_ef)
    assert inventory(indexes) == before_inventory
    assert sha(library) == config["native_library_sha256"] and sha(binary) == config["binary_sha256"]
    for series, filename in [("primary", "summary.json"), ("fixed-ef", "fixed-ef-summary.json")]:
        save(output / filename, summarize([r for r in runs if r["series"] == series]))
    save(output / "environment.json", dict(finished=now(),
        cpu=subprocess.check_output(["sysctl", "-n", "machdep.cpu.brand_string"], text=True).strip(),
        memory_bytes=int(subprocess.check_output(["sysctl", "-n", "hw.memsize"], text=True)),
        os=subprocess.check_output(["sw_vers"], text=True).strip(), unchanged_index_inventory=True,
        query_concurrency=1, query_threads=1, system_load="not isolated", cache="not purged"))
    status("complete")


if __name__ == "__main__":
    main()
