"""Validation-only recall parity experiment; no formal query file is read."""
import argparse,hashlib,json,os,re,subprocess
from pathlib import Path

def sha(p):
    with Path(p).open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()

def main():
    p=argparse.ArgumentParser();p.add_argument('--binary',type=Path,required=True);p.add_argument('--library',type=Path,required=True);p.add_argument('--output',type=Path,required=True)
    p.add_argument('--counts',nargs='+',type=int,default=[1000000]);p.add_argument('--scalar-caches',nargs='+',type=int,default=[1]);p.add_argument('--repeats',type=int,default=1);p.add_argument('--efforts',nargs='+',type=int,default=[112,128,144,160]);p.add_argument('--workers',type=int,default=32);p.add_argument('--cache',type=int,default=8);p.add_argument('--backend',choices=['zvec','microsoft'],default='zvec');p.add_argument('--profile',action='store_true');p.add_argument('--adj-cache',type=int,default=0);p.add_argument('--buffered',action='store_true');p.add_argument('--indexes',type=Path,default=Path('/tmp/zvec-local-cohere-20261104/experiment/indexes'));p.add_argument('--microsoft-indexes',type=Path,default=Path('/tmp/zvec-cohere-million/experiment/indexes'))
    a=p.parse_args();a.output.mkdir(exist_ok=False)
    data=Path('/tmp/zvec-disjoint-cohere-20261105/data');rows=[]
    cfg=dict(binary=str(a.binary),binary_sha256=sha(a.binary),library=str(a.library),library_sha256=sha(a.library),runner_sha256=sha(__file__),scalar_caches=a.scalar_caches,counts=a.counts,repeats=a.repeats,warmup_sha256=sha(data/'warmup.f32'),validation_sha256=sha(data/'validation.f32'),node_cache_mib=a.cache,adj_cache_mib=a.adj_cache,buffered=a.buffered,workers=a.workers,efforts=a.efforts,backend=a.backend,native_indexes=str(a.indexes),microsoft_indexes=str(a.microsoft_indexes),profile=a.profile)
    (a.output/'configuration.json').write_text(json.dumps(cfg,indent=2))
    for repeat in range(a.repeats):
      for n in a.counts:
       for effort,scalar in __import__("itertools").product(a.efforts, a.scalar_caches if repeat%2==0 else a.scalar_caches[::-1]):
        e={k:v for k,v in os.environ.items() if not k.startswith(('DYLD_','ZVEC_DISKANN_','ZVEC_BENCH_'))}
        e.update(ZVEC_BENCH_EXPECT_LIBRARY=str(a.library),ZVEC_DISKANN_READ_CACHE_MIB=str(a.cache),ZVEC_DISKANN_READ_WORKERS=str(a.workers),ZVEC_DISKANN_SCALAR_CACHE_MIB=str(scalar),ZVEC_DISKANN_ADJ_CACHE_MIB=str(a.adj_cache))
        if a.profile:e['ZVEC_DISKANN_PROFILE_READS']='1'
        if a.buffered:e['ZVEC_DISKANN_BUFFERED_READS']='1'
        path=a.output/f'pilot-n{n}-L{effort}-c{scalar}-r{repeat}.jsonl'
        with path.open('w') as o,path.with_suffix('.stderr').open('w') as err:
         subprocess.run(['/usr/bin/time','-l',str(a.binary),a.backend,(str(a.indexes/f'diskann-{n}') if a.backend=='zvec' else str(a.microsoft_indexes/f'diskann_fp16-{n}-768/index')),str(data/'warmup.f32'),str(data/'validation.f32'),str(n),str(effort),'/tmp/zvec-cohere-pq64-20261104/preparation/codes.bin'],env=e,stdout=o,stderr=err,check=True)
        raw=[json.loads(x) for x in path.read_text().splitlines()];r=next(x for x in raw if x['stage']=='queried');ids=next(x['ids'] for x in raw if x['stage']=='results');gt=json.loads((data/f'validation-truth-{n}.json').read_text())
        loaded=next(x['path'] for x in raw if x['stage']=='library');assert Path(loaded).resolve()==a.library.resolve()
        assert len(ids)==len(gt)==400 and r['warmup_queries']==600 and r['test_passes']==1 and r['test_warmup_passes']==0
        r.update(scalar_cache_mib=scalar,repeat=repeat,loaded_library=loaded,recall=sum(len(set(x)&set(y)) for x,y in zip(ids,gt))/4000,rss_mib=int(re.search(r'(\d+)\s+maximum resident set size',path.with_suffix('.stderr').read_text()).group(1))/2**20,ids_sha256=hashlib.sha256(json.dumps(ids,separators=(',',':')).encode()).hexdigest(),raw_file=path.name)
        if a.backend=='microsoft':assert all(x['ids_sha256']==r['ids_sha256'] for x in rows if x['count']==n and x['effort']==effort)
        rows.append(r);(a.output/'runs.json').write_text(json.dumps(rows,indent=2));print(json.dumps({k:r[k] for k in ['count','effort','repeat','scalar_cache_mib','qps','rss_mib','recall']}),flush=True)
    assert sha(a.binary)==cfg['binary_sha256'] and sha(a.library)==cfg['library_sha256']
if __name__=='__main__':main()
