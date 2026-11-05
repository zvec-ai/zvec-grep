"""Validation-only memory-budget DiskANN pilot. Never reads a formal test query file."""
import argparse,hashlib,json,os,re,subprocess
from pathlib import Path

def sha(p):
    with Path(p).open('rb') as f: return hashlib.file_digest(f,'sha256').hexdigest()

def main():
    p=argparse.ArgumentParser()
    p.add_argument('--base',type=Path,required=True)
    p.add_argument('--variant',required=True)
    p.add_argument('--binary',type=Path,required=True)
    p.add_argument('--library',type=Path,help='Expected actual dylib for memory-bench; defaults beside the executable')
    p.add_argument('--counts',nargs='+',type=int,default=[1000000])
    p.add_argument('--caches',nargs='+',type=int,default=[0,8,16,32,48,64])
    p.add_argument('--efforts',nargs='+',type=int,default=[128])
    p.add_argument('--workers',nargs='+',type=int,default=[8])
    p.add_argument('--repeats',type=int,default=1)
    p.add_argument('--profile',action='store_true')
    a=p.parse_args();out=a.base/a.variant;out.mkdir(exist_ok=False)
    data=Path('/tmp/zvec-disjoint-cohere-20261105/data')
    lib=a.library or a.binary.parent/'libzvec_c_api.dylib'
    config=dict(binary=str(a.binary),binary_sha256=sha(a.binary),library=str(lib),library_sha256=sha(lib),
                warmup_sha256=sha(data/'warmup.f32'),validation_sha256=sha(data/'validation.f32'),
                counts=a.counts,caches=a.caches,efforts=a.efforts,workers=a.workers,profile=a.profile,repeats=a.repeats)
    (out/'configuration.json').write_text(json.dumps(config,indent=2))
    (out/'source.patch').write_text(subprocess.check_output(['git','diff','047ebaf4'],cwd='/Users/xingrui/Documents/workspace/zvec/zvec_grep_diskann/zvec',text=True))
    env={k:v for k,v in os.environ.items() if not k.startswith(('DYLD_','ZVEC_DISKANN_'))}
    rows=[]
    for repeat in range(a.repeats):
        for n in a.counts:
            for effort in a.efforts:
                for workers in a.workers:
                    for cache in a.caches:
                        name=f'n{n}-L{effort}-c{cache}-w{workers}-r{repeat}'
                        path=out/f'{name}.jsonl'
                        e=dict(env,ZVEC_DISKANN_READ_CACHE_MIB=str(cache),ZVEC_DISKANN_READ_WORKERS=str(workers))
                        e['ZVEC_BENCH_EXPECT_LIBRARY']=str(lib)
                        if a.profile:e['ZVEC_DISKANN_PROFILE_READS']='1'
                        cmd=['/usr/bin/time','-l',str(a.binary),'zvec',f'/tmp/zvec-local-cohere-20261104/experiment/indexes/diskann-{n}',str(data/'warmup.f32'),str(data/'validation.f32'),str(n),str(effort),'/tmp/zvec-cohere-pq64-20261104/preparation/codes.bin']
                        with path.open('w') as stdout,path.with_suffix('.stderr').open('w') as stderr:
                            subprocess.run(cmd,env=e,stdout=stdout,stderr=stderr,check=True)
                        raw=[json.loads(line) for line in path.read_text().splitlines()]
                        r=next(x for x in raw if x['stage']=='queried');ids=next(x['ids'] for x in raw if x['stage']=='results')
                        gt=json.loads((data/f'validation-truth-{n}.json').read_text())
                        assert len(ids)==len(gt)==400 and r['test_passes']==1 and r['test_warmup_passes']==0
                        stderr=path.with_suffix('.stderr').read_text()
                        r.update(cache_mib=cache,workers=workers,repeat=repeat,recall=sum(len(set(x)&set(y)) for x,y in zip(ids,gt))/4000,
                                 rss_mib=int(re.search(r'(\d+)\s+maximum resident set size',stderr).group(1))/2**20,
                                 ids_sha256=hashlib.sha256(json.dumps(ids,separators=(',',':')).encode()).hexdigest(),raw_file=path.name)
                        if a.profile:r['profile']=json.loads(re.search(r'DISKANN_READ_PROFILE (\{.*\})',stderr).group(1))
                        rows.append(r);(out/'runs.json').write_text(json.dumps(rows,indent=2))
                        print(json.dumps({k:r[k] for k in ['count','effort','cache_mib','workers','repeat','qps','recall','rss_mib']}),flush=True)
    assert sha(a.binary)==config['binary_sha256'] and sha(lib)==config['library_sha256']

if __name__=='__main__':main()
