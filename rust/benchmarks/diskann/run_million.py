"""Resumable, sequential FP16 builds and measurements for larger Cohere prefixes."""
import argparse
import csv
import hashlib
import json
import pathlib
import shutil
import sqlite3
import subprocess
from datetime import datetime
import numpy as np

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=pathlib.Path, required=True)
parser.add_argument("--builder", type=pathlib.Path, required=True)
parser.add_argument("--data", type=pathlib.Path, required=True)
parser.add_argument("--previous-fp16-index", type=pathlib.Path, required=True)
parser.add_argument("--output", type=pathlib.Path, required=True)
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=True)
indexes = args.output / "indexes"
results = args.output / "results"
indexes.mkdir(exist_ok=True)
results.mkdir(exist_ok=True)
manifest = json.loads((args.data / "manifest.json").read_text())
counts = manifest["counts"]
assert counts == [200000,400000,600000,800000,1000000]

def sha(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f,"sha256").hexdigest()

configuration = dict(data_manifest_sha256=sha(args.data/"manifest.json"),
                     binary_sha256=sha(args.binary), builder_sha256=sha(args.builder),
                     measurement_scripts_sha256={name: sha(pathlib.Path(__file__).with_name(name))
                         for name in ["run_recall90.py", "run.py"]},
                     counts=counts, previous_fp16_index=str(args.previous_fp16_index.resolve()))
config_file = args.output / "configuration.json"
if config_file.exists():
    assert json.loads(config_file.read_text()) == configuration, "Resume requires identical data and executables"
config_file.write_text(json.dumps(configuration,indent=2))

def write_json(path, value):
    temp = path.with_suffix(path.suffix + ".tmp")
    temp.write_text(json.dumps(value,indent=2))
    temp.replace(path)

def status(stage, count):
    value = dict(stage=stage,count=count,updated=datetime.now().astimezone().isoformat())
    write_json(args.output/"status.json",value)
    print(stage,count,flush=True)

def audit_index(path, expected_count=None):
    db = sqlite3.connect(f"file:{path}?mode=ro",uri=True)
    db.executescript("PRAGMA cache_size=-256; PRAGMA mmap_size=0;")
    assert db.execute("SELECT version,dimension,metric FROM settings").fetchone()==(2,768,"cosine")
    actual,visited = hashlib.sha256(),0
    for i,(node,document,blob) in enumerate(db.execute("SELECT id,document_id,vector FROM nodes WHERE live=1 ORDER BY id")):
        assert node==i+1 and document==f"v{i:08}" and len(blob)==768*2
        actual.update(blob);visited+=1
    assert db.execute("SELECT COUNT(*) FROM nodes").fetchone()[0]==visited+1
    if expected_count is not None:
        assert visited==expected_count
    expected=hashlib.sha256()
    with (args.data/"base.f32").open("rb") as f:
        for offset in range(0,visited,4000):
            n=min(4000,visited-offset)
            values=np.fromfile(f,dtype="<f4",count=n*768)
            assert values.size==n*768
            expected.update(values.astype("<f2").tobytes())
    assert actual.digest()==expected.digest(),"Stored vectors do not match the claimed corpus prefix"
    db.close()
    return dict(count=visited,precision="FP16",all_stored_vectors_match_numpy_fp16=True,
                live_vector_bytes=visited*768*2,live_vectors_sha256=actual.hexdigest(),
                database_bytes=path.stat().st_size)

def preserve_failed(path):
    target=path.with_name(path.name+"-incomplete-"+datetime.now().strftime("%Y%m%d%H%M%S%f"))
    path.rename(target)

previous=args.previous_fp16_index
audit_index(previous,100000)
all_runs,all_summary,all_audits=[],[],[]
for count in counts:
    assert shutil.disk_usage(args.output).free > 8*2**30, "Insufficient disk headroom; benchmark paused without deleting data"
    diskdir=indexes/f"diskann_fp16-{count}-768"
    diskdir.mkdir(exist_ok=True)
    diskpath=diskdir/"index"
    complete=diskdir/"build-complete.json"
    assert diskpath.exists() or not complete.exists(), "Completed index is missing; refusing to reuse its marker"
    if not diskpath.exists():
        status("copy_diskann_prefix",count)
        temporary=diskdir/"index.copying"
        shutil.copy2(previous,temporary)
        temporary.replace(diskpath)
    if not complete.exists():
        status("audit_diskann_resume",count)
        prefix=audit_index(diskpath)["count"]
        assert 0<prefix<=count
        if prefix<count:
            status("build_diskann",count)
            attempt=datetime.now().strftime("%Y%m%d%H%M%S")
            with (diskdir/f"build-{attempt}.jsonl").open("w") as out,(diskdir/f"build-{attempt}.stderr").open("w") as err:
                subprocess.run(["/usr/bin/nice","-n","10","/usr/bin/time","-l",str(args.builder),"diskann",str(diskpath),
                    str(args.data/"base.f32"),str(count),str(prefix),"fp16"],stdout=out,stderr=err,check=True)
        status("audit_diskann_complete",count)
        audit=audit_index(diskpath,count)
        write_json(complete,audit)
    else:
        audit=json.loads(complete.read_text())
        assert audit["count"]==count
    all_audits.append(audit)
    previous=diskpath
    hnswdir=indexes/f"hnsw_fp16-{count}-768"
    hnswdir.mkdir(exist_ok=True)
    hnswpath=hnswdir/"index"
    complete=hnswdir/"build-complete.json"
    assert hnswpath.exists() or not complete.exists(), "Completed HNSW index is missing"
    if not complete.exists():
        if hnswpath.exists():
            preserve_failed(hnswpath)
        status("build_hnsw",count)
        attempt=datetime.now().strftime("%Y%m%d%H%M%S")
        with (hnswdir/f"build-{attempt}.jsonl").open("w") as out,(hnswdir/f"build-{attempt}.stderr").open("w") as err:
            subprocess.run(["/usr/bin/nice","-n","10","/usr/bin/time","-l",str(args.builder),"hnsw",str(hnswpath),
                str(args.data/"base.f32"),str(count),"0"],stdout=out,stderr=err,check=True)
        stages=[json.loads(line) for line in (hnswdir/f"build-{attempt}.jsonl").read_text().splitlines()]
        assert stages[-1]["stage"]=="built" and stages[-1]["count"]==count
        write_json(complete,dict(count=count,builder_verified_document_count_and_index_completeness=True))
    output=results/str(count)
    if not (output/"environment.json").exists():
        if output.exists():
            preserve_failed(output)
        status("validate_and_measure",count)
        with (results/f"run-{count}.log").open("w") as log:
            subprocess.run(["python3",str(pathlib.Path(__file__).with_name("run_recall90.py")),
                "--binary",str(args.binary),"--indexes",str(indexes),"--data",str(args.data),
                "--validation",str(args.data/"validation"),"--output",str(output),
                "--diskann-precision","fp16","--counts",str(count)],stdout=log,stderr=subprocess.STDOUT,check=True)
    group=json.loads((output/"results.json").read_text())
    assert len(group)==12 and {r["count"] for r in group}=={count}
    assert {r["storage_precision"] for r in group}=={"fp16"}
    all_runs.extend(group)
    all_summary.extend(json.loads((output/"summary.json").read_text()))
    write_json(args.output/"results.json",all_runs)
    write_json(args.output/"summary.json",all_summary)
    write_json(args.output/"build-audit.json",all_audits)
    with (args.output/"summary.csv").open("w",newline="") as f:
        writer=csv.DictWriter(f,fieldnames=list(all_summary[0]),lineterminator="\n")
        writer.writeheader();writer.writerows(all_summary)
    status("scale_complete",count)
status("complete",counts[-1])
print("ALL MILLION-SCALE MEASUREMENTS COMPLETE",flush=True)
