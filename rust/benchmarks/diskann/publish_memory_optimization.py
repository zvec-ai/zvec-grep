"""Independently audit one-pass memory comparison and publish its report."""
import argparse,gzip,hashlib,json,re,shutil,statistics,subprocess
from pathlib import Path
import numpy as np

HERE=Path(__file__).resolve().parent
REPO=HERE.parents[2]
COUNTS=[600000,800000,1000000]
PILOTS=['baseline-budgets','compact-budgets','admission-budgets','wider-budgets','tuning']
def sha(p):
    with Path(p).open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def save(p,x):p.write_text(json.dumps(x,ensure_ascii=False,indent=2)+'\n')
def near(a,b):assert abs(a-b)<1e-8*max(1,abs(a),abs(b)),(a,b)

def main():
    p=argparse.ArgumentParser();p.add_argument('--base',type=Path,required=True);a=p.parse_args();base=a.base
    dest=HERE/'results/memory-optimization';dest.mkdir(exist_ok=True)
    config=json.loads((base/'measure/configuration.json').read_text());source=Path(config['source'])
    assert config['source_commit']=='f8a36342340b837366264a48bb0e3983412558f9'
    assert config['cache_mib']==8 and config['read_workers']==32
    assert sha(HERE/'run_memory_comparison.py')==config['runner_sha256']
    assert sha(HERE/'harness/src/bin/memory-bench.rs')==config['harness_sha256']
    for name,digest in config['source_sha256'].items():
        assert sha(source/name)==digest
        assert hashlib.sha256(subprocess.check_output(['git','show',f"{config['source_commit']}:{name}"],cwd=source)).hexdigest()==digest
    for name,digest in config['microsoft_source_sha256'].items():assert sha(REPO/name)==digest
    for group in ['binaries','libraries']:
        for item in config[group].values():assert sha(item['path'])==item['sha256']
    data=Path(config['data']);holdout=Path('/tmp/zvec-unseen-opt-20261105/holdout')
    assert sha(data/'manifest.json')==config['data_manifest_sha256']
    assert sha(holdout/'manifest.json')==config['holdout_manifest_sha256']
    dm=json.loads((data/'manifest.json').read_text());hm=json.loads((holdout/'manifest.json').read_text())
    for directory,meta in [(data,dm),(holdout,hm)]:
        for name,info in meta['files'].items():assert sha(directory/name)==info['sha256']
    assert sha('/tmp/zvec-cohere-million/data/base.f32')==dm['base_sha256']==hm['base_sha256']
    ids_seen=set();fp32=set();fp16=set();fingerprints={}
    sets=[(name,data/f'{name}.f32',ids) for name,ids in dm['query_source_row_ids'].items()]
    sets.append(('test-6',holdout/'queries.f32',hm['query_source_row_ids']))
    for name,path,ids in sets:
        assert len(ids)==len(set(ids)) and not ids_seen.intersection(ids);ids_seen.update(ids)
        x=np.fromfile(path,dtype='<f4').reshape(-1,768);assert len(x)==len(ids);fingerprints[name]=[]
        for row in x:
            k=hashlib.sha256(row.tobytes()).hexdigest();h=hashlib.sha256(row.astype('<f2').tobytes()).hexdigest()
            assert k not in fp32 and h not in fp16;fp32.add(k);fp16.add(h);fingerprints[name].append(k)
    assert len(fp32)==len(fp16)==len(ids_seen)==4500
    raw_evidence=[];all_rows={};truth={}
    stages=PILOTS+['invalid-loader-validation','validation','controls','measure']
    for stage in stages:
        folder=base/stage;rows=json.loads((folder/'runs.json').read_text());all_rows[stage]=rows
        for r in rows:
            path=folder/r['raw_file'];raw=[json.loads(line) for line in path.read_text().splitlines()]
            actual=next(x for x in raw if x['stage']=='queried');ids=next(x['ids'] for x in raw if x['stage']=='results')
            n=r['count'];formal=stage=='measure';q=500 if formal else 400
            gtfile=(holdout/f'truth-{n}.json' if r['repeat']==6 else data/f"test-{r['repeat']}-truth-{n}.json") if formal else data/f'validation-truth-{n}.json'
            gt=json.loads(gtfile.read_text());truth[str(gtfile)]=gt
            assert len(ids)==len(gt)==actual['queries']==q and all(len(x)==len(set(x))==10 for x in ids)
            assert all(re.fullmatch(r'v\d{8}',x) and int(x[1:])<n for group in ids for x in group)
            assert actual['test_passes']==1 and actual['test_warmup_passes']==0 and actual['warmup_queries']==600
            recall=sum(len(set(x)&set(y)) for x,y in zip(ids,gt))/(q*10)
            near(r.get('recall_at_10',r.get('recall')),recall);near(r['batch_s'],actual['batch_s']);near(r['qps'],q/actual['batch_s'])
            key='results_sha256' if 'results_sha256' in r else 'ids_sha256'
            assert hashlib.sha256(json.dumps(ids,separators=(',',':')).encode()).hexdigest()==r[key]
            stderr=path.with_suffix('.stderr').read_text();near(r['rss_mib'],int(re.search(r'(\d+)\s+maximum resident set size',stderr).group(1))/2**20)
            assert r['rss_mib']>=actual['rss_peak_after_test_mib']
            times=sorted(actual['latencies_ms']);assert len(times)==q and all(t>0 for t in times)
            near(actual['p50_ms'],times[q//2]);near(actual['p95_ms'],times[q*95//100]);assert sum(times)/1000<=actual['batch_s']*1.001
            verified=stage in ['validation','controls','measure']
            if verified:
                lib=Path(r['library'] if stage=='controls' else config['libraries'][r['backend']]['path'])
                loaded=next(x['path'] for x in raw if x['stage']=='library')
                assert Path(loaded).resolve()==lib.resolve() and r['loaded_library']==loaded
                if stage=='controls':assert sha(lib)==r['library_sha256']
            if formal:
                assert recall>=.90 and r['finished']>config['start']
                assert r['effort']==(128 if r['backend']=='microsoft' else config['selection']['settings'][str(n)]['effort'])
                assert r['workers']==(1 if r['backend']=='microsoft' else 32)
            raw_evidence.append(dict(stage=stage,path=r['raw_file'],valid_for_final_comparison=verified,jsonl=path.read_text(),stderr=stderr))
        target=dest/stage;target.mkdir(exist_ok=True)
        for f in folder.glob('*.json'):shutil.copy2(f,target/f.name)
        if (folder/'source.patch').exists():shutil.copy2(folder/'source.patch',target/'source.patch')
        if stage in ['validation','measure']:
            assert json.loads((folder/'status.json').read_text())['phase']=='complete'
            sc=json.loads((folder/'configuration.json').read_text())
            assert sc['source_sha256']==config['source_sha256'] and sc['selection']==config['selection'] and sc['libraries']==config['libraries']
            for f,s in json.loads((folder/'index-inventory.json').read_text()).items():assert [Path(f).stat().st_size,Path(f).stat().st_mtime_ns]==s
            for summary in json.loads((folder/'summary.json').read_text()):
                group=[r for r in rows if r['backend']==summary['backend'] and r['count']==summary['count']]
                assert len(group)==summary['processes'] and sum(r['queries'] for r in group)==summary['queries']
                near(summary['qps'],sum(r['queries'] for r in group)/sum(r['batch_s'] for r in group));near(summary['rss_mib'],statistics.median(r['rss_mib'] for r in group))
                near(summary['recall_at_10'],sum(r['recall_hits'] for r in group)/sum(r['queries']*10 for r in group))
    validation=all_rows['validation'];formal=all_rows['measure'];controls=all_rows['controls']
    assert len(validation)==27 and len(formal)==42 and len(controls)==8
    for n in COUNTS:
        for backend in ['baseline','zvec','microsoft']:
            group=[r for r in validation if r['backend']==backend and r['count']==n]
            assert len({r['results_sha256'] for r in group})==1
            if backend=='zvec':assert min(r['recall_at_10'] for r in group)>=.915
        for backend in ['zvec','microsoft']:
            assert sorted(r['repeat'] for r in formal if r['backend']==backend and r['count']==n)==list(range(7))
    for effort in [108,90]:assert len({r['results_sha256'] for r in controls if r['effort']==effort})==1
    summaries=json.loads((base/'measure/summary.json').read_text());vs=json.loads((base/'validation/summary.json').read_text())
    def row(n,b):return next(r for r in summaries if r['count']==n and r['backend']==b)
    def val(n,b):return next(r for r in vs if r['count']==n and r['backend']==b)
    comparisons=[]
    for n in COUNTS:
        z=row(n,'zvec');m=row(n,'microsoft');ratio=z['qps']/m['qps'];assert ratio>1
        pairs=[r['qps']/next(x['qps'] for x in formal if x['backend']=='microsoft' and x['count']==n and x['repeat']==r['repeat']) for r in formal if r['backend']=='zvec' and r['count']==n]
        comparisons.append(dict(count=n,qps_ratio=ratio,rss_ratio=z['rss_mib']/m['rss_mib'],paired_wins=sum(x>1 for x in pairs),pairs=7,minimum_pair_ratio=min(pairs)))
    for name in ['cpp-tests.json','loader-isolation.json','selection-draft.json','rust-build.log','test-build.log']:
        shutil.copy2(base/name,dest/(name.replace('.log','.txt')))
    for name in ['build.json','build.txt']:shutil.copy2(base/'final'/name,dest/name)
    for name in ['clippy.log','clippy-reviewed.log']:shutil.copy2(base/name,dest/name.replace('.log','.txt'))
    assert 'Finished' in (base/'clippy-reviewed.log').read_text() and 'error:' not in (base/'clippy-reviewed.log').read_text()
    tests=json.loads((dest/'cpp-tests.json').read_text());assert tests['total_passed']==46 and tests['source_commit']==config['source_commit']
    assert (dest/'measure/source.patch').read_bytes()==subprocess.check_output(['git','diff','047ebaf4',config['source_commit']],cwd=source)
    for name,folder in [('original-data-manifest.json',data),('previous-holdout-manifest.json',holdout)]:shutil.copy2(folder/'manifest.json',dest/name)
    with (dest/'raw-evidence.json.gz').open('wb') as f:
        with gzip.GzipFile(fileobj=f,mode='wb',filename='',mtime=0) as g:g.write(json.dumps(dict(records=raw_evidence,truth=truth,query_fingerprints=fingerprints),separators=(',',':')).encode())
    audit=dict(passed=True,verified_processes=77,formal_processes=42,exploratory_processes=sum(len(all_rows[x]) for x in PILOTS),excluded_loader_processes=len(all_rows['invalid-loader-validation']),
               disjoint_fp32_fp16_source_ids=True,each_test_query_once_per_backend_scale=True,formal_queries=3500,previously_used_queries=True,
               actual_loaded_library_checked=True,minimum_formal_recall=min(r['recall_at_10'] for r in formal),comparisons=comparisons,raw_sha256=sha(dest/'raw-evidence.json.gz'))
    save(dest/'audit.json',audit)
    save(dest/'excluded-records.json',dict(stage='invalid-loader-validation',reason='The candidate executable initially hard-linked the baseline inode. Alternating launches produced inconsistent recall and RSS, consistent with native loader path reuse. No actual loaded-library guard was present; all 27 validation processes excluded. Replaced with separate executables, absolute dylib load commands, ad-hoc signatures and dladdr checks before any query. No formal queries executed in the excluded stage.'))
    table='\n'.join(f"| {n//10000} 万 | {row(n,'zvec')['qps']:.2f} | {row(n,'microsoft')['qps']:.2f} | {(row(n,'zvec')['qps']/row(n,'microsoft')['qps']-1)*100:.2f}% | {row(n,'zvec')['rss_mib']:.2f} / {row(n,'microsoft')['rss_mib']:.2f} | {row(n,'zvec')['recall_at_10']:.2%} / {row(n,'microsoft')['recall_at_10']:.2%} |" for n in COUNTS)
    savings='\n'.join(f"| {n//10000} 万 | {val(n,'baseline')['rss_mib']:.2f} | {val(n,'zvec')['rss_mib']:.2f} | {val(n,'baseline')['rss_mib']-val(n,'zvec')['rss_mib']:.2f} | {(1-val(n,'zvec')['rss_mib']/val(n,'baseline')['rss_mib'])*100:.2f}% | {val(n,'baseline')['qps']:.2f} → {val(n,'zvec')['qps']:.2f} |" for n in COUNTS)
    ranges='\n'.join(f"| {n//10000} 万 | {row(n,'zvec')['qps_min']:.2f}–{row(n,'zvec')['qps_max']:.2f} | {row(n,'microsoft')['qps_min']:.2f}–{row(n,'microsoft')['qps_max']:.2f} | {next(x['paired_wins'] for x in comparisons if x['count']==n)} / 7 |" for n in COUNTS)
    ctl=lambda variant,cache:next(r for r in controls if r['variant']==variant and r['cache_mib']==cache)
    old=ctl('baseline',8);compact=ctl('compact-keys',8);admission=ctl('admission',8)
    gains=[(r['qps_ratio']-1)*100 for r in comparisons];z=row(1000000,'zvec');m=row(1000000,'microsoft')
    report=f'''# Cohere：降低 zvec DiskANN RSS 后的单遍查询对比

测试日期：2026-11-05。Apple M3 Pro、36 GiB、macOS；本地 zvec 源码 `{config['source_commit'][:8]}`。

**三个规模均保持 QPS 领先、召回率超过 90%，同时显著降低 zvec 的 RSS。** 同轮独立验证对照中，相比上一版 64 MiB 缓存配置，RSS 从约 177／199／221 MiB 降至 110／128／146 MiB。正式 3,500 条单遍回归查询中，QPS 比微软适配层高 **{min(gains):.2f}%–{max(gains):.2f}%**。百万组 RSS 为 **{z['rss_mib']:.2f} 对 {m['rss_mib']:.2f} MiB**，仍多 {z['rss_mib']-m['rss_mib']:.2f} MiB，尚未达到相同内存占用。

## 正式结果

沿用相同 Cohere 60／80／100 万向量索引、768 维、cosine、FP16、PQ64、top-10。600 条预热、400 条验证与 3,500 条测试查询互不重叠。测试查询在此前轮次使用过，本轮只用于冻结后的回归，不称为全新盲测。七个互斥分片各 500 条，每个实现、每个规模下每条测试查询仅执行一次；每个进程先执行独立预热集一次。

QPS = 3,500 / 七个计时批次的总秒数；RSS 为七个查询进程峰值的中位数，包含加载、预热、查询和退出，不含建库。

| 向量数 | zvec DiskANN QPS | 微软 QPS | QPS 提升 | RSS：zvec / 微软（MiB） | Recall@10：zvec / 微软 |
|---|---:|---:|---:|---:|---:|
{table}

![QPS 与查询进程峰值 RSS](results/memory-optimization/qps-memory.png)

| 向量数 | zvec 分片 QPS 范围 | 微软分片 QPS 范围 | zvec 配对领先分片数 |
|---|---:|---:|---:|
{ranges}

全部 42 个正式进程最低 Recall@10 为 **{audit['minimum_formal_recall']:.2%}**。所有分片均保留；汇总领先不代表每条请求都更快。不同分片使用不同查询，其范围不能当作重复测量置信区间。

## 内存减少在哪里

以下为同轮 400 条独立验证查询、各三轮新进程的对照。RSS 取峰值中位数，QPS 按总计时合并；旧版是源码 `047ebaf4`、64 MiB 缓存、24 个 I/O 线程，新版为 8 MiB、32 个线程。搜索批次策略和 L 也有调整，因此这是完整配置对照。

| 向量数 | 旧版 RSS（MiB） | 本轮 RSS（MiB） | 减少（MiB） | 降幅 | QPS：旧版 → 本轮 |
|---|---:|---:|---:|---:|---:|
{savings}

1. **节点缓存从 64 MiB 缩至 8 MiB。** 每组用固定长度的偏移历史识别重复访问；组满后，大部分首次访问不占缓存，仅周期性放行以淘汰旧热点。历史元数据计入原预算。缓存保存的节点字节不变，不缓存查询答案。
2. **无损压缩规则 key 与 ID 映射，去掉整段临时拷贝。** 以 4 KiB 缓冲逐项校验；连续 key 只保存起始值，恒等映射无需数组。不符合条件时直接读取原数组作为最终缓冲，不改磁盘格式。百万条原数组本体约 11.44 MiB，另外省去加载临时块及其分配器驻留开销。隔离验证在相同 8 MiB 缓存、L=108、24 线程下，RSS 为 **{old['rss_mib']:.2f} → {compact['rss_mib']:.2f} MiB**，有序结果完全一致；该差额包含进程峰值与分配器行为，不能全部解释为仍存活对象的大小。
3. **用读取并行度补偿小缓存的未命中。** 32 个固定 I/O 线程；仅 macOS 节点读取且线程数至少 24 时，将批次宽度从 `min(max(8,L/5),32)` 调整为 `min(max(8,L/3),32)`，仍受扇区缓冲容量限制。较宽批次改变图探索顺序，因此重新在验证集选择 L=84／84／90，验证召回分别为 92.225%／92.00%／91.90%。默认窄线程池及其他存储路径保留原策略。

同 L=108、24 线程的独立读取诊断中，仅加入准入策略，8 MiB 缓存的 QPS 为 **{compact['qps']:.2f} → {admission['qps']:.2f}**，实际 pread 从 **{compact['read_profile']['pread_syscalls']:,} → {admission['read_profile']['pread_syscalls']:,}**，有序结果一致。此为单次验证诊断，含 600 条预热和 400 条验证，不能替代正式成绩或视为独立重复实验。

## 仍然存在的内存差距与取舍

双方百万条 PQ64 编码本体仍各约 **61.04 MiB**，本轮没有降低 PQ 精度、裁剪编码或把它移出常驻内存。zvec 还承担完整 Collection、主键正排、其他元数据、读取线程和缓冲的成本；微软组是 **Microsoft Rust DiskANN 0.61.0 + 自定义 SQLite/PQ 适配层**，并非官方原生 SSD 引擎。剩余差额未逐项完整归因，不能都算作节点缓存。

8 MiB 缓存是速度和内存的取舍。百万独立验证诊断中，关闭缓存为 {ctl('final',0)['rss_mib']:.2f} MiB、{ctl('final',0)['qps']:.2f} QPS；4 MiB 为 {ctl('final',4)['rss_mib']:.2f} MiB、{ctl('final',4)['qps']:.2f} QPS；8 MiB 为 {ctl('final',8)['rss_mib']:.2f} MiB、{ctl('final',8)['qps']:.2f} QPS。保留 8 MiB 是为 QPS 领先留出余量，没有用正式测试结果选择预算。

zvec 保留 `F_NOCACHE`、关闭预读；微软仍为 SQLite 256 KiB 堆页缓存、最多 16 MiB mmap，可使用系统文件缓存。RSS 不包含进程外全部文件缓存，因此本报告证明的是进程 RSS 的下降，未测量对其他应用的全部影响。节点缓存预算有上限，整个进程 RSS 仍随 PQ 和数据规模增长。

## 验证边界与复现

- 同一采样基库、精确 GT 与图索引不变，核对数据摘要、源查询行号、FP32 和 FP16 内容互斥，索引文件大小及修改时间前后不变。查询并发为 1。微软 L=128、重排 128；双方实际召回、图出度和码本不同，属于均超过 90% 门槛的比较。
- 候选和预算试验只使用验证集；先完成三轮交替对照，再冻结源码、库、二进制与参数，之后执行七个正式分片。正式测试没有重新调参、重复执行或挑选慢分片。系统文件缓存没有清空，其他桌面应用未隔离；20／40 万本轮未测。
- 初次交替验证曾硬链接基线可执行文件，观察到召回和 RSS 不一致，疑似加载路径复用。该批 **27 个验证进程全部排除**并保留原记录。正式测试前改为独立可执行文件、绝对动态库依赖和正常代码签名；每个进程在任何查询前用 `dladdr` 核对实际加载路径，父进程核对库摘要。修复后同一验证配置的有序结果在三轮间完全一致。
- **46 项 C++ 测试通过**，覆盖跨块连续 key、非连续及乱序 key、uint64 边界、clone 后释放原对象、截断段拒绝、小缓存准入、热点过期淘汰、并发访问、读取错误回收及跨页建库查询。没有新增 ThreadSanitizer 通过的声明。
- Rust 测量程序构建和 Clippy 检查通过；沿用原测量程序的数组分块及枚举表示，以命令行豁免 `chunks_exact_to_as_chunks`、`large_enum_variant` 两项风格提示，其他警告按 `-D warnings` 检查，避免正式测量后更改已冻结的程序源码。
- 保存 **77 份加载路径已验证的有效进程记录**（27 验证、8 归因对照、42 正式），另保留 54 份探索记录及 27 份排除记录。探索阶段未逐进程验证加载路径，主要结论只依据修复后的记录。

本轮在打开集合前设置：

```bash
export ZVEC_DISKANN_READ_CACHE_MIB=8
export ZVEC_DISKANN_READ_WORKERS=32
```

源码默认仍为 32 MiB 缓存、4 个读取线程，需显式采用本轮配置；多个图文件各自计预算。已发布 Rust／Node SDK 尚未包含这些源码修改，需要链接本地重新编译的库。

[独立核验](results/memory-optimization/audit.json)、[正式汇总](results/memory-optimization/measure/summary.json)、[原始结果与 GT](results/memory-optimization/raw-evidence.json.gz)、[冻结配置](results/memory-optimization/measure/configuration.json)、[源码补丁](results/memory-optimization/measure/source.patch)、[归因对照](results/memory-optimization/controls/runs.json)、[排除说明](results/memory-optimization/excluded-records.json)、[C++ 测试](results/memory-optimization/cpp-tests.json)。

复现脚本：[构建](build_memory_variant.py)、[独立链接与加载校验](prepare_memory_binaries.py)、[验证预算](run_memory_pilot.py)、[归因对照](run_memory_controls.py)、[冻结对比](run_memory_comparison.py)、[核验与绘图](publish_memory_optimization.py)。入口程序为 [memory-bench.rs](harness/src/bin/memory-bench.rs)，绝对链接和签名信息见[加载隔离记录](results/memory-optimization/loader-isolation.json)；完整本机目录 `/tmp/zvec-memory-opt-20261105`。此前 64 MiB 配置的报告见[上一轮](REPORT_UNSEEN_SPEED_CN.md)。
'''
    (HERE/'REPORT_MEMORY_BASELINE_CN.md').write_text(report)
    import matplotlib
    matplotlib.use('Agg')
    import matplotlib.pyplot as plt
    from matplotlib import font_manager
    font_manager.fontManager.addfont('/System/Library/Fonts/STHeiti Light.ttc')
    plt.rcParams.update({'font.family':font_manager.FontProperties(fname='/System/Library/Fonts/STHeiti Light.ttc').get_name(),'font.size':12,'axes.unicode_minus':False,'svg.hashsalt':'memory-optimization'})
    fig,axes=plt.subplots(1,2,figsize=(13,6.4));colors={'microsoft':'#163B65','zvec':'#E87722'}
    for ax,key,title,unit in [(axes[0],'qps','查询吞吐量 · 越高越好','QPS（次/秒）'),(axes[1],'rss_mib','查询进程峰值 RSS · 越低越好','MiB')]:
        for backend,label,marker,style in [('microsoft','微软 Rust DiskANN + SQLite/PQ','s','--'),('zvec','zvec DiskANN','o','-')]:
            values=[row(n,backend)[key] for n in COUNTS]
            ax.plot([60,80,100],values,label=label,color=colors[backend],marker=marker,linestyle=style,lw=2.2)
            for n,v in zip(COUNTS,values):ax.annotate(f'{v:.2f}',(n/10000,v),xytext=(0,11 if backend=='zvec' else -19),textcoords='offset points',ha='center',color=colors[backend],fontsize=10)
        ax.set_title(title);ax.set_ylabel(unit);ax.set_xlabel('Cohere 向量数量（768 维）');ax.set_xticks([60,80,100],['60 万','80 万','100 万'])
        ax.set_xlim(54,106);ax.set_ylim(0,max(row(n,b)[key] for n in COUNTS for b in colors)*1.25);ax.grid(axis='y',alpha=.2);ax.spines[['top','right']].set_visible(False)
    fig.suptitle('3,500 条互斥测试查询 · 单遍回归\nFP16 · PQ64 · 查询并发 1 · 各组 Recall@10 ≥ 90%',fontsize=16)
    handles,labels=axes[0].get_legend_handles_labels();fig.legend(handles,labels,loc='lower center',bbox_to_anchor=(.5,.105),ncol=2,frameon=False)
    fig.text(.5,.082,'每进程 600 条独立预热 + 500 条测试；7 个分片，每条测试查询只执行一次。',ha='center',fontsize=10)
    fig.text(.5,.047,'zvec：8 MiB 节点缓存 / 32 个 I/O 线程；双方实际召回及图参数不同。',ha='center',fontsize=10)
    fig.text(.5,.015,'本地 zvec f8a36342 · Apple M3 Pro / 36 GiB / macOS · 2026-11-05',ha='center',fontsize=9,color='#555555')
    fig.subplots_adjust(left=.08,right=.985,top=.80,bottom=.28,wspace=.25)
    fig.savefig(dest/'qps-memory.png',dpi=200);fig.savefig(dest/'qps-memory.svg',metadata={'Date':None});plt.close(fig)
    svg=dest/'qps-memory.svg';svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines())+'\n')
    print(json.dumps(audit,indent=2))

if __name__=='__main__':main()
