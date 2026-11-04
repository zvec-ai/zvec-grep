"""Validate/freeze PQ effort, then compare fresh FP16 and PQ64 processes on the same graphs."""
import argparse
import hashlib
import json
import math
import pathlib
import re
import statistics
import subprocess
from datetime import datetime
from zoneinfo import ZoneInfo

p=argparse.ArgumentParser()
for name in ["binary","data","experiment","preparation","output"]:
    p.add_argument("--"+name,type=pathlib.Path,required=True)
args=p.parse_args()
args.output.mkdir(parents=True,exist_ok=False)
for name in ["validation","runs"]: (args.output/name).mkdir()
counts=[200000,400000,600000,800000,1000000]
grid=[128,192,256,384,512,768,1024]
rerank=128

def read(path): return json.loads(path.read_text())
def write(name,value): (args.output/name).write_text(json.dumps(value,indent=2)+"\n")
def sha(path):
    with path.open("rb") as f: return hashlib.file_digest(f,"sha256").hexdigest()
def now(): return datetime.now(ZoneInfo("Asia/Shanghai")).isoformat()
def index_path(count): return args.experiment/f"indexes/diskann_fp16-{count}-768/index"
def metadata():
    return {str(n):dict(bytes=index_path(n).stat().st_size,mtime_ns=index_path(n).stat().st_mtime_ns) for n in counts}

prep=read(args.preparation/"manifest.json")
assert prep["configuration"]["data_manifest_sha256"]==sha(args.data/"manifest.json")
assert sha(args.preparation/"codes.bin")==prep["files"]["codes.bin"]["sha256"]
for folder in [args.data,args.data/"validation"]:
    for name,info in read(folder/"manifest.json")["files"].items():
        assert sha(folder/name)==info["sha256"],name
before=metadata()
config=dict(started=now(),counts=counts,binary_sha256=sha(args.binary),preparation_manifest_sha256=sha(args.preparation/"manifest.json"),
    data_manifest_sha256=sha(args.data/"manifest.json"),pq_code_bytes=64,pq_candidate_grid=grid,rerank=rerank,
    validation_threshold=.92,test_target=.90,repeats=6,warmup_queries=20,measured_queries=200,query_concurrency=1,
    baseline="reuse original independently validated L; recheck validation and test IDs",
    selection="first ascending PQ L achieving validation Recall@10 >= 92%; all scales frozen before test",
    graph="reuse identical FP16 graphs; PQ only changes traversal scoring plus full-vector reranking",
    filter="unfiltered, immutable contiguous live IDs verified at PQ open; entry node 0 excluded",
    schedule="six fresh processes per mode/scale, alternate full/PQ order at each repetition",
    system_cache="not purged; shared OS file cache and other system activity not controlled",
    index_files_before=before)
write("configuration.json",config)
write("environment.json",dict(cpu=subprocess.check_output(["sysctl","-n","machdep.cpu.brand_string"],text=True).strip(),
    memory_bytes=int(subprocess.check_output(["sysctl","-n","hw.memsize"],text=True)),
    os=subprocess.check_output(["sw_vers"],text=True).strip(),
    vm_stat_before=subprocess.check_output(["vm_stat"],text=True),swap_before=subprocess.check_output(["sysctl","vm.swapusage"],text=True)))
evidence=[]
baseline_ids={}

def invoke(mode,count,effort,phase,repeat):
    name=f"{count}-{mode}-{repeat}"
    folder=args.output/phase
    query_folder=args.data/"validation" if phase=="validation" else args.data
    command=["/usr/bin/time","-l",str(args.binary),mode,str(index_path(count)),str(count),str(effort),str(query_folder/"queries.f32")]
    if mode=="pq": command += [str(args.preparation/"codes.bin"),str(rerank)]
    print("START",phase,name,"L",effort,flush=True)
    with (folder/(name+".jsonl")).open("w") as out,(folder/(name+".stderr")).open("w") as err:
        subprocess.run(command,stdout=out,stderr=err,check=True)
    stages=[json.loads(s) for s in (folder/(name+".jsonl")).read_text().splitlines()]
    row=next(s for s in stages if s["stage"]=="queried")
    ids=next(s["ids"] for s in stages if s["stage"]=="results")
    gt=read(query_folder/f"truth-{count}.json")
    assert len(ids)==len(gt)==200 and all(len(set(r))==len(r)==10 for r in ids)
    assert all(re.fullmatch(r"v\d{8}",doc) and 0<=int(doc[1:])<count for r in ids for doc in r)
    assert row["count"]==count and row["queries"]==200 and row["list_size"]==effort and row["storage_precision"]=="fp16"
    assert row["pq_codes_bytes"]==(count*64 if mode=="pq" else 0)
    assert math.isclose(row["qps"],200000/row["batch_ms"],rel_tol=1e-12)
    row["recall_at_10"]=sum(len(set(a)&set(b)) for a,b in zip(ids,gt))/2000
    stderr=(folder/(name+".stderr")).read_text()
    row["rss_mib"]=int(re.search(r"(\d+)\s+maximum resident set size",stderr)[1])/2**20
    row.update(mode=mode,repeat=repeat,phase=phase)
    if phase=="runs" and mode=="full":
        if count not in baseline_ids:
            original=args.experiment/f"results/{count}/{count}/diskann-{count}-0.jsonl"
            baseline_ids[count]=next(json.loads(s)["ids"] for s in original.read_text().splitlines() if json.loads(s)["stage"]=="results")
        assert ids==baseline_ids[count],f"Baseline output changed at {count}"
    for suffix in ["jsonl","stderr"]:
        path=folder/(name+"."+suffix)
        evidence.append(dict(path=str(path.relative_to(args.output)),sha256=sha(path)))
    print("DONE",phase,name,"recall",row["recall_at_10"],"QPS",round(row["qps"],2),"RSS",round(row["rss_mib"],2),flush=True)
    return row

selection=[]; validation=[]
for count in counts:
    old=next(r for r in read(args.experiment/f"results/{count}/selection.json")["selected"] if r["engine"]=="diskann")
    baseline=invoke("full",count,old["effort"],"validation",old["effort"])
    assert baseline["recall_at_10"]==old["validation_recall_at_10"]
    validation.append(baseline)
    selection.append(dict(count=count,mode="full",list_size=old["effort"],validation_recall_at_10=baseline["recall_at_10"]))
    for effort in grid:
        trial=invoke("pq",count,effort,"validation",effort); validation.append(trial)
        write("validation.json",validation)
        if trial["recall_at_10"]>=.92:
            selection.append(dict(count=count,mode="pq",list_size=effort,validation_recall_at_10=trial["recall_at_10"]))
            break
    else: raise RuntimeError(f"PQ64 did not reach validation target at {count}; all trials retained")
    write("selection.json",selection)
write("status.json",dict(stage="parameters_frozen",time=now()))
records=[]
for count in counts:
    for repeat in range(6):
        for mode in (["full","pq"] if repeat%2==0 else ["pq","full"]):
            selected=next(r for r in selection if (r["count"],r["mode"])==(count,mode))
            records.append(invoke(mode,count,selected["list_size"],"runs",repeat))
            write("runs.json",records)
    write("status.json",dict(stage="measured",count=count,time=now()))
summary=[]
for count in counts:
    for mode in ["full","pq"]:
        group=[r for r in records if (r["count"],r["mode"])==(count,mode)]
        assert len(group)==6
        row=dict(count=count,mode=mode,engine=group[0]["engine"],list_size=group[0]["list_size"],rerank=group[0]["rerank"],
            repeats=6,pq_codes_bytes=group[0]["pq_codes_bytes"])
        for metric in ["qps","mean_ms","p50_ms","p95_ms","rss_mib","recall_at_10"]:
            values=[r[metric] for r in group]
            row.update({metric:statistics.median(values),metric+"_min":min(values),metric+"_max":max(values)})
        summary.append(row)
after=metadata(); write("index-files-after.json",after)
assert before==after,"Original graph file changed"
assert sha(args.binary)==config["binary_sha256"]
write("summary.json",summary); write("raw-evidence.json",evidence)
write("status.json",dict(stage="complete",time=now(),all_test_recalls_at_least_90=all(r["recall_at_10"]>=.9 for r in records)))
print("COMPLETE",flush=True)
