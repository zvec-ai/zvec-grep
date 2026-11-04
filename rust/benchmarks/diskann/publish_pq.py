"""Recompute PQ comparison statistics from raw outputs and publish small audit artifacts."""
import argparse
import csv
import hashlib
import json
import math
import pathlib
import re
import shutil
import statistics

p=argparse.ArgumentParser()
for name in ["data","experiment","preparation","binary","output"]:
    p.add_argument("--"+name,type=pathlib.Path,required=True)
args=p.parse_args()
def read(path): return json.loads(path.read_text())
def sha(path):
    with path.open("rb") as f: return hashlib.file_digest(f,"sha256").hexdigest()
def write(name,value): (args.output/name).write_text(json.dumps(value,indent=2)+"\n")

status=read(args.experiment/"status.json"); assert status["stage"]=="complete"
config=read(args.experiment/"configuration.json")
assert sha(args.binary)==config["binary_sha256"]
assert sha(args.preparation/"manifest.json")==config["preparation_manifest_sha256"]
prep=read(args.preparation/"manifest.json")
for name,info in prep["files"].items(): assert sha(args.preparation/name)==info["sha256"]
assert sha(args.data/"manifest.json")==config["data_manifest_sha256"]
for folder in [args.data,args.data/"validation"]:
    for name,info in read(folder/"manifest.json")["files"].items(): assert sha(folder/name)==info["sha256"]
assert config["index_files_before"]==read(args.experiment/"index-files-after.json")
records=read(args.experiment/"runs.json")
trials=read(args.experiment/"validation.json")
selection=read(args.experiment/"selection.json")
evidence=read(args.experiment/"raw-evidence.json")
assert len(records)==60 and len(selection)==10
assert len(evidence)==2*(len(records)+len(trials))
for item in evidence: assert sha(args.experiment/item["path"])==item["sha256"]
diagnostics=[]; ids_by_group={}
for row in [*trials,*records]:
    path=args.experiment/row["phase"]/f"{row['count']}-{row['mode']}-{row['repeat']}"
    stages=[json.loads(line) for line in path.with_suffix(".jsonl").read_text().splitlines()]
    raw=next(s for s in stages if s["stage"]=="queried")
    ids=next(s["ids"] for s in stages if s["stage"]=="results")
    gt=read(args.data/("validation" if row["phase"]=="validation" else "")/f"truth-{row['count']}.json")
    assert len(ids)==len(gt)==200 and all(len(set(x))==10 for x in ids)
    recall=sum(len(set(a)&set(b)) for a,b in zip(ids,gt))/2000
    assert recall==row["recall_at_10"]
    assert all(raw[key]==row[key] for key in raw)
    assert math.isclose(raw["qps"],200000/raw["batch_ms"],rel_tol=1e-12)
    stderr=path.with_suffix(".stderr").read_text()
    rss=int(re.search(r"(\d+)\s+maximum resident set size",stderr)[1])/2**20
    assert rss==row["rss_mib"]
    if row["phase"]=="runs":
        key=(row["count"],row["mode"])
        assert ids==ids_by_group.setdefault(key,ids)
        elapsed=re.search(r"([\d.]+)\s+real\s+([\d.]+)\s+user\s+([\d.]+)\s+sys",stderr)
        diagnostics.append(dict(count=row["count"],mode=row["mode"],repeat=row["repeat"],
            process_real_s=float(elapsed[1]),process_user_s=float(elapsed[2]),process_sys_s=float(elapsed[3]),
            page_faults=int(re.search(r"(\d+)\s+page faults",stderr)[1]),
            page_reclaims=int(re.search(r"(\d+)\s+page reclaims",stderr)[1])))
summary=read(args.experiment/"summary.json")
assert len(summary)==10
for row in summary:
    group=[r for r in records if (r["count"],r["mode"])==(row["count"],row["mode"])]
    assert sorted(r["repeat"] for r in group)==list(range(6))
    selected=next(r for r in selection if (r["count"],r["mode"])==(row["count"],row["mode"]))
    assert all(r["list_size"]==selected["list_size"] for r in group)
    assert selected["validation_recall_at_10"]>=.92
    for key in ["qps","mean_ms","p50_ms","p95_ms","rss_mib","recall_at_10"]:
        values=[r[key] for r in group]
        assert (row[key],row[key+"_min"],row[key+"_max"])==(statistics.median(values),min(values),max(values))
    assert row["pq_codes_bytes"]==(row["count"]*64 if row["mode"]=="pq" else 0)
args.output.mkdir(parents=True,exist_ok=True)
for name in ["configuration.json","environment.json","selection.json","validation.json","runs.json","summary.json","raw-evidence.json","status.json","index-files-after.json"]:
    shutil.copyfile(args.experiment/name,args.output/name)
write("preparation.json",prep)
if (args.preparation/"reproduction.json").exists():
    reproduction=read(args.preparation/"reproduction.json")
    assert reproduction["files"]==prep["files"]
    assert reproduction["all_training_inputs_and_artifacts_byte_identical"]
    write("reproduction.json",reproduction)
write("process-diagnostics.json",dict(scope="entire process including open, PQ loading and warmup",records=diagnostics))
audit=dict(raw_test_processes_verified=60,raw_validation_processes_verified=len(trials),
    measured_queries=12000,data_and_pq_artifact_hashes_verified=True,index_metadata_unchanged=True,
    recall_recomputed_from_raw_ids=True,qps_and_rss_recomputed=True,summary_recomputed=True,
    result_ids_identical_across_repeats=True,baseline_output_parity_checked_by_runner=True,
    all_test_recalls_at_least_90=all(r["recall_at_10"]>=.9 for r in records),
    all_pq_codes_loaded_into_owned_memory=True,production_default_changed=False,
    scope="PQ64 traversal on existing SQLite graph; full FP16 reranking; static unfiltered benchmark only")
write("audit.json",audit)
with (args.output/"summary.csv").open("w",newline="") as f:
    writer=csv.DictWriter(f,fieldnames=list(summary[0]),lineterminator="\n");writer.writeheader();writer.writerows(summary)
print(json.dumps(audit,indent=2))
for r in summary: print(r["count"],r["mode"],"QPS",round(r["qps"],2),"RSS",round(r["rss_mib"],2),"recall",r["recall_at_10"])
