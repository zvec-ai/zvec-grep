"""Validate a frozen scalar-cache candidate, then run seven disjoint regression shards once."""
import argparse,hashlib,json,os,re,statistics,subprocess
from pathlib import Path
from datetime import datetime
from zoneinfo import ZoneInfo

HERE=Path(__file__).resolve().parent
REPO=HERE.parents[2]
COUNTS=[600000,800000,1000000]

def sha(p):
    with Path(p).open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()

def save(p,x):p.write_text(json.dumps(x,indent=2)+'\n')
def now():return datetime.now(ZoneInfo('Asia/Shanghai')).isoformat()

def main():
    p=argparse.ArgumentParser();p.add_argument('--base',type=Path,required=True);p.add_argument('--stage',choices=['validation','measure'],required=True)
    a=p.parse_args();out=a.base/a.stage;out.mkdir(exist_ok=False);(out/'raw').mkdir()
    source=Path('/Users/xingrui/Documents/workspace/zvec/zvec_grep_diskann/zvec')
    data=Path('/tmp/zvec-disjoint-cohere-20261105/data')
    previous=json.loads((HERE/'results/memory-optimization/measure/configuration.json').read_text())
    selection=json.loads((a.base/'selection-draft.json').read_text())
    binaries={'baseline':Path('/tmp/zvec-memory-opt-20261105/rust-target/release/memory-bench'),'zvec':a.base/'final/memory-bench','microsoft':a.base/'final/memory-bench'}
    libraries={'baseline':Path('/tmp/zvec-memory-opt-20261105/final/libzvec_c_api.dylib'), 'zvec':a.base/'final/libzvec_c_api.dylib', 'microsoft':a.base/'final/libzvec_c_api.dylib'}
    assert sha(libraries['baseline'])==previous['libraries']['zvec']['sha256']
    assert sha(libraries['zvec'])==json.loads((a.base/'final/build.json').read_text())['library_sha256']
    for f,digest in previous['microsoft_source_sha256'].items():assert sha(REPO/f)==digest
    manifest=json.loads((data/'manifest.json').read_text())
    holdout=json.loads((Path('/tmp/zvec-unseen-opt-20261105/holdout/manifest.json')).read_text())
    for folder,meta in [(data,manifest),(Path('/tmp/zvec-unseen-opt-20261105/holdout'),holdout)]:
        for name,info in meta['files'].items():assert sha(folder/name)==info['sha256']
    for name,binary in binaries.items():
        assert str(libraries[name]) in subprocess.check_output(['otool','-L',str(binary)],text=True)
    def inventory():
        result={}
        for n in COUNTS:
            for f in (Path(previous['indexes'])/f'diskann-{n}').rglob('*'):
                if f.is_file():result[str(f)]=[f.stat().st_size,f.stat().st_mtime_ns]
            f=Path(previous['microsoft_indexes'])/f'diskann_fp16-{n}-768/index';result[str(f)]=[f.stat().st_size,f.stat().st_mtime_ns]
        return result
    before=inventory()
    source_paths=list(json.loads((a.base/'final/build.json').read_text())['source_sha256']) + list(previous['source_sha256'])
    config=dict(start=now(),stage=a.stage,selection=selection,source=str(source),source_base='f8a36342',
                source_commit=subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip(),
                source_sha256={f:sha(source/f) for f in source_paths},
                binaries={b:dict(path=str(f),sha256=sha(f)) for b,f in binaries.items()},
                libraries={b:dict(path=str(f),sha256=sha(f)) for b,f in libraries.items()},
                harness_sha256=sha(HERE/'harness/src/bin/memory-bench.rs'),runner_sha256=sha(__file__),
                microsoft_source_sha256=previous['microsoft_source_sha256'],
                indexes=previous['indexes'],microsoft_indexes=previous['microsoft_indexes'],pq_preparation=previous['pq_preparation'],
                data=str(data),data_manifest_sha256=sha(data/'manifest.json'),holdout_manifest_sha256=sha(Path('/tmp/zvec-unseen-opt-20261105/holdout/manifest.json')),
                counts=COUNTS,warmup_queries=600,validation_queries=400,regression_queries=3500,previously_used_test_queries=True,
                cache_mib=selection['cache_mib'],read_workers=selection['workers'],query_concurrency=1,test_passes=1,test_warmup_passes=0,
                scalar_cache_mib=selection['scalar_cache_mib'],native_file_policy='graph: F_NOCACHE retained, read-ahead disabled; scalar: 4 KiB buffered pages, no persistent mmap',os_file_cache='not purged',
                test_order='seven disjoint regression shards, each once; alternate scale and backend orders')
    if a.stage=='measure':
        confirmed=json.loads((a.base/'validation/comparison.json').read_text());assert confirmed['all_scales_faster']
        valid_config=json.loads((a.base/'validation/configuration.json').read_text())
        assert config['source_sha256']==valid_config['source_sha256']
        assert config['libraries']==valid_config['libraries'] and config['selection']==valid_config['selection']
        assert not subprocess.check_output(['git','status','--porcelain'],cwd=source,text=True).strip()
        config['confirmation_sha256']=sha(a.base/'validation/comparison.json')
    save(out/'configuration.json',config);save(out/'index-inventory.json',before)
    (out/'source.patch').write_text(subprocess.check_output(['git','diff','f8a36342'],cwd=source,text=True))
    rows=[]
    env={k:v for k,v in os.environ.items() if not k.startswith(('DYLD_','ZVEC_DISKANN_'))}
    def invoke(backend,n,phase,repeat,effort=None,workers=None,profile=False):
        if effort is None:effort=128 if backend=='microsoft' else (previous['selection']['settings'][str(n)]['effort'] if backend=='baseline' else selection['settings'][str(n)]['effort'])
        if workers is None:workers=selection['workers'] if backend=='zvec' else (32 if backend=='baseline' else 1)
        if phase=='regression' and repeat==6:qfile=Path('/tmp/zvec-unseen-opt-20261105/holdout/queries.f32');gtfile=Path(f'/tmp/zvec-unseen-opt-20261105/holdout/truth-{n}.json')
        elif phase=='regression':qfile=data/f'test-{repeat}.f32';gtfile=data/f'test-{repeat}-truth-{n}.json'
        else:qfile=data/'validation.f32';gtfile=data/f'validation-truth-{n}.json'
        path=out/'raw'/f'{phase}-{backend}-{n}-{repeat}-L{effort}-w{workers}.jsonl';assert not path.exists()
        status=dict(time=now(),phase=phase,backend=backend,count=n,repeat=repeat,effort=effort,workers=workers)
        print(json.dumps(status),flush=True);save(out/'status.json',status)
        index=Path(previous['microsoft_indexes'])/f'diskann_fp16-{n}-768/index' if backend=='microsoft' else Path(previous['indexes'])/f'diskann-{n}'
        e=dict(env,ZVEC_DISKANN_READ_CACHE_MIB=str(0 if backend=='microsoft' else (8 if backend=='baseline' else selection['cache_mib'])),ZVEC_DISKANN_READ_WORKERS=str(workers))
        e['ZVEC_BENCH_EXPECT_LIBRARY']=str(libraries[backend])
        e['ZVEC_DISKANN_SCALAR_CACHE_MIB']=str(selection['scalar_cache_mib'] if backend=='zvec' else 0)
        if profile:e['ZVEC_DISKANN_PROFILE_READS']='1'
        cmd=['/usr/bin/time','-l',str(binaries[backend]),'microsoft' if backend=='microsoft' else 'zvec',str(index),str(data/'warmup.f32'),str(qfile),str(n),str(effort),str(Path(previous['pq_preparation'])/'codes.bin')]
        with path.open('w') as stdout,path.with_suffix('.stderr').open('w') as stderr:subprocess.run(cmd,env=e,stdout=stdout,stderr=stderr,check=True)
        raw=[json.loads(line) for line in path.read_text().splitlines()];row=next(r for r in raw if r['stage']=='queried');ids=next(r['ids'] for r in raw if r['stage']=='results')
        loaded=next(r['path'] for r in raw if r['stage']=='library');assert Path(loaded).resolve()==libraries[backend].resolve()
        gt=json.loads(gtfile.read_text());q=500 if phase in ['holdout','regression'] else 400
        assert row['test_passes']==1 and row['test_warmup_passes']==0 and row['warmup_queries']==600
        assert len(ids)==len(gt)==row['queries']==q and all(len(set(x))==10 for x in ids)
        hits=sum(len(set(x)&set(y)) for x,y in zip(ids,gt));text=path.with_suffix('.stderr').read_text()
        row.update(backend=backend,phase=phase,repeat=repeat,workers=workers,recall_hits=hits,recall_at_10=hits/(q*10),
                   rss_mib=int(re.search(r'(\d+)\s+maximum resident set size',text).group(1))/2**20,
                   results_sha256=hashlib.sha256(json.dumps(ids,separators=(',',':')).encode()).hexdigest(),raw_file=str(path.relative_to(out)),finished=now(),profile=profile)
        if profile:row['read_profile']=json.loads(re.search(r'DISKANN_READ_PROFILE (\{.*\})',text).group(1))
        row['loaded_library']=loaded
        row['scalar_cache_mib']=int(e['ZVEC_DISKANN_SCALAR_CACHE_MIB'])
        if phase=='validation':
            assert all(r['results_sha256']==row['results_sha256'] for r in rows if r['backend']!='microsoft' and backend!='microsoft' and r['count']==n and r['effort']==effort)
        rows.append(row);save(out/'runs.json',rows)
        assert row['recall_at_10']>=.90
        return row
    if a.stage=='validation':
        for repeat in range(3):
            for n in COUNTS if repeat%2==0 else COUNTS[::-1]:
                for backend in (['baseline','zvec','microsoft'] if repeat%2==0 else ['microsoft','zvec','baseline']):invoke(backend,n,'validation',repeat)
    else:
        for repeat in range(7):
            for i,n in enumerate(COUNTS if repeat%2==0 else COUNTS[::-1]):
                for backend in (['zvec','microsoft'] if (repeat+i)%2==0 else ['microsoft','zvec']):invoke(backend,n,'regression',repeat)
    summary=[];comparison=[]
    for phase in (['validation'] if a.stage=='validation' else ['regression']):
        for n in COUNTS:
            for backend in (['baseline','zvec','microsoft'] if a.stage=='validation' else ['zvec','microsoft']):
                g=[r for r in rows if r['phase']==phase and r['count']==n and r['backend']==backend]
                summary.append(dict(phase=phase,count=n,backend=backend,processes=len(g),queries=sum(r['queries'] for r in g),
                    qps=sum(r['queries'] for r in g)/sum(r['batch_s'] for r in g),qps_median=statistics.median(r['qps'] for r in g),qps_min=min(r['qps'] for r in g),qps_max=max(r['qps'] for r in g),
                    rss_mib=statistics.median(r['rss_mib'] for r in g),recall_at_10=sum(r['recall_hits'] for r in g)/sum(r['queries']*10 for r in g),recall_min=min(r['recall_at_10'] for r in g)))
            z=next(r for r in summary if r['phase']==phase and r['count']==n and r['backend']=='zvec');m=next(r for r in summary if r['phase']==phase and r['count']==n and r['backend']=='microsoft')
            comparison.append(dict(phase=phase,count=n,qps_ratio=z['qps']/m['qps'],faster=z['qps']>m['qps']))
    assert inventory()==before
    for b,f in binaries.items():assert sha(f)==config['binaries'][b]['sha256'] and sha(libraries[b])==config['libraries'][b]['sha256']
    for f,digest in config['source_sha256'].items():assert sha(source/f)==digest
    assert sha(__file__)==config['runner_sha256']
    save(out/'summary.json',summary);save(out/'comparison.json',dict(all_scales_faster=all(r['faster'] for r in comparison),scales=comparison))
    save(out/'environment.json',dict(finished=now(),cpu=subprocess.check_output(['sysctl','-n','machdep.cpu.brand_string'],text=True).strip(),os=subprocess.check_output(['sw_vers'],text=True),no_build_during_measurement=True))
    save(out/'status.json',dict(phase='complete',finished=now()))

if __name__=='__main__':main()
