"""Fresh paired runs of zvec DiskANN and the previous Microsoft Rust/SQLite/PQ adapter."""
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
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


def save(path, value):
    temp = path.with_suffix(".tmp")
    temp.write_text(json.dumps(value, indent=2) + "\n")
    temp.replace(path)


def now():
    return datetime.now(ZoneInfo("Asia/Shanghai")).isoformat()


def main():
    parser = argparse.ArgumentParser()
    for name in ["native-binary", "microsoft-binary", "data", "native-experiment", "microsoft-experiment",
                 "pq-experiment", "pq-preparation", "native-library", "source-root", "output"]:
        parser.add_argument("--" + name, type=Path, required=True)
    a = parser.parse_args()
    output, data = a.output.resolve(), a.data.resolve()
    assert not output.exists(), "Preserve all trials; use a new output directory"
    here = Path(__file__).resolve().parent
    repo = here.parents[2]
    counts = [200000, 400000, 600000, 800000, 1000000]
    original = json.loads((a.native_experiment / "configuration.json").read_text())
    native_selected = json.loads((a.native_experiment / "selection.json").read_text())["selected"]
    ms_selected = json.loads((a.pq_experiment / "selection.json").read_text())
    ms_original = json.loads((a.pq_experiment / "configuration.json").read_text())
    prep = json.loads((a.pq_preparation / "manifest.json").read_text())
    library = a.native_library.resolve()
    assert sha(library) == original["native_library_sha256"]
    assert subprocess.check_output(["git", "-C", str(a.source_root), "rev-parse", "HEAD"], text=True).strip() == original["zvec_source_commit"]
    assert not subprocess.check_output(["git", "-C", str(a.source_root), "diff", "HEAD", "--", ".", ":(exclude)thirdparty"])
    binaries = {"zvec": a.native_binary.resolve(), "microsoft": a.microsoft_binary.resolve()}
    for binary in binaries.values():
        linked = subprocess.check_output(["otool", "-L", str(binary)], text=True)
        paths = [line.strip().split(" (compatibility", 1)[0] for line in linked.splitlines()[1:]]
        assert any(Path(p).is_absolute() and Path(p).resolve() == library for p in paths)
        assert sha(binary.parent / library.name) == sha(library)
    assert sha(data / "manifest.json") == original["data_manifest_sha256"] == prep["configuration"]["data_manifest_sha256"]
    for folder in [data, data / "validation"]:
        for filename, meta in json.loads((folder / "manifest.json").read_text())["files"].items():
            assert sha(folder / filename) == meta["sha256"]
    for filename, meta in prep["files"].items():
        assert sha(a.pq_preparation / filename) == meta["sha256"]
    indexes = {}
    for count in counts:
        indexes["zvec", count] = a.native_experiment / "indexes" / f"diskann-{count}"
        indexes["microsoft", count] = a.microsoft_experiment / "indexes" / f"diskann_fp16-{count}-768" / "index"
        old_file = ms_original["index_files_before"][str(count)]
        p = indexes["microsoft", count]
        assert [p.stat().st_size, p.stat().st_mtime_ns] == [old_file["bytes"], old_file["mtime_ns"]]

    def inventory():
        files = {}
        for (backend, count), path in indexes.items():
            for p in ([path] if path.is_file() else path.rglob("*")):
                if p.is_file():
                    files[f"{backend}/{count}/{p.name if path.is_file() else p.relative_to(path)}"] = [p.stat().st_size, p.stat().st_mtime_ns]
        return files

    before = inventory()
    parameters = []
    for count in counts:
        n = next(r for r in native_selected if r["engine"] == "diskann" and r["count"] == count)
        m = next(r for r in ms_selected if r["mode"] == "pq" and r["count"] == count)
        parameters += [dict(backend="zvec", count=count, effort=n["effort"], expected_validation_recall=n["recall_at_10"]),
                       dict(backend="microsoft", count=count, effort=m["list_size"], expected_validation_recall=m["validation_recall_at_10"])]
    source_paths = sorted((repo / "rust/crates/zg-diskann/src").rglob("*.rs")) + [
        here / "harness/src/bin/native-retest.rs", here / "harness/src/bin/microsoft-retest.rs",
        here / "harness/Cargo.lock", here / "harness/Cargo.toml", Path(__file__).resolve()]
    hashes = {str(p.relative_to(repo)): sha(p) for p in source_paths}
    config = dict(start=now(), counts=counts, repeats=6, parameters=parameters,
        zvec_source_commit=original["zvec_source_commit"], native_library_sha256=sha(library),
        library_path=str(library), binaries={k:dict(path=str(p),sha256=sha(p)) for k,p in binaries.items()},
        source_sha256=hashes, data_manifest_sha256=sha(data / "manifest.json"),
        pq_preparation_manifest_sha256=sha(a.pq_preparation / "manifest.json"),
        pq_codes_sha256=sha(a.pq_preparation / "codes.bin"),
        implementations={"zvec":dict(library="local zvec C++",max_degree=32,build_list=100,pq_chunks=64),
                         "microsoft":dict(library="diskann / diskann-quantization 0.61.0",storage="custom SQLite adapter",
                                          max_degree=48,build_list=100,pq_chunks=64,rerank=128,
                                          codebook="shared across scales, trained on 32768 FP16-rounded vectors from first 200k")},
        query_concurrency=1, query_threads=1, warmup_queries=20, full_workload_warmup_passes=2,
        distinct_measured_queries=200, measured_passes=5, queries_per_process=1000,
        precision="FP16 storage and FP16-rounded query input", metric="cosine", top_k=10,
        parameters_frozen_before_test=True, validation_threshold=.92,test_target=.90,
        indexes="reuse unchanged previously verified graphs and PQ codes; no rebuilding or tuning",
        cache="no purge; each implementation retains its own I/O/cache policy",
        schedule="six rounds, reverse scale and backend order in odd rounds",
        native_experiment=str(a.native_experiment.resolve()), microsoft_experiment=str(a.microsoft_experiment.resolve()),
        pq_experiment=str(a.pq_experiment.resolve()), pq_preparation=str(a.pq_preparation.resolve()), data=str(data))
    output.mkdir(parents=True)
    (output / "raw").mkdir()
    save(output / "configuration.json", config)
    save(output / "index-inventory.json", before)
    environment = {k:v for k,v in os.environ.items() if k not in {
        "DYLD_LIBRARY_PATH","DYLD_FALLBACK_LIBRARY_PATH","DYLD_INSERT_LIBRARIES"}}
    runs, validation = [], []

    def status(stage, **details):
        value = dict(stage=stage, updated=now(), **details)
        save(output / "status.json", value)
        print(json.dumps(value), flush=True)

    def invoke(backend, count, repeat, phase):
        setting = next(r for r in parameters if r["backend"] == backend and r["count"] == count)
        folder = data / "validation" if phase == "validation" else data
        cmd = ["/usr/bin/time", "-l", str(binaries[backend])]
        if backend == "zvec":
            cmd += ["diskann",str(indexes[backend,count]),str(folder / "queries.f32"),str(count),str(setting["effort"])]
        else:
            cmd += [str(indexes[backend,count]),str(folder / "queries.f32"),str(count),str(setting["effort"]),str(a.pq_preparation / "codes.bin")]
        path = output / "raw" / f"{phase}-{backend}-{count}-{repeat}.jsonl"
        status(phase, backend=backend, count=count, repeat=repeat)
        with path.open("w") as out, path.with_suffix(".stderr").open("w") as err:
            subprocess.run(cmd, stdout=out, stderr=err, env=environment, check=True)
        stages = [json.loads(line) for line in path.read_text().splitlines()]
        result = next(r for r in stages if r["stage"] == "queried")
        ids = next(r["ids"] for r in stages if r["stage"] == "results")
        truth = json.loads((folder / f"truth-{count}.json").read_text())
        assert len(ids) == len(truth) == 200 and all(len(set(r)) == len(r) == 10 for r in ids)
        recall = sum(len(set(x) & set(y)) for x,y in zip(ids,truth)) / 2000
        assert result["queries"] == 1000 and result["full_warmup_passes"] == 2
        assert result["effort"] == setting["effort"] and result["count"] == count
        if phase == "validation":
            assert recall == setting["expected_validation_recall"] and recall >= .92
        else:
            assert recall >= .90
        measured = result["phases"][2:]
        assert len(measured) == 5 and all(p["phase"] == "measured" for p in measured)
        assert abs(result["batch_s"] - sum(p["wall_s"] for p in measured)) < 1e-8
        result.update(backend=backend, phase=phase, repeat=repeat, recall_at_10=recall,
            rss_mib=int(re.search(r"(\d+)\s+maximum resident set size",path.with_suffix(".stderr").read_text()).group(1))/2**20,
            cpu_s=sum(p["resources"]["user_s"]+p["resources"]["system_s"] for p in measured),
            measured_major_faults=sum(p["resources"]["major_faults"] for p in measured),
            voluntary_switches=sum(p["resources"]["voluntary_switches"] for p in measured),
            raw_file=str(path.relative_to(output)),finished=now())
        return result

    for count in counts:
        for backend in ["zvec","microsoft"]:
            validation.append(invoke(backend,count,0,"validation"))
            save(output / "validation.json",validation)
    status("validation_passed_parameters_frozen")
    for repeat in range(6):
        for count in counts if repeat%2 == 0 else list(reversed(counts)):
            for backend in (["zvec","microsoft"] if repeat%2 == 0 else ["microsoft","zvec"]):
                runs.append(invoke(backend,count,repeat,"measure"))
                save(output / "runs.json",runs)
    summaries = []
    for count in counts:
        for backend in ["zvec","microsoft"]:
            group = [r for r in runs if r["backend"] == backend and r["count"] == count]
            assert len(group) == 6
            row = dict(count=count,backend=backend,effort=group[0]["effort"],repeats=6)
            for metric in ["qps","rss_mib","recall_at_10","mean_ms","p50_ms","p95_ms","cpu_s","batch_s","measured_major_faults","voluntary_switches"]:
                row[metric] = statistics.median(r[metric] for r in group)
                row[metric+"_min"] = min(r[metric] for r in group)
                row[metric+"_max"] = max(r[metric] for r in group)
            summaries.append(row)
    assert inventory() == before
    assert sha(library) == config["native_library_sha256"]
    assert all(sha(p) == config["binaries"][k]["sha256"] for k,p in binaries.items())
    assert all(sha(repo / p) == digest for p,digest in hashes.items())
    save(output / "summary.json",summaries)
    save(output / "environment.json",dict(finished=now(),
        cpu=subprocess.check_output(["sysctl","-n","machdep.cpu.brand_string"],text=True).strip(),
        memory_bytes=int(subprocess.check_output(["sysctl","-n","hw.memsize"],text=True)),
        os=subprocess.check_output(["sw_vers"],text=True).strip(),system_load="not isolated",file_cache="not purged",
        no_build_or_training_during_measurement=True,unchanged_index_inventory=True))
    status("complete")


if __name__ == "__main__":
    main()
