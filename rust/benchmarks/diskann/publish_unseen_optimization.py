"""Audit frozen unseen-query optimization, then publish report and static figures."""
import argparse,gzip,hashlib,json,re,shutil,statistics,subprocess
from pathlib import Path
import numpy as np

HERE=Path(__file__).resolve().parent
REPO=HERE.parents[2]
COUNTS=[600000,800000,1000000]
PILOTS=['beam-pilot','cpu-pilot','queue-pilot','wide-pilot','effort-pilot','wake-pilot','clock-pilot','final-tuning']

def sha(p):
    with Path(p).open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def save(p,x):p.write_text(json.dumps(x,ensure_ascii=False,indent=2)+'\n')
def near(a,b):assert abs(a-b)<1e-8*max(1,abs(a),abs(b)),(a,b)

def main():
    p=argparse.ArgumentParser();p.add_argument('--base',type=Path,required=True);a=p.parse_args()
    base=a.base;dest=HERE/'results/unseen-optimization';dest.mkdir(exist_ok=True)
    data=Path('/tmp/zvec-disjoint-cohere-20261105/data');hm=json.loads((base/'holdout/manifest.json').read_text());dm=json.loads((data/'manifest.json').read_text())
    config=json.loads((base/'measure/configuration.json').read_text());source=Path(config['source'])
    assert json.loads((base/'measure/status.json').read_text())['phase']=='complete'
    assert config['source_commit']=='047ebaf470a5594790b1f70e7c69c956885466d8'
    for f,digest in config['source_sha256'].items():
        assert sha(source/f)==digest
        assert hashlib.sha256(subprocess.check_output(['git','show',f"{config['source_commit']}:{f}"],cwd=source)).hexdigest()==digest
    for f,digest in config['microsoft_source_sha256'].items():assert sha(REPO/f)==digest
    for field in ['binaries','libraries']:
        for item in config[field].values():assert sha(item['path'])==item['sha256']
    assert sha(HERE/'harness/src/bin/disjoint-bench.rs')==config['harness_sha256']
    assert sha(HERE/'run_unseen_comparison.py')==config['runner_sha256']
    assert sha(data/'manifest.json')==config['data_manifest_sha256']
    assert sha(base/'holdout/manifest.json')==config['holdout_manifest_sha256']
    for directory,meta in [(data,dm),(base/'holdout',hm)]:
        for name,info in meta['files'].items():assert sha(directory/name)==info['sha256']
    old_rows=set(dm['previous_query_row_ids'])
    rows32=set();rows16=set()
    fingerprints={}
    for name,ids in dm['query_source_row_ids'].items():
        assert not old_rows.intersection(ids);old_rows.update(ids)
        x=np.fromfile(data/f'{name}.f32',dtype='<f4').reshape(-1,768);fingerprints[name]=[]
        for row in x:
            k=hashlib.sha256(row.tobytes()).hexdigest();k16=hashlib.sha256(row.astype('<f2').tobytes()).hexdigest()
            assert k not in rows32 and k16 not in rows16;rows32.add(k);rows16.add(k16);fingerprints[name].append(k)
    assert len(old_rows)==4440 and not old_rows.intersection(hm['query_source_row_ids'])
    pool_file=Path(dm['source_query_file']);assert sha(pool_file)==hm['source_queries_sha256']
    pool=np.fromfile(pool_file,dtype='<f4').reshape(5000,768).astype(np.float64)
    pool=(pool/np.linalg.norm(pool,axis=1)[:,None]).astype('<f4')
    held=np.fromfile(base/'holdout/queries.f32',dtype='<f4').reshape(500,768)
    assert np.array_equal(held,pool[hm['query_source_row_ids']])
    fingerprints['holdout']=[]
    for row in held:
        k=hashlib.sha256(row.tobytes()).hexdigest();k16=hashlib.sha256(row.astype('<f2').tobytes()).hexdigest()
        assert k not in rows32 and k16 not in rows16;rows32.add(k);rows16.add(k16);fingerprints['holdout'].append(k)
    assert len(rows32)==len(rows16)==4500
    # New GT: scalar dot products and full stable sorting for three queries.
    bfile=Path('/tmp/zvec-cohere-million/data/base.f32');assert sha(bfile)==hm['base_sha256']
    b=np.memmap(bfile,dtype='<f4',mode='r',shape=(1000000,768));positions=[0,217,499]
    q=held[positions].astype(np.float64);q/=np.linalg.norm(q,axis=1)[:,None]
    accumulated=[[] for _ in positions];checks=[]
    for end in range(20000,1000001,20000):
        block=b[end-20000:end].astype(np.float64);block/=np.linalg.norm(block,axis=1)[:,None]
        for i in range(len(positions)):accumulated[i].extend(np.einsum('ij,j->i',block,q[i],optimize=False).tolist())
        if end in COUNTS:
            truth=json.loads((base/f'holdout/truth-{end}.json').read_text())
            for i,position in enumerate(positions):
                order=np.argsort(-np.asarray(accumulated[i]),kind='stable')[:10]
                assert truth[position]==[f'v{x:08}' for x in order]
            checks.append(dict(count=end,query_positions=positions,method='independent scalar dot and full stable sort',passed=True))
    records=[];by_stage={};paired=[]
    for stage in PILOTS+['validation','measure']:
        folder=base/stage;rows=json.loads((folder/'runs.json').read_text());by_stage[stage]=rows
        for r in rows:
            raw_path=folder/r['raw_file'];raw=[json.loads(line) for line in raw_path.read_text().splitlines()]
            actual=next(x for x in raw if x['stage']=='queried');ids=next(x['ids'] for x in raw if x['stage']=='results')
            phase=r.get('phase','pilot')
            gtfile=base/f"holdout/truth-{r['count']}.json" if phase=='holdout' else (data/f"test-{r['repeat']}-truth-{r['count']}.json" if phase=='regression' else data/f"validation-truth-{r['count']}.json")
            gt=json.loads(gtfile.read_text());nq=500 if phase in ['holdout','regression'] else 400
            assert len(ids)==len(gt)==actual['queries']==nq and all(len(x)==len(set(x))==10 for x in ids)
            assert all(re.fullmatch(r'v\d{8}',x) and int(x[1:])<r['count'] for group in ids for x in group)
            assert actual['test_passes']==1 and actual['test_warmup_passes']==0 and actual['warmup_queries']==600
            hits=sum(len(set(x)&set(y)) for x,y in zip(ids,gt));recall=hits/(nq*10)
            near(r.get('recall_at_10',r.get('recall')),recall);near(r['qps'],nq/actual['batch_s'])
            near(r['batch_s'],actual['batch_s'])
            key='results_sha256' if 'results_sha256' in r else 'ids_sha256'
            assert hashlib.sha256(json.dumps(ids,separators=(',',':')).encode()).hexdigest()==r[key]
            stderr=raw_path.with_suffix('.stderr').read_text()
            near(r['rss_mib'],int(re.search(r'(\d+)\s+maximum resident set size',stderr).group(1))/2**20)
            assert r['rss_mib']>=actual['rss_peak_after_test_mib']
            times=sorted(actual['latencies_ms']);assert len(times)==nq and all(t>0 for t in times)
            near(actual['p50_ms'],times[nq//2]);near(actual['p95_ms'],times[nq*95//100])
            assert sum(times)/1000<=actual['batch_s']*1.001
            if stage=='measure':
                assert recall>=.90 and r['finished']>config['start']
                assert r['effort']==(128 if r['backend']=='microsoft' else config['selection']['settings'][str(r['count'])]['effort'])
                assert r['workers']==(1 if r['backend']=='microsoft' else 24)
            records.append(dict(stage=stage,path=r['raw_file'],jsonl=raw_path.read_text(),stderr=stderr))
        if stage in ['validation','measure']:
            stage_config=json.loads((folder/'configuration.json').read_text())
            assert stage_config['source_sha256']==config['source_sha256'] and stage_config['libraries']==config['libraries']
            for file,stat in json.loads((folder/'index-inventory.json').read_text()).items():assert [Path(file).stat().st_size,Path(file).stat().st_mtime_ns]==stat
            summaries=json.loads((folder/'summary.json').read_text())
            for item in summaries:
                g=[r for r in rows if r['phase']==item['phase'] and r['count']==item['count'] and r['backend']==item['backend']]
                assert len(g)==item['processes'] and sum(r['queries'] for r in g)==item['queries']
                near(item['qps'],sum(r['queries'] for r in g)/sum(r['batch_s'] for r in g));near(item['rss_mib'],statistics.median(r['rss_mib'] for r in g))
                near(item['recall_at_10'],sum(r['recall_hits'] for r in g)/sum(r['queries']*10 for r in g));near(item['recall_min'],min(r['recall_at_10'] for r in g))
            target=dest/stage;target.mkdir(exist_ok=True)
            for filename in ['configuration.json','runs.json','summary.json','comparison.json','index-inventory.json','environment.json','source.patch']:shutil.copy2(folder/filename,target/filename)
    assert len(by_stage['validation'])==35 and len(by_stage['measure'])==42
    # Equal-L before/after controls must return identical ordered results.
    v=by_stage['validation'];formal=by_stage['measure']
    for n in COUNTS:
        for effort in [128,config['selection']['settings'][str(n)]['effort']]:
            group=[r for r in v if r['backend']!='microsoft' and r['count']==n and r['effort']==effort]
            assert len({r['results_sha256'] for r in group})==1
        for phase in ['holdout','regression']:
            z=[r for r in formal if r['phase']==phase and r['backend']=='zvec' and r['count']==n]
            m=[r for r in formal if r['phase']==phase and r['backend']=='microsoft' and r['count']==n]
            assert len(z)==len(m)==(1 if phase=='holdout' else 6)
            assert sorted(r['repeat'] for r in z)==([0] if phase=='holdout' else list(range(6)))
            assert sorted(r['repeat'] for r in m)==sorted(r['repeat'] for r in z)
            ratios=[r['qps']/next(x['qps'] for x in m if x['repeat']==r['repeat']) for r in z]
            pooled=sum(r['batch_s'] for r in m)/sum(r['batch_s'] for r in z)
            assert pooled>1
            paired.append(dict(phase=phase,count=n,qps_ratio=pooled,paired_wins=sum(x>1 for x in ratios),pairs=len(z),minimum_pair_ratio=min(ratios)))
    # Keep every diagnostic candidate, including those that did not help.
    for variant in PILOTS:
        d=dest/'pilots'/variant;d.mkdir(parents=True,exist_ok=True)
        for name in ['configuration.json','runs.json','source.patch']:shutil.copy2(base/variant/name,d/name)
    for name in ['cpp-tests.json','selection-draft.json','rust-build.log']:
        shutil.copy2(base/name,dest/('rust-build.txt' if name.endswith('.log') else name))
    shutil.copy2(base/'holdout/manifest.json',dest/'holdout-manifest.json')
    shutil.copy2(base/'final/build.json',dest/'build.json')
    shutil.copy2(base/'final/build.txt',dest/'build.txt')
    tests=json.loads((dest/'cpp-tests.json').read_text());assert tests['total_passed']==43 and tests['source_commit']==config['source_commit']
    patch=subprocess.check_output(['git','diff','7d3a413e',config['source_commit']],cwd=source)
    assert (dest/'measure/source.patch').read_bytes()==patch
    truth={str(f.relative_to(base)):json.loads(f.read_text()) for f in (base/'holdout').glob('truth-*.json')}
    for n in COUNTS:
        for name in ['validation']+[f'test-{i}' for i in range(6)]:truth[f'data/{name}-truth-{n}.json']=json.loads((data/f'{name}-truth-{n}.json').read_text())
    evidence=dict(records=records,truth=truth,query_fingerprints=fingerprints)
    with (dest/'raw-evidence.json.gz').open('wb') as f:
        with gzip.GzipFile(fileobj=f,mode='wb',filename='',mtime=0) as g:g.write(json.dumps(evidence,separators=(',',':')).encode())
    audit=dict(passed=True,records_checked=len(records),pilot_records=sum(len(by_stage[x]) for x in PILOTS),validation_records=35,formal_records=42,
               fresh_holdout_queries=500,excluded_previous_source_rows=4440,unique_fp32_and_fp16=True,
               test_passes=1,test_warmup_passes=0,gt_checks=checks,source_binary_and_index_checks=True,
               equal_L_ordered_results_unchanged=True,comparisons=paired,raw_evidence_sha256=sha(dest/'raw-evidence.json.gz'))
    save(dest/'audit.json',audit)
    save(dest/'startup-failure.json',dict(action='copy previously built baseline executable into a new directory',observed_exit_code=137,
         stdout_bytes=(base/'baseline-smoke.jsonl').stat().st_size,stderr=(base/'baseline-smoke.stderr').read_text(),
         resolution='use the original working baseline path and a freshly linked candidate executable; no formal test queries were executed by the failed process'))
    summary=json.loads((base/'measure/summary.json').read_text());vs=json.loads((base/'validation/summary.json').read_text())
    def row(n,b,phase='holdout'):return next(x for x in summary if x['count']==n and x['backend']==b and x['phase']==phase)
    def val(n,b):return next(x for x in vs if x['count']==n and x['backend']==b)
    def table(phase):return '\n'.join(f"| {n//10000} 万 | {row(n,'zvec',phase)['qps']:.2f} | {row(n,'microsoft',phase)['qps']:.2f} | {(row(n,'zvec',phase)['qps']/row(n,'microsoft',phase)['qps']-1)*100:.2f}% | {row(n,'zvec',phase)['rss_mib']:.2f} / {row(n,'microsoft',phase)['rss_mib']:.2f} | {row(n,'zvec',phase)['recall_at_10']:.2%} / {row(n,'microsoft',phase)['recall_at_10']:.2%} |" for n in COUNTS)
    controls='\n'.join(f"| {n//10000} 万 | {config['selection']['settings'][str(n)]['effort']} | {val(n,'baseline')['qps']:.2f} | {val(n,'zvec')['qps']:.2f} | {(val(n,'zvec')['qps']/val(n,'baseline')['qps']-1)*100:.2f}% | {val(n,'zvec')['recall_at_10']:.2%} |" for n in COUNTS)
    ranges='\n'.join(f"| {n//10000} 万 | {row(n,'zvec','regression')['qps_min']:.2f}–{row(n,'zvec','regression')['qps_max']:.2f} | {row(n,'microsoft','regression')['qps_min']:.2f}–{row(n,'microsoft','regression')['qps_max']:.2f} | {next(x['paired_wins'] for x in paired if x['phase']=='regression' and x['count']==n)} / 6 |" for n in COUNTS)
    oldprofile=next(r['read_profile'] for r in v if r['phase']=='profile128' and r['backend']=='baseline')
    newprofile=next(r['read_profile'] for r in v if r['phase']=='profile128' and r['backend']=='zvec')
    z=row(1000000,'zvec');m=row(1000000,'microsoft');minimum=min(r['recall_at_10'] for r in formal)
    report=f'''# Cohere：zvec DiskANN 未知查询优化验证

历史配置：本文保留源码 `047ebaf4`、64 MiB 缓存的结果；后续降低 RSS 的配置与单遍回归见[最新报告](REPORT_NODE_CN.md)。

测试日期：2026-11-05。Apple M3 Pro、36 GiB、macOS；本地 zvec 源码 `{config['source_commit'][:8]}`。

**60 万、80 万、100 万三个规模均达到本轮目标。** 在代码和参数冻结后，用此前从未使用的 500 条查询单遍确认，zvec 的 QPS 比微软方案高 **15.60%–18.86%**，Recall@10 为 **91.96%–93.20%**。随后对既有 3,000 条查询做单遍回归，三个规模合并 QPS 仍高 **8.63%–18.62%**。此前[未知查询基线](REPORT_UNSEEN_BASELINE_CN.md)中大规模落后的结论已由本轮结果更新。

缓存预算仍为每个图文件 **64 MiB**，保留 `F_NOCACHE` 和关闭预读。测试使用 **24 个有上限的 I/O 工作线程**，查询并发仍为 1。百万新查询组 RSS 为 **{z['rss_mib']:.2f} 对 {m['rss_mib']:.2f} MiB**，zvec 的进程内存仍高于微软方案。

## 全新查询：独立确认

这 500 条查询来自同一 Cohere 官方查询池，排除了此前使用的 4,440 条源查询，未参与任何候选版本测试、参数选择或预热。每个实现、每个规模均开新进程，先执行独立预热集 600 条一次，再执行这 500 条一次。每组只有一次计时，不把它称为多轮复测。

| 向量数 | zvec DiskANN QPS | 微软 QPS | zvec QPS 提升 | RSS：zvec / 微软（MiB） | Recall@10：zvec / 微软 |
|---|---:|---:|---:|---:|---:|
{table('holdout')}

![全新查询的 QPS 与 RSS](results/unseen-optimization/qps-memory-holdout.png)

## 既有查询：单遍回归

既有 3,000 条查询曾在上一轮测试中使用，因此单列为回归集，不称为新的盲测集。本轮没有使用这些查询调参。六个互不重叠的分片各 500 条，每个新进程先执行 600 条独立预热，再执行自己的分片一次。QPS = 3,000 / 六个计时批次的总秒数；RSS 为六个进程峰值中位数。

| 向量数 | zvec DiskANN QPS | 微软 QPS | zvec QPS 提升 | RSS：zvec / 微软（MiB） | Recall@10：zvec / 微软 |
|---|---:|---:|---:|---:|---:|
{table('regression')}

![单遍回归查询的 QPS 与 RSS](results/unseen-optimization/qps-memory-regression.png)

| 向量数 | zvec 分片 QPS 范围 | 微软分片 QPS 范围 | zvec 配对领先分片数 |
|---|---:|---:|---:|
{ranges}

三个规模的合并吞吐量均领先，但 80 万、100 万各有一个分片略低于微软；这不代表每个分片或每条请求都更快。六个分片使用不同查询，范围也包含查询难度差异，不能当作重复测量置信区间。百万微软回归组的一次计时记录到 1 次 major fault，所有记录均保留。全部 42 个正式进程的最低 Recall@10 为 **{minimum:.2%}**。

## 定位到的瓶颈与代码改动

**主要瓶颈是读取批次的串行等待。** 原版允许最多 8 个读取工作线程，但 L=128 时一个搜索批次最多扩展约 25 个候选，未命中时经常要分几轮读取。本轮把可配置上限扩展到 32，并在独立验证集选择 24。验证中继续增到 32 没有持续提高性能，未采用无上限线程数。

**降低线程池调度开销。** 工作线程按批次领取任务，再用批次内原子索引领取请求，避免每次读取前后争用全局队列锁。小批次只唤醒所需线程。仍在所有读取完成后才返回，错误也会完整回收，保持原节点处理顺序。

**减少热点节点被一次性访问淘汰。** 固定预算、最多 16 路的缓存采用有上限的多次 Clock 保留机会：新节点 1 次，重复命中最多 3 次，扫描时逐次扣减。没有扩大缓存，也没有缓存查询答案。

**消除重复计算和分配。** 访问过的邻居在 PQ 计算前排除，复用邻居和距离临时数组；只有可能进入 top-k 的候选才复制原向量。相同 L 的旧版与新版返回有序结果逐项一致。只做这类 CPU 改动时 QPS 提升很小，主要收益仍来自提高读取并发。

百万独立验证诊断固定 L=128，均包含 600 条预热和 400 条验证，逻辑请求都是 {oldprofile['requests']:,} 次：

| 指标 | 旧版：8 个 I/O 线程 | 新版：24 个 I/O 线程 |
|---|---:|---:|
| 实际 pread 次数 | {oldprofile['pread_syscalls']:,} | {newprofile['pread_syscalls']:,} |
| API 读取量（MiB） | {oldprofile['read_bytes']/2**20:.2f} | {newprofile['read_bytes']/2**20:.2f} |
| 读取批次累计墙钟（秒） | {oldprofile['batch_ns']/1e9:.3f} | {newprofile['batch_ns']/1e9:.3f} |
| 各读取线程耗时之和（秒） | {oldprofile['read_ns']/1e9:.3f} | {newprofile['read_ns']/1e9:.3f} |

批次等待降低 **{(1-newprofile['batch_ns']/oldprofile['batch_ns'])*100:.2f}%**，实际读取次数降低 **{(1-newprofile['pread_syscalls']/oldprofile['pread_syscalls'])*100:.2f}%**。线程耗时之和没有下降，说明收益主要来自等待重叠，不能解释为每次 SSD 读取都变快。诊断覆盖预热和验证全过程，正式 QPS 测量关闭诊断。

## 区分代码收益与搜索参数收益

下表在同轮独立验证集上比较，每组各 3 个新进程，QPS 按总查询数 / 总计时计算。两版使用同一 L、同一图、PQ、精度和 64 MiB 缓存，旧版 8 个、新版 24 个 I/O 工作线程；有序结果相同。因此这里的提升来自代码与读取并发配置，不依赖降低召回率。

| 向量数 | 两版共同 L | 旧版 QPS | 新版 QPS | 提升 | 两版共同 Recall@10 |
|---|---:|---:|---:|---:|---:|
{controls}

另固定百万组 L=128 的单次验证对照为 **355.21 → 472.41 QPS**，两版 Recall@10 都是 **92.78%**，有序结果相同。这份对照与正式新查询成绩单列。

正式配置则按本轮 90% 召回目标，在独立 400 条验证集上选择 L=**96／96／108**，验证门槛为 **91.5%**，给正式查询留余量；上一轮对应三个规模均为 L=128、验证门槛为 92%。因此，相比上一轮报告的全部提升还包含降低搜索工作量的收益和召回率取舍，不能全部归因于代码。微软仍为 L=128、重排 128，实际召回高于 zvec；这是双方超过 90% 门槛的比较，不是完全相同召回率或线程数的比较。

## 方法与适用范围

- 沿用同一组 Cohere 60／80／100 万样本，768 维、cosine、FP16、PQ64、top-10；索引未重建，文件大小与修改时间前后不变。20 万、40 万本轮未重测，历史结果见[上一轮报告](REPORT_UNSEEN_BASELINE_CN.md)。
- zvec 图出度配置为 32、构图候选数 100、各规模独立 PQ；微软图出度配置为 48、构图候选数 100、沿用共享 PQ64 码本。微软组是 **微软 Rust DiskANN 0.61.0 + 自定义 SQLite/PQ 适配层**，并非官方原生 SSD 存储引擎。
- 600 条预热、400 条验证、500 条全新查询及 3,000 条回归查询互不重叠；核对源行号、归一化 FP32 内容和 FP16 内容。新查询固定 seed=20261107，精确 GT 针对每个样本规模重新计算 FP64 cosine，并独立检查 3 条查询的标量点积和全排序。
- 候选试验只访问验证集。先做三轮交替验证和同 L 对照，再冻结源码、二进制、64 MiB 缓存、24 个工作线程及 L；先执行全新查询，后执行单遍回归，没有根据正式成绩再调整参数或替换慢分片。
- 每个实现、每个规模下，每条正式查询只执行一次。不同查询自然共享缓存，没有在每条查询之间清缓存；查询并发为 1。QPS 包含 FP16 转换、向量查询和 ID 提取，不含打开、输入校验、独立预热和离线 GT 比较。
- RSS 是 `/usr/bin/time -l` 的完整查询进程峰值，包含加载、预热、查询和退出，不含建库。zvec 保留不缓存文件读取；微软 SQLite 使用 256 KiB 堆页缓存、最多 16 MiB mmap，并可受益于系统文件缓存。没有清空系统文件缓存，RSS 也不覆盖进程外全部文件缓存。
- 正式测量期间没有构建或 GT 计算；其他桌面应用未隔离。500 条新查询每组只有一次计时，且仅涵盖本机和当前分布；本报告不承诺所有机器、数据、并发场景均领先。

## 使用与复现

本地源码默认仍为 4 个读取线程、32 MiB 缓存。本轮在打开集合前显式设置：

```bash
export ZVEC_DISKANN_READ_CACHE_MIB=64
export ZVEC_DISKANN_READ_WORKERS=24
```

搜索 L 按上述各规模配置。索引无需重建。**已发布 Rust／Node SDK 二进制尚未替换，使用这些改动需要链接本地重新编译的库。**

43 项 C++ 测试通过，包括宽线程池的逐请求执行、短批次错误回收、热点保留与过期淘汰、并发读、固定预算和跨页节点建库加载查询。本轮未获得 ThreadSanitizer 验证；此前该运行时在进入 main 前初始化失败。

共保存并核验 **{len(records)} 份有效进程记录**：{audit['pilot_records']} 次候选验证、35 次最终验证与对照、42 次正式测量。缩小搜索批次等未采用方案也保留。首次复制旧程序到新路径时进程在启动阶段被终止，随后使用原始可运行基线路径和重新编译的候选程序；该失败没有正式查询成绩。

[独立核验](results/unseen-optimization/audit.json)、[正式汇总](results/unseen-optimization/measure/summary.json)、[逐次记录](results/unseen-optimization/measure/runs.json)、[配置与摘要](results/unseen-optimization/measure/configuration.json)、[新查询来源与 GT 摘要](results/unseen-optimization/holdout-manifest.json)、[原始结果与 GT](results/unseen-optimization/raw-evidence.json.gz)、[源码补丁](results/unseen-optimization/measure/source.patch)、[C++ 测试](results/unseen-optimization/cpp-tests.json)。

复现脚本：[候选构建](build_unseen_variant.py)、[候选验证](run_unseen_pilot.py)、[新查询与 GT](prepare_unseen_holdout.py)、[冻结对比](run_unseen_comparison.py)、[报告核验与绘图](publish_unseen_optimization.py)。参数见 `--help`；本机完整实验目录为 `/tmp/zvec-unseen-opt-20261105`，需保留配置记录中的 Cohere 数据、索引、基线程序和本地动态库。
'''
    (HERE/'REPORT_UNSEEN_SPEED_CN.md').write_text(report)
    import matplotlib
    matplotlib.use('Agg')
    import matplotlib.pyplot as plt
    from matplotlib import font_manager
    font='/System/Library/Fonts/STHeiti Light.ttc';font_manager.fontManager.addfont(font)
    plt.rcParams.update({'font.family':font_manager.FontProperties(fname=font).get_name(),'font.size':11,'axes.spines.top':False,'axes.spines.right':False,'axes.unicode_minus':False,'svg.hashsalt':'unseen-optimization'})
    for phase,title,note in [('holdout','500 条全新查询：一次执行','每组 600 条独立预热 + 500 条全新查询；每个实现、每个规模各计时一次。'),('regression','3,000 条既有查询：单遍回归','6 个互斥分片各 500 条，每条只执行一次；QPS 按总计时合并，RSS 取进程峰值中位数。')]:
        fig,axes=plt.subplots(1,2,figsize=(13,6.4));series=[('microsoft','微软 Rust DiskANN + SQLite/PQ','#163B65','s','--'),('zvec','zvec DiskANN','#E87722','o','-')]
        for ax,metric,label,unit in [(axes[0],'qps','查询吞吐量 · 越高越好','QPS（次/秒）'),(axes[1],'rss_mib','查询进程峰值 RSS · 越低越好','MiB')]:
            for backend,name,color,marker,style in series:
                xs=[n/10000 for n in COUNTS];ys=[row(n,backend,phase)[metric] for n in COUNTS]
                ax.plot(xs,ys,color=color,marker=marker,linestyle=style,linewidth=2.3,markersize=6,label=name)
                for n,x,y in zip(COUNTS,xs,ys):
                    peer=row(n,'zvec' if backend=='microsoft' else 'microsoft',phase)[metric]
                    ax.annotate(f'{y:.2f}',(x,y),xytext=(0,11 if y>=peer else -21),textcoords='offset points',ha='center',color=color,fontsize=10)
            ax.set_xticks([60,80,100],['60 万','80 万','100 万']);ax.set_xlim(54,106)
            ax.set_ylim(0,max(row(n,b,phase)[metric] for n in COUNTS for b,_,_,_,_ in series)*1.23)
            ax.set(title=label,xlabel='Cohere 向量数量（768 维）',ylabel=unit);ax.grid(axis='y',alpha=.18)
        fig.suptitle(title+'\nFP16 · PQ64 · 查询并发 1 · 各组 Recall@10 ≥ 90%',fontsize=15,y=.98)
        fig.legend(*axes[0].get_legend_handles_labels(),loc='lower center',bbox_to_anchor=(.5,.10),ncol=2,frameon=False)
        fig.text(.5,.073,note,ha='center',fontsize=10)
        fig.text(.5,.039,'zvec：64 MiB 缓存 / 24 个 I/O 线程；微软为 SQLite 同步读取，图参数及实际召回率不同。',ha='center',fontsize=10)
        fig.text(.5,.007,f"本地 zvec {config['source_commit'][:8]} · Apple M3 Pro / 36 GiB / macOS · 2026-11-05",ha='center',fontsize=9,color='#555555')
        fig.subplots_adjust(left=.075,right=.985,top=.80,bottom=.27,wspace=.25)
        fig.savefig(dest/f'qps-memory-{phase}.png',dpi=200);fig.savefig(dest/f'qps-memory-{phase}.svg',metadata={'Date':None});plt.close(fig)
        svg=dest/f'qps-memory-{phase}.svg';svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines())+'\n')
    print(json.dumps(audit,ensure_ascii=False,indent=2))

if __name__=='__main__':main()
