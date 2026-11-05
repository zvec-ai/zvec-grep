"""Reserve 500 Cohere queries not used in any earlier benchmark or validation."""
import argparse,hashlib,json
from pathlib import Path
import numpy as np
from prepare_disjoint import top,sha,save

def main():
    p=argparse.ArgumentParser();p.add_argument('--base',type=Path,required=True);a=p.parse_args()
    out=a.base/'holdout';out.mkdir(exist_ok=False)
    old_path=Path('/tmp/zvec-disjoint-cohere-20261105/data/manifest.json');old=json.loads(old_path.read_text())
    raw=Path(old['source_query_file']);assert sha(raw)==old['source_query_file_sha256']
    pool=np.fromfile(raw,dtype='<f4').reshape(5000,768).astype(np.float64)
    pool=(pool/np.linalg.norm(pool,axis=1)[:,None]).astype('<f4')
    excluded=set(old['previous_query_row_ids'])
    for rows in old['query_source_row_ids'].values():excluded.update(rows)
    base_path=Path('/tmp/zvec-cohere-million/data/base.f32');assert sha(base_path)==old['base_sha256']
    base=np.memmap(base_path,dtype='<f4',mode='r',shape=(1000000,768))
    seen={hashlib.sha256(row.tobytes()).digest() for row in base}
    seen.update(hashlib.sha256(pool[i].tobytes()).digest() for i in excluded)
    seen16={hashlib.sha256(pool[i].astype('<f2').tobytes()).digest() for i in excluded}
    selected=[]
    for i in np.random.default_rng(20261107).permutation(5000):
        key=hashlib.sha256(pool[i].tobytes()).digest();key16=hashlib.sha256(pool[i].astype('<f2').tobytes()).digest()
        if int(i) in excluded or key in seen or key16 in seen16:continue
        selected.append(int(i));seen.add(key);seen16.add(key16)
        if len(selected)==500:break
    assert len(selected)==500
    del seen,seen16
    pool[selected].tofile(out/'queries.f32')
    q=pool[selected].astype(np.float64);q/=np.linalg.norm(q,axis=1)[:,None]
    scores=np.empty((500,0));ids=np.empty((500,0),dtype=np.int64);checks=[]
    for n in [200000,400000,600000,800000,1000000]:
        b=base[n-200000:n].astype(np.float64);b/=np.linalg.norm(b,axis=1)[:,None]
        block_ids=np.arange(n-200000,n);next_s=np.empty((500,11));next_i=np.empty((500,11),dtype=np.int64)
        for start in range(0,500,64):
            rows=q[start:start+64]@b.T
            for j,row in enumerate(rows):
                at=start+j;s,ix=top(row,block_ids)
                next_s[at],next_i[at]=top(np.concatenate((scores[at],s)),np.concatenate((ids[at],ix)))
        scores,ids=next_s,next_i
        if n>=600000:
            save(out/f'truth-{n}.json',[[f'v{i:08}' for i in row[:10]] for row in ids])
            gap=float(np.min(scores[:,9]-scores[:,10]));assert gap>0;checks.append(dict(count=n,minimum_boundary_gap=gap))
            print('HOLDOUT GT',n,flush=True)
    meta=dict(seed=20261107,source_manifest_sha256=sha(old_path),source_queries_sha256=sha(raw),base_sha256=old['base_sha256'],
              previous_rows_excluded=len(excluded),query_source_row_ids=selected,queries=500,counts=[600000,800000,1000000],
              unique_fp32_and_fp16=True,gt_checks=checks,files={})
    for f in out.iterdir():meta['files'][f.name]=dict(bytes=f.stat().st_size,sha256=sha(f))
    save(out/'manifest.json',meta)

if __name__=='__main__':main()
