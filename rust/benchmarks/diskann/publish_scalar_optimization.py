"""Independently audit one-pass bounded scalar-cache comparison and publish its report."""
import argparse,gzip,hashlib,json,re,shutil,statistics,subprocess
from pathlib import Path
import numpy as np

HERE=Path(__file__).resolve().parent
REPO=HERE.parents[2]
COUNTS=[600000,800000,1000000]
PILOTS=['pilot-v1','pilot-v3']
def sha(p):
    with Path(p).open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def save(p,x):p.write_text(json.dumps(x,ensure_ascii=False,indent=2)+'\n')
def near(a,b):assert abs(a-b)<1e-8*max(1,abs(a),abs(b)),(a,b)

def main():
    p=argparse.ArgumentParser();p.add_argument('--base',type=Path,required=True);a=p.parse_args();base=a.base
    dest=HERE/'results/scalar-cache';dest.mkdir(exist_ok=True)
    config=json.loads((base/'measure/configuration.json').read_text());source=Path(config['source'])
    assert config['source_commit'].startswith('6473f807')
    assert config['cache_mib']==8 and config['read_workers']==32 and config['scalar_cache_mib']==1
    assert sha(HERE/'run_scalar_comparison.py')==config['runner_sha256']
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
    stages=PILOTS+['validation','measure']
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
            verified=True
            if verified:
                lib=Path(json.loads((folder/'configuration.json').read_text())['library'] if stage in PILOTS else config['libraries'][r['backend']]['path'])
                loaded=next(x['path'] for x in raw if x['stage']=='library')
                assert Path(loaded).resolve()==lib.resolve() and r['loaded_library']==loaded
                if stage in PILOTS:assert sha(lib)==json.loads((folder/'configuration.json').read_text())['library_sha256']
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
    validation=all_rows['validation'];formal=all_rows['measure']
    assert len(validation)==27 and len(formal)==42
    previous=json.loads((HERE/'results/memory-optimization/measure/runs.json').read_text())
    for n in COUNTS:
        for backend in ['baseline','zvec','microsoft']:
            group=[r for r in validation if r['backend']==backend and r['count']==n]
            assert len({r['results_sha256'] for r in group})==1
            assert min(r['recall_at_10'] for r in group)>=.90
        assert len({r['results_sha256'] for r in validation if r['backend']!='microsoft' and r['count']==n})==1
        for backend in ['zvec','microsoft']:
            group=[r for r in formal if r['backend']==backend and r['count']==n]
            assert sorted(r['repeat'] for r in group)==list(range(7))
            for r in group:
                old=next(x for x in previous if x['count']==n and x['backend']==backend and x['repeat']==r['repeat'])
                assert r['results_sha256']==old['results_sha256'], 'ordered results changed'
    summaries=json.loads((base/'measure/summary.json').read_text());vs=json.loads((base/'validation/summary.json').read_text())
    def row(n,b):return next(r for r in summaries if r['count']==n and r['backend']==b)
    def val(n,b):return next(r for r in vs if r['count']==n and r['backend']==b)
    comparisons=[]
    for n in COUNTS:
        z=row(n,'zvec');m=row(n,'microsoft');ratio=z['qps']/m['qps']
        pairs=[r['qps']/next(x['qps'] for x in formal if x['backend']=='microsoft' and x['count']==n and x['repeat']==r['repeat']) for r in formal if r['backend']=='zvec' and r['count']==n]
        comparisons.append(dict(count=n,qps_ratio=ratio,rss_ratio=z['rss_mib']/m['rss_mib'],paired_wins=sum(x>1 for x in pairs),pairs=7,minimum_pair_ratio=min(pairs),rss_saved_vs_baseline_mib=val(n,'baseline')['rss_mib']-val(n,'zvec')['rss_mib']))
    for name in ['cpp-tests.json','selection-draft.json']:shutil.copy2(base/name,dest/name)
    for name in ['build.json','build.log']:shutil.copy2(base/'final'/name,dest/name.replace('.log','.txt'))
    tests=json.loads((dest/'cpp-tests.json').read_text())
    assert all(r['exit_code']==0 for r in tests.values())
    assert sum(r['passed'] for r in tests.values())==136
    assert sum(r.get('skipped',0) for r in tests.values())==1
    assert (dest/'measure/source.patch').read_bytes()==subprocess.check_output(['git','diff','f8a36342',config['source_commit']],cwd=source)
    for name,folder in [('original-data-manifest.json',data),('previous-holdout-manifest.json',holdout)]:shutil.copy2(folder/'manifest.json',dest/name)
    with (dest/'raw-evidence.json.gz').open('wb') as f:
        with gzip.GzipFile(fileobj=f,mode='wb',filename='',mtime=0) as g:g.write(json.dumps(dict(records=raw_evidence,truth=truth,query_fingerprints=fingerprints),separators=(',',':')).encode())
    # An initial process failed before main/library verification. Preserve it
    # separately; no query or performance row came from that launch.
    failure=base/'buffered-v1/pilot-c0.jsonl'
    assert not failure.read_text()
    save(dest/'startup-failure.json',dict(excluded=True,query_results=0,reason='Initial prototype process terminated before library verification; no benchmark output.',stderr=failure.with_suffix('.stderr').read_text()))
    audit=dict(passed=True,verified_processes=len(raw_evidence),formal_processes=42,validation_processes=27,pilot_processes=sum(len(all_rows[x]) for x in PILOTS),
               disjoint_fp32_fp16_source_ids=True,each_test_query_once_per_backend_scale=True,formal_queries=3500,previously_used_queries=True,
               actual_loaded_library_checked=True,ordered_ids_match_previous_baseline_all_formal_queries=True,
               minimum_formal_recall=min(r['recall_at_10'] for r in formal),comparisons=comparisons,
               cpp_tests_passed=136,cpp_tests_skipped=1,raw_sha256=sha(dest/'raw-evidence.json.gz'))
    save(dest/'audit.json',audit)
    table='\n'.join(f"| {n//10000} 万 | {row(n,'zvec')['qps']:.2f} | {row(n,'microsoft')['qps']:.2f} | {(row(n,'zvec')['qps']/row(n,'microsoft')['qps']-1)*100:.2f}% | {row(n,'zvec')['rss_mib']:.2f} / {row(n,'microsoft')['rss_mib']:.2f} | {row(n,'zvec')['recall_at_10']:.2%} / {row(n,'microsoft')['recall_at_10']:.2%} |" for n in COUNTS)
    savings='\n'.join(f"| {n//10000} 万 | {val(n,'baseline')['rss_mib']:.2f} | {val(n,'zvec')['rss_mib']:.2f} | {val(n,'baseline')['rss_mib']-val(n,'zvec')['rss_mib']:.2f} | {(1-val(n,'zvec')['rss_mib']/val(n,'baseline')['rss_mib'])*100:.2f}% | {val(n,'baseline')['qps']:.2f} → {val(n,'zvec')['qps']:.2f} |" for n in COUNTS)
    ranges='\n'.join(f"| {n//10000} 万 | {row(n,'zvec')['qps_min']:.2f}–{row(n,'zvec')['qps_max']:.2f} | {row(n,'microsoft')['qps_min']:.2f}–{row(n,'microsoft')['qps_max']:.2f} | {next(x['paired_wins'] for x in comparisons if x['count']==n)} / 7 |" for n in COUNTS)
    gains=[(r['qps_ratio']-1)*100 for r in comparisons];z=row(1000000,'zvec');m=row(1000000,'microsoft')
    assert min(gains)>0, 'Retain and report failure; do not publish a success claim'
    report=f'''# Cohere：zvec DiskANN 有界标量缓存测试

测试日期：2026-11-05。Apple M3 Pro、36 GiB、macOS；本地 zvec 源码 `{config['source_commit'][:8]}`。

**在 60／80／100 万向量下，减少 RSS 后仍保持 QPS 领先微软适配层、Recall@10 超过 90%。** 正式单遍回归中 QPS 领先 **{min(gains):.2f}%–{max(gains):.2f}%**。百万组 RSS 为 **{z['rss_mib']:.2f} 对 {m['rss_mib']:.2f} MiB**，差额为 {z['rss_mib']-m['rss_mib']:.2f} MiB，仍未达到相同内存占用。

## 正式结果

沿用同一 Cohere 采样基库、索引和精确 GT：768 维、cosine、FP16、PQ64、top-10。600 条预热、400 条验证与 3,500 条测试查询互不重叠，核对了源行号、FP32 和 FP16 内容。测试查询在之前轮次使用过，本轮是冻结参数后的回归，不称为全新盲测。七个互斥分片各 500 条，每个实现、每个规模下，每条测试查询只执行一次；每个新进程先运行独立预热集一次。

QPS = 3,500 / 七个测试批次总秒数；RSS 是七个查询进程完整生命周期峰值的中位数，包含加载、预热、查询和退出，不含建库。查询并发为 1。

| 向量数 | zvec DiskANN QPS | 微软 QPS | QPS 提升 | RSS：zvec / 微软（MiB） | Recall@10：zvec / 微软 |
|---|---:|---:|---:|---:|---:|
{table}

![QPS 与查询进程峰值 RSS](results/scalar-cache/qps-memory.png)

| 向量数 | zvec 分片 QPS 范围 | 微软分片 QPS 范围 | zvec 配对领先分片数 |
|---|---:|---:|---:|
{ranges}

全部 42 个正式进程的最低 Recall@10 为 **{audit['minimum_formal_recall']:.2%}**。所有分片都保留，汇总领先不代表每条请求都更快；分片范围包含查询差异，不能当作重复测量置信区间。双方实际召回率不同，这是均超过 90% 门槛的比较。

## 相比上轮减少了多少内存

以下是同轮 400 条独立验证查询、各三次新进程的交替对照。基线为上轮 `f8a36342`，原有 8 MiB 节点缓存、32 个 I/O 线程及 L=84／84／90 全部保持不变。本轮只额外启用 1 MiB 标量缓存；QPS 按总计时合并，RSS 取峰值中位数。

| 向量数 | 基线 RSS（MiB） | 本轮 RSS（MiB） | 减少（MiB） | 降幅 | QPS：基线 → 本轮 |
|---|---:|---:|---:|---:|---:|
{savings}

这是一项用少量额外读取换取较低 RSS 的优化；与自身 mmap 基线相比，验证 QPS 有所下降，但仍超过同轮微软方案。验证集有序结果与基线完全一致；正式测试的全部有序 top-10 结果也与上一轮相同配置一致，未靠降低召回率换取内存收益。

## 修改了什么

1. **标量文件不再长期保持整文件映射。** 打开时临时读取 Arrow 布局，保留行号和文件偏移，复制同一文件描述符后释放映射。已有索引无需重建。
2. **主键与全局 ID 按 4 KiB 页读取，LRU 缓存为 1 MiB。** 当 IPC 文件仅包含未压缩的 uint64/string 列时，只读取所需的有效位、字符串偏移及内容；页面与每项记账开销计入缓存预算。无需为了取一个主键读入整个约 80 KiB 批次。
3. **保留通用读取和对象生命周期。** 其他类型或压缩缓冲区使用有界批次缓存，超大批次绕过缓存；扫描不填充点查缓存，结果复制到自己的缓冲区。并发读取和淘汰受锁保护，文件路径替换不会切换已经打开的底层文件。

图搜索、FP16、PQ64、码本、重排参数及节点缓存均未改变。双方百万条 PQ64 编码本体仍各约 61.04 MiB。zvec 还保留 Collection、doc ID 数组、元数据和读取线程等成本，本轮没有逐项分解剩余 RSS 差额。

## 使用配置和边界

在打开只读集合前设置，并链接本地编译的 zvec 库：

```bash
export ZVEC_DISKANN_READ_CACHE_MIB=8
export ZVEC_DISKANN_READ_WORKERS=32
export ZVEC_DISKANN_SCALAR_CACHE_MIB=1
```

新增标量缓存默认 0，即保留原有 mmap 策略，范围 0–64 MiB；正值只作用于含 DiskANN 的只读集合，每个 IPC 文件分别计预算。图节点缓存仍是单独预算。已发布的 Rust／Node SDK 尚未包含本轮源码变更。

微软组仍是 **Microsoft Rust DiskANN 0.61.0 + 自定义 SQLite/PQ 适配层**，SQLite 页缓存约 256 KiB、mmap 上限 16 MiB；并非微软官方原生 SSD 引擎。微软 L=128、重排 128，两边图参数和码本不同。

**缓存预算不是整个进程 RSS 的硬上限。** Arrow 元数据、活动批次、并发结果、PQ、线程及分配器另占内存。标量普通文件读取会使用系统文件缓存；本轮降低的是进程 RSS，不能据此声称整机物理内存等量下降或已证明其他应用不受影响。图节点读取保留 `F_NOCACHE`，未清空系统文件缓存，其他桌面应用未隔离。

## 验证与复现

- 先在独立验证集试验，再做三轮交替对照，随后冻结源码、库、测量程序和参数。正式测试不调参、不复跑、不挑选慢分片。源数据摘要与索引文件大小、修改时间前后核对一致。
- 每个有效进程在任何查询前用 `dladdr` 检查实际动态库路径，使用独立可执行文件和绝对依赖；父进程检查库和程序 SHA-256。保留 {audit['verified_processes']} 份有效原始记录：13 次试验、27 次验证、42 次正式测试。一次原型启动在进入测量程序前失败，没有产生查询结果，单独保留。
- **136 项 C++ 测试通过，1 项跳过**：正排存储 47 项、Collection 4 项、分段行号 16 项、索引关闭回归 69 项。覆盖页淘汰、并发、乱序与重复行、空值、空字符串、跨页长字符串、结果生命周期、截断读取失败与类型回退。压缩回退测试因本地 Arrow 未构建压缩 codec 而跳过，未声称该路径在当前环境已测。未运行新的 ThreadSanitizer。
- Rust 查询循环沿用上轮程序，没有修改计时、预热或结果获取逻辑。Python 报告脚本从原始 JSONL 和 GT 独立复算 QPS、RSS、召回及结果摘要，并验证查询集合互斥。

[核验记录](results/scalar-cache/audit.json)、[正式汇总](results/scalar-cache/measure/summary.json)、[原始结果与 GT](results/scalar-cache/raw-evidence.json.gz)、[冻结配置](results/scalar-cache/measure/configuration.json)、[源码补丁](results/scalar-cache/measure/source.patch)、[三轮验证](results/scalar-cache/validation/summary.json)、[C++ 测试](results/scalar-cache/cpp-tests.json)。

复现入口：[构建](build_scalar_variant.py)、[预算试验](run_scalar_pilot.py)、[冻结对比](run_scalar_comparison.py)、[核验与绘图](publish_scalar_optimization.py)。本机完整目录为 `/tmp/zvec-bounded-scalar-20261105`。[上一轮报告](REPORT_MEMORY_BASELINE_CN.md)保留原有数据和内存优化说明。
'''
    (HERE/'REPORT_SCALAR_CACHE_CN.md').write_text(report)
    import matplotlib
    matplotlib.use('Agg')
    import matplotlib.pyplot as plt
    from matplotlib import font_manager
    font_manager.fontManager.addfont('/System/Library/Fonts/STHeiti Light.ttc')
    plt.rcParams.update({'font.family':font_manager.FontProperties(fname='/System/Library/Fonts/STHeiti Light.ttc').get_name(),'font.size':12,'axes.unicode_minus':False,'svg.hashsalt':'scalar-cache'})
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
    fig.text(.5,.047,'zvec：8 MiB 节点缓存 + 1 MiB 标量页缓存；双方实际召回及图参数不同。',ha='center',fontsize=10)
    fig.text(.5,.015,'本地 zvec 6473f807 · Apple M3 Pro / 36 GiB / macOS · 2026-11-05',ha='center',fontsize=9,color='#555555')
    fig.subplots_adjust(left=.08,right=.985,top=.80,bottom=.28,wspace=.25)
    fig.savefig(dest/'qps-memory.png',dpi=200);fig.savefig(dest/'qps-memory.svg',metadata={'Date':None});plt.close(fig)
    svg=dest/'qps-memory.svg';svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines())+'\n')
    print(json.dumps(audit,indent=2))

if __name__=='__main__':main()
