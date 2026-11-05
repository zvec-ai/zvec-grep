"""Isolated validation-only memory and I/O attribution controls."""
import argparse,hashlib,json,os,re,shutil,subprocess
from pathlib import Path

def sha(p): return hashlib.sha256(Path(p).read_bytes()).hexdigest()
def save(p,x): p.write_text(json.dumps(x,indent=2)+'\n')

def main():
    p=argparse.ArgumentParser();p.add_argument('--base',type=Path,required=True);a=p.parse_args()
    b=a.base;out=b/'controls';out.mkdir(exist_ok=False)
    source=b/'rust-target/release/memory-bench';final=b/'final/libzvec_c_api.dylib'
    original=Path('/tmp/zvec-unseen-opt-20261105/final/libzvec_c_api.dylib')
    data=Path('/tmp/zvec-disjoint-cohere-20261105/data')
    gt=json.loads((data/'validation-truth-1000000.json').read_text())
    env={k:v for k,v in os.environ.items() if not k.startswith(('DYLD_','ZVEC_DISKANN_','ZVEC_BENCH_'))}
    runs=[]
    configurations=[('baseline',original,0,108,24),('baseline',original,8,108,24),
                    ('compact-keys',b/'compact-keys/libzvec_c_api.dylib',0,108,24),
                    ('compact-keys',b/'compact-keys/libzvec_c_api.dylib',8,108,24),
                    ('admission',b/'admission/libzvec_c_api.dylib',8,108,24),
                    ('final',final,0,90,32),('final',final,4,90,32),('final',final,8,90,32)]
    for variant,lib,cache,effort,workers in configurations:
        name=f'{variant}-c{cache}';d=out/name;d.mkdir()
        exe=d/'memory-bench';shutil.copy2(source,exe)
        if lib!=final:subprocess.run(['install_name_tool','-change',str(final),str(lib),str(exe)],check=True)
        subprocess.run(['codesign','--force','--sign','-',str(exe)],check=True,capture_output=True)
        e=dict(env,ZVEC_BENCH_EXPECT_LIBRARY=str(lib),ZVEC_DISKANN_READ_CACHE_MIB=str(cache),
               ZVEC_DISKANN_READ_WORKERS=str(workers),ZVEC_DISKANN_PROFILE_READS='1')
        cmd=['/usr/bin/time','-l',str(exe),'zvec','/tmp/zvec-local-cohere-20261104/experiment/indexes/diskann-1000000',
             str(data/'warmup.f32'),str(data/'validation.f32'),'1000000',str(effort),'/tmp/zvec-cohere-pq64-20261104/preparation/codes.bin']
        path=d/'run.jsonl'
        with path.open('w') as o,path.with_suffix('.stderr').open('w') as err:subprocess.run(cmd,env=e,stdout=o,stderr=err,check=True)
        raw=[json.loads(line) for line in path.read_text().splitlines()];r=next(x for x in raw if x['stage']=='queried')
        loaded=next(x['path'] for x in raw if x['stage']=='library');assert Path(loaded).resolve()==lib.resolve()
        ids=next(x['ids'] for x in raw if x['stage']=='results');assert len(ids)==len(gt)==400
        stderr=path.with_suffix('.stderr').read_text()
        r.update(variant=variant,cache_mib=cache,workers=workers,library=str(lib),library_sha256=sha(lib),binary_sha256=sha(exe),loaded_library=loaded,
                 recall_at_10=sum(len(set(x)&set(y)) for x,y in zip(ids,gt))/4000,
                 rss_mib=int(re.search(r'(\d+)\s+maximum resident set size',stderr).group(1))/2**20,
                 results_sha256=hashlib.sha256(json.dumps(ids,separators=(',',':')).encode()).hexdigest(),raw_file=str(path.relative_to(out)),
                 read_profile=json.loads(re.search(r'DISKANN_READ_PROFILE (\{.*\})',stderr).group(1)))
        assert all(r['results_sha256']==x['results_sha256'] for x in runs if x['effort']==effort)
        runs.append(r);save(out/'runs.json',runs)
        print(json.dumps({k:r[k] for k in ['variant','cache_mib','effort','qps','rss_mib','recall_at_10']}),flush=True)
    save(out/'configuration.json',dict(runner_sha256=sha(__file__),warmup_sha256=sha(data/'warmup.f32'),validation_sha256=sha(data/'validation.f32'),query_scope='independent validation only',all_equal_L_ids_identical=True))

if __name__=='__main__':main()
