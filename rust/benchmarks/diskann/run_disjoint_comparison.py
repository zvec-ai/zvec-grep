"""Measure six disjoint single-pass query shards; never warm or tune on test queries."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
from datetime import datetime
from zoneinfo import ZoneInfo

HERE=Path(__file__).resolve().parent
REPO=HERE.parents[2]
COUNTS=[200000,400000,600000,800000,1000000]

def sha(path):
    with Path(path).open('rb') as f:
        return hashlib.file_digest(f,'sha256').hexdigest()

def save(path,value):
    path.write_text(json.dumps(value,indent=2)+'\n')

def now():
    return datetime.now(ZoneInfo('Asia/Shanghai')).isoformat()

def main():
    p=argparse.ArgumentParser()
    p.add_argument('--base',type=Path,required=True)
    p.add_argument('--source',type=Path,required=True)
    a=p.parse_args()
    data=a.base/'data'
    output=a.base/'measure'
    assert not output.exists()
    old=json.loads((HERE/'results/node-comparison/measure/configuration.json').read_text())
    meta=json.loads((data/'manifest.json').read_text())
    assert meta['counts']==COUNTS
    for name,info in meta['files'].items():
        assert sha(data/name)==info['sha256'],name
    binary=a.base/'rust-target/release/disjoint-bench'
    library=binary.parent/'libzvec_c_api.dylib'
    assert sha(library)==old['libraries']['optimized']['sha256']
    assert subprocess.check_output(['git','-C',str(a.source),'rev-parse','HEAD'],text=True).strip()==old['source_commit']
    for f,digest in old['source_sha256'].items():
        assert sha(a.source/f)==digest
    for f,digest in old['microsoft_source_sha256'].items():
        assert sha(REPO/f)==digest
    assert sha(Path(old['pq_preparation'])/'codes.bin')==old['pq_codes_sha256']
    assert '@rpath/libzvec_c_api.dylib' in subprocess.check_output(['otool','-L',str(binary)],text=True)
    rpaths=re.findall(r'cmd LC_RPATH\s+cmdsize \d+\s+path (.*?) \(offset',subprocess.check_output(['otool','-l',str(binary)],text=True))
    assert rpaths[0]=='@executable_path'
    loader=json.loads((a.base/'loader.json').read_text())
    assert loader['overlap_rejected'] and len(loader['library_lines'])==1
    assert str(library).replace('/tmp/','/private/tmp/') in loader['library_lines'][0]
    def inventory():
        result={}
        for n in COUNTS:
            for f in (Path(old['indexes'])/f'diskann-{n}').rglob('*'):
                if f.is_file(): result[str(f)]=[f.stat().st_size,f.stat().st_mtime_ns]
            f=Path(old['microsoft_indexes'])/f'diskann_fp16-{n}-768/index'
            result[str(f)]=[f.stat().st_size,f.stat().st_mtime_ns]
        return result
    before=inventory()
    output.mkdir()
    (output/'raw').mkdir()
    source_files=['harness/src/bin/disjoint-bench.rs','harness/Cargo.toml','harness/Cargo.lock','prepare_disjoint.py','run_disjoint_comparison.py']
    config=dict(start=now(),base=str(a.base),source_commit=old['source_commit'],source=str(a.source),
                native_source_sha256=old['source_sha256'],microsoft_source_sha256=old['microsoft_source_sha256'],
                binary=dict(path=str(binary),sha256=sha(binary)),library=dict(path=str(library),sha256=sha(library)),
                source_files_sha256={f:sha(HERE/f) for f in source_files},data_manifest_sha256=sha(data/'manifest.json'),
                indexes=old['indexes'],microsoft_indexes=old['microsoft_indexes'],pq_preparation=old['pq_preparation'],pq_codes_sha256=old['pq_codes_sha256'],
                cache_mib=64,read_workers=8,query_concurrency=1,warmup_queries=600,validation_queries=400,
                test_queries=3000,shards=6,queries_per_shard=500,test_passes=1,test_warmup_passes=0,
                selection='start at previous L; raise only on independent 400-query validation until recall >= 92%; freeze before all test runs',
                schedule='alternate scale order by shard; backend order alternates by shard and scale',
                qps_definition='3000 / sum(single-pass timed seconds over six distinct shards)',
                rss_definition='median of six whole-process peak RSS values; MiB',
                os_file_cache='not purged; process/reader cache fresh each run',same_dylib_for_both=True)
    save(output/'configuration.json',config)
    save(output/'index-inventory.json',before)
    env={k:v for k,v in os.environ.items() if not k.startswith('DYLD_') and not k.startswith('ZVEC_DISKANN_')}
    runs=[]
    def invoke(backend,n,effort,phase,shard,budget=64,profile=False):
        query_name=f'test-{shard}' if phase=='measure' else 'validation'
        label=f'{phase}-{backend}-{n}-{shard}-L{effort}-cache{budget}'
        path=output/'raw'/f'{label}.jsonl'
        assert not path.exists()
        status=dict(backend=backend,count=n,effort=effort,phase=phase,shard=shard,budget=budget,time=now())
        print(json.dumps(status),flush=True)
        save(output/'status.json',status)
        index=Path(old['microsoft_indexes'])/f'diskann_fp16-{n}-768/index' if backend=='microsoft' else Path(old['indexes'])/f'diskann-{n}'
        command=['/usr/bin/time','-l',str(binary),backend,str(index),str(data/'warmup.f32'),str(data/f'{query_name}.f32'),str(n),str(effort),str(Path(old['pq_preparation'])/'codes.bin')]
        e=dict(env,ZVEC_DISKANN_READ_CACHE_MIB=str(budget if backend=='zvec' else 0),ZVEC_DISKANN_READ_WORKERS=str(8 if backend=='zvec' else 1))
        if profile: e['ZVEC_DISKANN_PROFILE_READS']='1'
        with path.open('w') as out,path.with_suffix('.stderr').open('w') as err:
            subprocess.run(command,env=e,stdout=out,stderr=err,check=True)
        raw=[json.loads(line) for line in path.read_text().splitlines()]
        row=next(x for x in raw if x['stage']=='queried')
        ids=next(x['ids'] for x in raw if x['stage']=='results')
        truth=json.loads((data/f'{query_name}-truth-{n}.json').read_text())
        q=500 if phase=='measure' else 400
        assert len(ids)==len(truth)==row['queries']==q
        assert all(len(set(x))==10 for x in ids)
        assert row['warmup_queries']==600 and row['test_passes']==1 and row['test_warmup_passes']==0
        hits=sum(len(set(x)&set(y)) for x,y in zip(ids,truth))
        row.update(phase=phase,shard=shard,cache_mib=budget if backend=='zvec' else 0,profile=profile,
                   recall_at_10=hits/(q*10),recall_hits=hits,
                   rss_mib=int(re.search(r'(\d+)\s+maximum resident set size',path.with_suffix('.stderr').read_text()).group(1))/2**20,
                   cpu_s=row['measured_resources']['user_s']+row['measured_resources']['system_s'],
                   major_faults=row['measured_resources']['major_faults'],raw_file=str(path.relative_to(output)),finished=now(),
                   results_sha256=hashlib.sha256(json.dumps(ids,separators=(',',':')).encode()).hexdigest())
        runs.append(row)
        save(output/'runs.json',runs)
        if phase=='measure': assert row['recall_at_10']>=.90, row['recall_at_10']
        return row
    settings={}
    for n in COUNTS:
        for backend in ['zvec','microsoft']:
            initial=(96 if n<1000000 else 128) if backend=='zvec' else 128
            for effort in [initial]+[v for v in [128,160,192,256,384,512] if v>initial]:
                row=invoke(backend,n,effort,'validation',0)
                if row['recall_at_10']>=.92:
                    settings[f'{backend}-{n}']=dict(effort=effort,validation_recall=row['recall_at_10'])
                    break
            else: raise RuntimeError('Recall target not reached on validation')
    save(output/'selection.json',dict(selected_at=now(),settings=settings,threshold=.92,test_results_seen=False))
    for shard in range(6):
        for position,n in enumerate(COUNTS if shard%2==0 else COUNTS[::-1]):
            backends=['zvec','microsoft'] if (shard+position)%2==0 else ['microsoft','zvec']
            for backend in backends:
                invoke(backend,n,settings[f'{backend}-{n}']['effort'],'measure',shard)
    # RSS attribution probes use only validation queries, after formal timing.
    for repeat in range(3):
        for budget in ([0,64] if repeat%2==0 else [64,0]):
            invoke('zvec',1000000,settings['zvec-1000000']['effort'],'memory',repeat,budget)
    invoke('zvec',1000000,settings['zvec-1000000']['effort'],'profile',0,64,True)
    summary=[]
    for n in COUNTS:
        for backend in ['zvec','microsoft']:
            group=[r for r in runs if r['phase']=='measure' and r['count']==n and r['backend']==backend]
            assert len(group)==6
            row=dict(count=n,backend=backend,queries=3000,shards=6,effort=settings[f'{backend}-{n}']['effort'],
                     qps=3000/sum(r['batch_s'] for r in group),recall_at_10=sum(r['recall_hits'] for r in group)/30000)
            for metric in ['qps','rss_mib','recall_at_10','p50_ms','p95_ms','cpu_s','major_faults','rss_peak_before_open_mib','rss_peak_after_open_mib','rss_peak_after_warmup_mib','rss_peak_after_test_mib']:
                vals=[r[metric] for r in group]
                if metric not in ['qps','recall_at_10']: row[metric]=statistics.median(vals)
                row[metric+'_median']=statistics.median(vals)
                row[metric+'_min']=min(vals)
                row[metric+'_max']=max(vals)
            summary.append(row)
    assert inventory()==before
    assert sha(binary)==config['binary']['sha256'] and sha(library)==config['library']['sha256']
    for f,digest in config['source_files_sha256'].items(): assert sha(HERE/f)==digest
    save(output/'summary.json',summary)
    save(output/'environment.json',dict(finished=now(),cpu=subprocess.check_output(['sysctl','-n','machdep.cpu.brand_string'],text=True).strip(),
         memory_bytes=int(subprocess.check_output(['sysctl','-n','hw.memsize'],text=True)),os=subprocess.check_output(['sw_vers'],text=True),
         indexes_unchanged=True,no_build_or_gt_computation_during_measurement=True))
    save(output/'status.json',dict(phase='complete',finished=now()))
    print('DISJOINT MEASUREMENT COMPLETE',flush=True)

if __name__=='__main__': main()
