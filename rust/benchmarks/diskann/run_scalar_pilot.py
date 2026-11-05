"""Validation-only scalar-cache experiment; no formal query file is read."""
import argparse,hashlib,json,os,re,subprocess
from pathlib import Path

def sha(p):
    with Path(p).open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()

def main():
    p=argparse.ArgumentParser();p.add_argument('--binary',type=Path,required=True);p.add_argument('--library',type=Path,required=True);p.add_argument('--output',type=Path,required=True)
    p.add_argument('--counts',nargs='+',type=int,default=[1000000]);p.add_argument('--scalar-caches',nargs='+',type=int,default=[0,1,2,4]);p.add_argument('--repeats',type=int,default=1)
    a=p.parse_args();a.output.mkdir(exist_ok=False)
    data=Path('/tmp/zvec-disjoint-cohere-20261105/data');rows=[]
    cfg=dict(binary=str(a.binary),binary_sha256=sha(a.binary),library=str(a.library),library_sha256=sha(a.library),runner_sha256=sha(__file__),scalar_caches=a.scalar_caches,counts=a.counts,repeats=a.repeats,warmup_sha256=sha(data/'warmup.f32'),validation_sha256=sha(data/'validation.f32'),node_cache_mib=8,workers=32)
    (a.output/'configuration.json').write_text(json.dumps(cfg,indent=2))
    for repeat in range(a.repeats):
      for n in a.counts:
       for scalar in a.scalar_caches if repeat%2==0 else a.scalar_caches[::-1]:
        effort=90 if n==1000000 else 84
        e={k:v for k,v in os.environ.items() if not k.startswith(('DYLD_','ZVEC_DISKANN_','ZVEC_BENCH_'))}
        e.update(ZVEC_BENCH_EXPECT_LIBRARY=str(a.library),ZVEC_DISKANN_READ_CACHE_MIB='8',ZVEC_DISKANN_READ_WORKERS='32',ZVEC_DISKANN_SCALAR_CACHE_MIB=str(scalar))
        path=a.output/f'pilot-n{n}-c{scalar}-r{repeat}.jsonl'
        with path.open('w') as o,path.with_suffix('.stderr').open('w') as err:
         subprocess.run(['/usr/bin/time','-l',str(a.binary),'zvec',f'/tmp/zvec-local-cohere-20261104/experiment/indexes/diskann-{n}',str(data/'warmup.f32'),str(data/'validation.f32'),str(n),str(effort),'/tmp/zvec-cohere-pq64-20261104/preparation/codes.bin'],env=e,stdout=o,stderr=err,check=True)
        raw=[json.loads(x) for x in path.read_text().splitlines()];r=next(x for x in raw if x['stage']=='queried');ids=next(x['ids'] for x in raw if x['stage']=='results');gt=json.loads((data/f'validation-truth-{n}.json').read_text())
        loaded=next(x['path'] for x in raw if x['stage']=='library');assert Path(loaded).resolve()==a.library.resolve()
        assert len(ids)==len(gt)==400 and r['warmup_queries']==600 and r['test_passes']==1 and r['test_warmup_passes']==0
        r.update(scalar_cache_mib=scalar,repeat=repeat,loaded_library=loaded,recall=sum(len(set(x)&set(y)) for x,y in zip(ids,gt))/4000,rss_mib=int(re.search(r'(\d+)\s+maximum resident set size',path.with_suffix('.stderr').read_text()).group(1))/2**20,ids_sha256=hashlib.sha256(json.dumps(ids,separators=(',',':')).encode()).hexdigest(),raw_file=path.name)
        assert all(x['ids_sha256']==r['ids_sha256'] for x in rows if x['count']==n)
        rows.append(r);(a.output/'runs.json').write_text(json.dumps(rows,indent=2));print(json.dumps({k:r[k] for k in ['count','repeat','scalar_cache_mib','qps','rss_mib','recall']}),flush=True)
    assert sha(a.binary)==cfg['binary_sha256'] and sha(a.library)==cfg['library_sha256']
if __name__=='__main__':main()
