"""Prepare unseen, disjoint Cohere warmup/validation/test queries and exact GT."""
import argparse
import hashlib
import json
from pathlib import Path
import numpy as np

COUNTS = [200000, 400000, 600000, 800000, 1000000]

def sha(path):
    with Path(path).open('rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()

def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')

def top(scores, ids, k=11):
    cut = np.partition(scores, -k)[-k]
    eligible = np.flatnonzero(scores >= cut)
    chosen = eligible[np.lexsort((ids[eligible], -scores[eligible]))[:k]]
    return scores[chosen], ids[chosen]

def main():
    p = argparse.ArgumentParser()
    p.add_argument('--data', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--query-source', type=Path, required=True)
    p.add_argument('--query-source-manifest', type=Path, required=True)
    p.add_argument('--seed', type=int, default=20261105)
    a = p.parse_args()
    a.output.mkdir(parents=True, exist_ok=False)
    old = json.loads((a.data/'manifest.json').read_text())
    assert sha(a.data/'base.f32') == old['files']['base.f32']['sha256']
    base = np.memmap(a.data/'base.f32', dtype='<f4', mode='r', shape=(1000000,768))
    validation = json.loads((a.data/'validation/manifest.json').read_text())
    original_validation = json.loads(a.query_source_manifest.read_text())
    assert sha(a.query_source_manifest) == validation['original_validation_manifest_sha256']
    assert sha(a.query_source) == original_validation['source_query_file_sha256']
    pool = np.fromfile(a.query_source, dtype='<f4').reshape(5000,768)
    pool = (pool.astype(np.float64)/np.linalg.norm(pool.astype(np.float64),axis=1)[:,None]).astype('<f4')
    for folder, meta in [(a.data,old),(a.data/'validation',validation)]:
        assert np.array_equal(pool[meta['query_source_row_ids']],np.fromfile(folder/'queries.f32',dtype='<f4').reshape(220,768))
    previous = set(old['query_source_row_ids'])
    previous.update(validation['query_source_row_ids'])
    seen = {hashlib.sha256(row.tobytes()).digest() for row in base}
    seen.update(hashlib.sha256(pool[i].tobytes()).digest() for i in previous)
    selected = []
    for row in np.random.default_rng(a.seed).permutation(5000):
        key = hashlib.sha256(pool[row].tobytes()).digest()
        if int(row) in previous or key in seen:
            continue
        seen.add(key)
        selected.append(int(row))
        if len(selected) == 4000:
            break
    assert len(selected) == 4000 and len(set(selected)) == 4000
    assert not set(selected)&previous
    del seen
    groups = {'warmup':selected[:600], 'validation':selected[600:1000]}
    groups.update({f'test-{i}':selected[1000+i*500:1500+i*500] for i in range(6)})
    for name, rows in groups.items():
        pool[rows].tofile(a.output/f'{name}.f32')
    measured_ids = selected[600:]
    q = pool[measured_ids].astype(np.float64)
    q /= np.linalg.norm(q,axis=1)[:,None]
    best_scores = np.empty((len(q),0))
    best_ids = np.empty((len(q),0),dtype=np.int64)
    checks=[]
    scalar_q=[0,399,400,3399]
    reference_scores={i:np.empty(0) for i in scalar_q}
    reference_ids={i:np.empty(0,dtype=np.int64) for i in scalar_q}
    for n in COUNTS:
        lo=n-200000
        b=base[lo:n].astype(np.float64)
        b/=np.linalg.norm(b,axis=1)[:,None]
        block_ids=np.arange(lo,n)
        next_scores=np.empty((len(q),11))
        next_ids=np.empty((len(q),11),dtype=np.int64)
        for start in range(0,len(q),64):
            scores=q[start:start+64]@b.T
            for j,row in enumerate(scores):
                at=start+j
                s,ids=top(row,block_ids)
                next_scores[at],next_ids[at]=top(np.concatenate((best_scores[at],s)),np.concatenate((best_ids[at],ids)))
        best_scores,best_ids=next_scores,next_ids
        for i in scalar_q:
            # Independently accumulate scalar dot products, then select exact top-k.
            scalar=np.einsum('ij,j->i',b,q[i],optimize=False)
            s,ids=top(scalar,block_ids)
            reference_scores[i],reference_ids[i]=top(np.concatenate((reference_scores[i],s)),np.concatenate((reference_ids[i],ids)))
            assert np.array_equal(reference_ids[i][:10],best_ids[i,:10])
        del b
        truth=[[f'v{i:08}' for i in row[:10]] for row in best_ids]
        save(a.output/f'validation-truth-{n}.json',truth[:400])
        for r in range(6):
            save(a.output/f'test-{r}-truth-{n}.json',truth[400+r*500:900+r*500])
        gap=float(np.min(best_scores[:,9]-best_scores[:,10]))
        assert gap>0
        checks.append(dict(count=n,minimum_top10_boundary_gap=gap,scalar_reference_query_positions=scalar_q))
        print('EXACT GT',n,flush=True)
    meta=dict(seed=a.seed,rng='PCG64',numpy_version=np.__version__,dimension=768,counts=COUNTS,
              source_data_manifest_sha256=sha(a.data/'manifest.json'),base_sha256=sha(a.data/'base.f32'),
              source_query_file=str(a.query_source),source_query_file_sha256=sha(a.query_source),previous_query_row_ids=sorted(previous),
              query_source_row_ids=groups,warmup_queries=600,validation_queries=400,test_queries=3000,
              test_shards=6,test_queries_per_shard=500,disjoint_by_source_row_and_normalized_vector=True,
              previous_queries_and_exact_base_matches_excluded=True,gt='exhaustive float64 cosine of normalized FP32 input; stable ID tie-break',
              gt_checks=checks,files={})
    for f in a.output.iterdir():
        meta['files'][f.name]=dict(bytes=f.stat().st_size,sha256=sha(f))
    save(a.output/'manifest.json',meta)
    print('DISJOINT DATA READY',flush=True)

if __name__=='__main__': main()
