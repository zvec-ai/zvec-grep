"""Fresh paired read-cache experiment; select budgets on validation queries only."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import statistics
import shutil
import subprocess
from datetime import datetime
from zoneinfo import ZoneInfo

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
COUNTS = [200000, 400000, 600000, 800000, 1000000]
METRICS = ['qps', 'rss_mib', 'recall_at_10', 'batch_s', 'cpu_s', 'open_s',
           'p50_ms', 'p95_ms', 'measured_major_faults', 'voluntary_switches']

def sha(path):
    with Path(path).open('rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()

def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')

def now():
    return datetime.now(ZoneInfo('Asia/Shanghai')).isoformat()

def main():
    p = argparse.ArgumentParser()
    p.add_argument('--base', type=Path, required=True)
    p.add_argument('--data', type=Path, required=True)
    p.add_argument('--indexes', type=Path, required=True)
    p.add_argument('--source', type=Path, required=True)
    p.add_argument('--stage', choices=['pilot', 'measure'], required=True)
    a = p.parse_args()
    output = a.base / a.stage
    assert not output.exists(), output
    backends = ['baseline', 'optimized'] + (['microsoft'] if a.stage == 'measure' else [])
    binaries = {name: a.base / name / 'native-retest' for name in backends}
    libraries = {name: a.base / name / 'libzvec_c_api.dylib' for name in binaries}
    old = json.loads((HERE / 'results/microsoft-comparison/configuration.json').read_text())
    assert sha(libraries['baseline']) == old['native_library_sha256']
    for name, binary in binaries.items():
        linkage = subprocess.check_output(['otool', '-L', str(binary)], text=True)
        assert str(libraries[name]) + ' (compatibility' in linkage
    for folder in [a.data, a.data / 'validation']:
        for filename, meta in json.loads((folder / 'manifest.json').read_text())['files'].items():
            assert sha(folder / filename) == meta['sha256'], filename
    assert sha(a.data / 'manifest.json') == old['data_manifest_sha256']
    settings = {r['count']: r for r in old['parameters'] if r['backend'] == 'zvec'}
    ms_settings = {r['count']: r for r in old['parameters'] if r['backend'] == 'microsoft'}
    ms_indexes = Path(old['microsoft_experiment']) / 'indexes'
    pq = Path(old['pq_preparation'])
    if a.stage == 'measure':
        assert sha(pq / 'codes.bin') == old['pq_codes_sha256']
        assert sha(pq / 'manifest.json') == old['pq_preparation_manifest_sha256']
        for filename, meta in json.loads((pq / 'manifest.json').read_text())['files'].items():
            assert sha(pq / filename) == meta['sha256']
        ms_original = json.loads((Path(old['pq_experiment']) / 'configuration.json').read_text())
        for n in COUNTS:
            f = ms_indexes / f'diskann_fp16-{n}-768' / 'index'
            previous = ms_original['index_files_before'][str(n)]
            assert [f.stat().st_size, f.stat().st_mtime_ns] == [previous['bytes'], previous['mtime_ns']]

    def inventory():
        result = {str(f.relative_to(a.indexes)): [f.stat().st_size, f.stat().st_mtime_ns]
                  for n in COUNTS for f in (a.indexes / f'diskann-{n}').rglob('*') if f.is_file()}
        if a.stage == 'measure':
            for n in COUNTS:
                f = ms_indexes / f'diskann_fp16-{n}-768' / 'index'
                result[f'microsoft/{n}'] = [f.stat().st_size, f.stat().st_mtime_ns]
        return result
    inventory_before = inventory()
    changed = ['src/core/algorithm/diskann/diskann_read_cache.h',
               'src/core/algorithm/diskann/diskann_file_reader.h',
               'src/core/algorithm/diskann/diskann_file_reader.cc',
               'src/core/algorithm/diskann/diskann_indexer.cc']
    ms_sources = {str(f.relative_to(REPO)):sha(f) for f in (REPO/'rust/crates/zg-diskann/src').rglob('*.rs')}
    config = dict(start=now(), stage=a.stage, counts=COUNTS, settings=settings,
                  backends=backends, microsoft_settings=ms_settings,
                  microsoft_indexes=str(ms_indexes), pq_preparation=str(pq),
                  pq_codes_sha256=old['pq_codes_sha256'], microsoft_source_sha256=ms_sources,
                  microsoft_harness_sha256=sha(HERE/'harness/src/bin/microsoft-retest.rs'),
                  schedule='six rounds; alternate scale order and rotate/reverse backend order',
                  data=str(a.data), data_manifest_sha256=sha(a.data / 'manifest.json'),
                  indexes=str(a.indexes), source=str(a.source),
                  source_base_commit='960ecbc23f566df729429bdff655ece6dbb2261c',
                  source_commit=subprocess.check_output(['git', '-C', str(a.source), 'rev-parse', 'HEAD'], text=True).strip(),
                  source_sha256={f:sha(a.source / f) for f in changed},
                  harness_sha256=sha(HERE / 'harness/src/bin/native-retest.rs'),
                  runner_sha256=sha(__file__),
                  binaries={k:dict(path=str(v), sha256=sha(v)) for k,v in binaries.items()},
                  libraries={k:dict(path=str(v), sha256=sha(v)) for k,v in libraries.items()},
                  query_concurrency=1, num_threads=1, warmup_independent=20,
                  warmup_full_passes=2, measured_passes=5, distinct_queries=200,
                  top_k=10, metric='cosine', precision='FP16', pq_chunks=64,
                  search_parameters_unchanged=True, graph_and_pq_unchanged=True,
                  selection_rule='smallest budget reaching 90% of best geometric-mean validation QPS across five scales; 2 processes per budget/scale',
                  cache_scope='fixed per file reader; F_NOCACHE retained for misses',
                  cache_budget_candidates_mib=[16,32,64], file_cache='not purged')
    output.mkdir()
    (output/'raw').mkdir()
    save(output/'configuration.json',config)
    shutil.copy2(__file__, output/'runner.py')
    save(output/'index-inventory.json',inventory_before)
    patch = subprocess.check_output(['git', '-C', str(a.source), 'diff', '960ecbc2', '--', 'src/core/algorithm/diskann'], text=True)
    (output/'source.patch').write_text(patch)
    runs=[]
    env = {k:v for k,v in os.environ.items() if k not in ['DYLD_LIBRARY_PATH','DYLD_FALLBACK_LIBRARY_PATH','DYLD_INSERT_LIBRARIES','ZVEC_DISKANN_READ_CACHE_MIB']}
    def invoke(backend, n, repeat, phase, budget):
        status=dict(stage=phase, backend=backend, count=n, repeat=repeat, budget_mib=budget, updated=now())
        save(output/'status.json',status)
        print(json.dumps(status),flush=True)
        folder=a.data/'validation' if phase=='validation' else a.data
        path=output/'raw'/f'{phase}-{backend}-{budget}-{n}-{repeat}.jsonl'
        setting = ms_settings[n] if backend == 'microsoft' else settings[n]
        command=['/usr/bin/time','-l',str(binaries[backend])]
        if backend == 'microsoft':
            command += [str(ms_indexes/f'diskann_fp16-{n}-768'/'index'),str(folder/'queries.f32'),str(n),str(setting['effort']),str(pq/'codes.bin')]
        else:
            command += ['diskann',str(a.indexes/f'diskann-{n}'),str(folder/'queries.f32'),str(n),str(setting['effort'])]
        process_env=dict(env,ZVEC_DISKANN_READ_CACHE_MIB=str(budget))
        with path.open('w') as out,path.with_suffix('.stderr').open('w') as err:
            subprocess.run(command,stdout=out,stderr=err,env=process_env,check=True)
        raw=[json.loads(line) for line in path.read_text().splitlines()]
        row=next(r for r in raw if r['stage']=='queried')
        ids=next(r['ids'] for r in raw if r['stage']=='results')
        truth=json.loads((folder/f'truth-{n}.json').read_text())
        assert len(ids)==len(truth)==200 and all(len(set(x))==10 for x in ids)
        recall=sum(len(set(x)&set(y)) for x,y in zip(ids,truth))/2000
        assert recall>=.90
        if phase=='validation': assert recall==setting['expected_validation_recall']
        assert row['count']==n and row['effort']==setting['effort'] and row['queries']==1000
        measured=row['phases'][2:]
        assert len(measured)==5 and all(r['phase']=='measured' for r in measured)
        row.update(backend=backend,budget_mib=budget,count=n,repeat=repeat,phase=phase,
                   recall_at_10=recall,results_sha256=hashlib.sha256(json.dumps(ids,separators=(',',':')).encode()).hexdigest(),
                   rss_mib=int(re.search(r'(\d+)\s+maximum resident set size',path.with_suffix('.stderr').read_text()).group(1))/2**20,
                   cpu_s=sum(r['resources']['user_s']+r['resources']['system_s'] for r in measured),
                   measured_major_faults=sum(r['resources']['major_faults'] for r in measured),
                   voluntary_switches=sum(r['resources']['voluntary_switches'] for r in measured),
                   raw_file=str(path.relative_to(output)),finished=now())
        # Every implementation and budget must produce the same ordered IDs.
        peers=[r for r in runs if r['count']==n and r['phase']==phase and r['backend']!='microsoft']
        if backend != 'microsoft':
            assert all(r['results_sha256']==row['results_sha256'] for r in peers)
        else:
            assert row['rerank']==128 and row['pq_codes_bytes']==n*64
        runs.append(row)
        save(output/'runs.json',runs)
    if a.stage=='pilot':
        for n in COUNTS: invoke('baseline',n,0,'validation',0)
        for repeat in range(2):
            for n in COUNTS if repeat==0 else COUNTS[::-1]:
                for budget in ([16,32,64] if repeat==0 else [64,32,16]):
                    invoke('optimized',n,repeat,'validation',budget)
        scores={budget:math.exp(statistics.mean(math.log(statistics.median(r['qps'] for r in runs if r['budget_mib']==budget and r['count']==n)) for n in COUNTS)) for budget in [16,32,64]}
        chosen=min(b for b in scores if scores[b]>=max(scores.values())*.9)
        save(output/'selection.json',dict(budget_mib=chosen,validation_geometric_mean_qps=scores,rule=config['selection_rule'],frozen_at=now()))
    else:
        selection=json.loads((a.base/'pilot/selection.json').read_text())
        budget=selection['budget_mib']
        save(output/'selection.json',selection)
        # Revalidate all three final binaries before any test-set timing.
        for n in COUNTS:
            invoke('baseline',n,0,'validation',0)
            invoke('optimized',n,0,'validation',budget)
            invoke('microsoft',n,0,'validation',0)
        for repeat in range(6):
            for n in COUNTS if repeat%2==0 else COUNTS[::-1]:
                order=backends[repeat%3:]+backends[:repeat%3]
                if repeat%2: order=order[::-1]
                for backend in order:
                    invoke(backend,n,repeat,'measure',budget if backend=='optimized' else 0)
    summary=[]
    for n in COUNTS:
        groups=sorted({(r['backend'],r['budget_mib']) for r in runs if r['phase']==('validation' if a.stage=='pilot' else 'measure')})
        for backend,budget in groups:
            group=[r for r in runs if r['count']==n and r['backend']==backend and r['budget_mib']==budget and r['phase']==('validation' if a.stage=='pilot' else 'measure')]
            row=dict(count=n,backend=backend,budget_mib=budget,repeats=len(group))
            for metric in METRICS:
                values=[r[metric] for r in group]
                row.update({metric:statistics.median(values),metric+'_min':min(values),metric+'_max':max(values)})
            summary.append(row)
    assert inventory()==inventory_before
    assert all(sha(v)==config['binaries'][k]['sha256'] for k,v in binaries.items())
    assert all(sha(v)==config['libraries'][k]['sha256'] for k,v in libraries.items())
    assert all(sha(a.source/f)==digest for f,digest in config['source_sha256'].items())
    assert sha(__file__)==config['runner_sha256']
    assert all(sha(REPO/f)==digest for f,digest in ms_sources.items())
    save(output/'summary.json',summary)
    save(output/'environment.json',dict(finished=now(),cpu=subprocess.check_output(['sysctl','-n','machdep.cpu.brand_string'],text=True).strip(),
         memory_bytes=int(subprocess.check_output(['sysctl','-n','hw.memsize'],text=True)),os=subprocess.check_output(['sw_vers'],text=True).strip(),
         no_build_during_measurement=True,unchanged_indexes=True))
    save(output/'status.json',dict(stage='complete',finished=now()))
    print('Complete',flush=True)

if __name__=='__main__': main()
