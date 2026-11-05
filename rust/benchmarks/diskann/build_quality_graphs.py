"""Build isolated higher-degree graphs; never replace an existing index."""
import argparse,hashlib,json,os,subprocess,time
from pathlib import Path

def sha(p):
    with Path(p).open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def main():
    p=argparse.ArgumentParser();p.add_argument('--base',type=Path,required=True);p.add_argument('--counts',nargs='+',type=int,required=True);a=p.parse_args()
    cfg=json.loads((a.base/'configuration.json').read_text());exe=Path(cfg['binary']);lib=Path(cfg['library'])
    assert sha(exe)==cfg['binary_sha256'] and sha(lib)==cfg['library_sha256']
    for n in a.counts:
        index=a.base/'indexes'/f'diskann-{n}';assert not index.exists()
        path=a.base/'builds'/f'diskann-{n}.jsonl';assert not path.exists()
        env={k:v for k,v in os.environ.items() if not k.startswith(('DYLD_','ZVEC_BENCH_','ZVEC_DISKANN_'))}
        env.update(ZVEC_BENCH_EXPECT_LIBRARY=str(lib),ZVEC_BENCH_BUILD_DEGREE=str(cfg['degree']),ZVEC_BENCH_BUILD_LIST=str(cfg['construction_list']))
        start=time.time()
        with path.open('w') as out,path.with_suffix('.stderr').open('w') as err:
            r=subprocess.run(['/usr/bin/time','-l',str(exe),'build','diskann',str(index),cfg['base_file'],str(n),'128','1'],env=env,stdout=out,stderr=err)
        record=dict(count=n,exit_code=r.returncode,wall_s=time.time()-start,binary_sha256=sha(exe),library_sha256=sha(lib),runner_sha256=sha(__file__))
        if r.returncode==0:
            rows=[json.loads(x) for x in path.read_text().splitlines()];assert next(x['path'] for x in rows if x['stage']=='library')==str(lib.resolve())
            record['built']=next(x for x in rows if x['stage']=='built')
            record['files']={str(f.relative_to(index)):[f.stat().st_size,f.stat().st_mtime_ns] for f in index.rglob('*') if f.is_file()}
        path.with_suffix('.json').write_text(json.dumps(record,indent=2));print(json.dumps(record['built'] if r.returncode==0 else record),flush=True)
        assert r.returncode==0
if __name__=='__main__':main()
