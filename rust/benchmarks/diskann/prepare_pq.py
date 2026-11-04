"""Train PQ64 on base vectors only; encode the existing million-vector FP16 corpus."""
import argparse
import hashlib
import json
import os
import pathlib
import subprocess
import time

import numpy as np

p = argparse.ArgumentParser()
p.add_argument("--binary", type=pathlib.Path, required=True)
p.add_argument("--data", type=pathlib.Path, required=True)
p.add_argument("--experiment", type=pathlib.Path, required=True)
p.add_argument("--output", type=pathlib.Path, required=True)
args = p.parse_args()
args.output.mkdir(parents=True, exist_ok=False)

def sha(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()

def write(name, value):
    (args.output / name).write_text(json.dumps(value, indent=2) + "\n")

manifest = json.loads((args.data / "manifest.json").read_text())
assert sha(args.data / "base.f32") == manifest["files"]["base.f32"]["sha256"]
base = np.memmap(args.data / "base.f32", mode="r", dtype="<f4", shape=(1000000,768))
ids = np.random.default_rng(20261104).choice(200000,32768,replace=False)
training = args.output / "training.f32"
base[ids].astype("<f4").tofile(training)
write("training-ids.json", ids.tolist())
config = dict(dimension=768, chunks=64, centroids_per_chunk=256, bytes_per_vector=64,
    training_rows=32768, training_population_prefix=200000, sampling_seed=20261104,
    kmeans_seed=20261104, lloyd_iterations=20, threads=4,
    library="diskann-quantization 0.61.0", training="LightPQTrainingParameters: kmeans++ and Lloyd iterations",
    input_precision="FP16-rounded original normalized FP32 corpus; no query vectors in training",
    codebook_shared_across_scales=True, opq=False, centering=False,
    data_manifest_sha256=sha(args.data / "manifest.json"), binary_sha256=sha(args.binary),
    training_file_sha256=sha(training), training_ids_sha256=sha(args.output / "training-ids.json"))
write("configuration.json", config)
env = dict(os.environ, RAYON_NUM_THREADS="4")
timings = {}
for mode, source, target, last in [
    ("train", training, args.output / "pivots.f32", "20"),
    ("encode", args.data / "base.f32", args.output / "codes.bin", str(args.output / "pivots.f32")),
]:
    print(mode, flush=True)
    started = time.monotonic()
    tmp = target.with_suffix(target.suffix + ".tmp")
    with (args.output / f"{mode}.log").open("w") as log:
        subprocess.run(["nice","-n","10","/usr/bin/time","-l",str(args.binary),mode,str(source),str(tmp),"768","64",last],
                       stdout=log,stderr=subprocess.STDOUT,env=env,check=True)
    tmp.rename(target)
    timings[mode + "_seconds"] = time.monotonic()-started
    print(mode,"complete",timings[mode+"_seconds"],flush=True)

assert (args.output / "codes.bin").stat().st_size == 24+256*768*4+1000000*64
codes = np.memmap(args.output / "codes.bin",mode="r",dtype="u1",offset=24+256*768*4,shape=(1000000,64))
pivots = np.fromfile(args.output / "pivots.f32",dtype="<f4").reshape(256,768)
probe_ids = sorted(set([0,199999,399999,599999,799999,999999,*np.random.default_rng(20261105).choice(1000000,128,replace=False).tolist()]))
for i in probe_ids:
    vector = base[i].astype(np.float16).astype(np.float64)
    for chunk in range(64):
        section = slice(chunk*12,(chunk+1)*12)
        distances = np.sum((pivots[:,section].astype(np.float64)-vector[section])**2,axis=1)
        assert distances[codes[i,chunk]] <= distances.min()+1e-7, (i,chunk)

# Independently bind every prefix's FP16 input bytes to the original persisted graph.
digest = hashlib.sha256()
prefix_hashes = {}
for start in range(0,1000000,4000):
    digest.update(base[start:start+4000].astype("<f2").tobytes())
    count = start+4000
    if count in manifest["counts"]:
        saved = json.loads((args.experiment / f"indexes/diskann_fp16-{count}-768/build-complete.json").read_text())
        assert digest.hexdigest() == saved["live_vectors_sha256"], count
        prefix_hashes[str(count)] = digest.hexdigest()
write("manifest.json",dict(configuration=config, timings=timings,
    independent_encoding_probe_rows=probe_ids, independent_encoding_check="FP64 squared-L2 nearest centroid per chunk, tolerance 1e-7",
    prefix_fp16_hashes_match_original_indexes=prefix_hashes,
    files={name:dict(bytes=(args.output/name).stat().st_size,sha256=sha(args.output/name))
           for name in ["pivots.f32","codes.bin","training.f32","training-ids.json"]}))
print("PREPARATION VERIFIED",flush=True)
