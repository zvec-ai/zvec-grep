"""Independently audit the disjoint experiment and refresh the Markdown report."""
import argparse
from datetime import datetime
import gzip
import hashlib
import json
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import numpy as np

HERE=Path(__file__).resolve().parent
REPO=HERE.parents[2]
COUNTS=[200000,400000,600000,800000,1000000]

def sha(path):
    with Path(path).open('rb') as f: return hashlib.file_digest(f,'sha256').hexdigest()

def save(path,value):
    path.write_text(json.dumps(value,indent=2,ensure_ascii=False)+'\n')

def near(a,b):
    assert abs(a-b)<1e-8*max(1,abs(a),abs(b)),(a,b)

def main():
    p=argparse.ArgumentParser()
    p.add_argument('--base',type=Path,required=True)
    a=p.parse_args()
    src=a.base/'measure'
    data=a.base/'data'
    assert json.loads((src/'status.json').read_text())['phase']=='complete'
    config=json.loads((src/'configuration.json').read_text())
    manifest=json.loads((data/'manifest.json').read_text())
    runs=json.loads((src/'runs.json').read_text())
    summary=json.loads((src/'summary.json').read_text())
    selection=json.loads((src/'selection.json').read_text())
    assert sha(data/'manifest.json')==config['data_manifest_sha256']
    for f,info in manifest['files'].items(): assert sha(data/f)==info['sha256']
    for kind in ['binary','library']: assert sha(config[kind]['path'])==config[kind]['sha256']
    for f,digest in config['source_files_sha256'].items(): assert sha(HERE/f)==digest
    for f,digest in config['native_source_sha256'].items():
        assert sha(Path(config['source'])/f)==digest
        blob=subprocess.check_output(['git','-C',config['source'],'show',f"{config['source_commit']}:{f}"])
        assert hashlib.sha256(blob).hexdigest()==digest
    memory_source='src/core/algorithm/diskann/diskann_searcher_entity.cc'
    memory_blob=subprocess.check_output(['git','-C',config['source'],'show',f"{config['source_commit']}:{memory_source}"])
    assert hashlib.sha256(memory_blob).hexdigest()==sha(Path(config['source'])/memory_source)
    for f,digest in config['microsoft_source_sha256'].items(): assert sha(REPO/f)==digest
    assert sha(Path(config['pq_preparation'])/'codes.bin')==config['pq_codes_sha256']
    for f,stat in json.loads((src/'index-inventory.json').read_text()).items():
        assert [Path(f).stat().st_size,Path(f).stat().st_mtime_ns]==stat
    seen_rows=set(manifest['previous_query_row_ids'])
    seen32=set();seen16=set();query_fingerprints={}
    for name,rows in manifest['query_source_row_ids'].items():
        assert len(rows)==len(set(rows)) and not set(rows)&seen_rows
        seen_rows.update(rows)
        x=np.fromfile(data/f'{name}.f32',dtype='<f4').reshape(-1,768)
        assert len(x)==len(rows)
        hashes=[]
        for v in x:
            k=hashlib.sha256(v.tobytes()).hexdigest()
            k16=hashlib.sha256(v.astype('<f2').tobytes()).hexdigest()
            assert k not in seen32 and k16 not in seen16
            seen32.add(k);seen16.add(k16);hashes.append(k)
        query_fingerprints[name]=hashes
    assert len(seen32)==len(seen16)==4000
    records=[]
    for r in runs:
        path=src/r['raw_file']
        raw=[json.loads(line) for line in path.read_text().splitlines()]
        actual=next(x for x in raw if x['stage']=='queried')
        ids=next(x['ids'] for x in raw if x['stage']=='results')
        name=f"test-{r['shard']}" if r['phase']=='measure' else 'validation'
        truth=json.loads((data/f"{name}-truth-{r['count']}.json").read_text())
        q=500 if r['phase']=='measure' else 400
        assert len(ids)==len(truth)==actual['queries']==q
        assert actual['test_passes']==1 and actual['test_warmup_passes']==0 and actual['warmup_queries']==600
        assert all(len(set(row))==10 and all(0<=int(s[1:])<r['count'] for s in row) for row in ids)
        hits=sum(len(set(left).intersection(right)) for left,right in zip(ids,truth))
        assert hits==r['recall_hits']
        near(r['recall_at_10'],hits/(q*10))
        near(r['qps'],q/actual['batch_s'])
        assert hashlib.sha256(json.dumps(ids,separators=(',',':')).encode()).hexdigest()==r['results_sha256']
        times=sorted(actual['latencies_ms'])
        assert len(times)==q and all(t>0 for t in times)
        near(actual['p50_ms'],times[q//2]);near(actual['p95_ms'],times[q*95//100])
        assert sum(times)/1000<=actual['batch_s']*1.001
        stderr=path.with_suffix('.stderr').read_text()
        near(r['rss_mib'],int(re.search(r'(\d+)\s+maximum resident set size',stderr).group(1))/2**20)
        assert r['rss_mib']>=actual['rss_peak_after_test_mib']
        if r['phase']=='measure':
            assert r['recall_at_10']>=.90
            assert r['effort']==selection['settings'][f"{r['backend']}-{r['count']}"]['effort']
            assert datetime.fromisoformat(r['finished'])>datetime.fromisoformat(selection['selected_at'])
        records.append(dict(name=r['raw_file'],jsonl=path.read_text(),stderr=stderr))
    comparison=[]
    for n in COUNTS:
        for backend in ['zvec','microsoft']:
            group=[r for r in runs if r['phase']=='measure' and r['count']==n and r['backend']==backend]
            assert len(group)==6 and sorted(r['shard'] for r in group)==list(range(6))
            item=next(r for r in summary if r['count']==n and r['backend']==backend)
            near(item['qps'],3000/sum(r['batch_s'] for r in group))
            near(item['recall_at_10'],sum(r['recall_hits'] for r in group)/30000)
            for metric in ['rss_mib','qps','recall_at_10','rss_peak_after_open_mib','rss_peak_after_warmup_mib']:
                values=[r[metric] for r in group]
                near(item[metric+'_median'],statistics.median(values))
                near(item[metric+'_min'],min(values));near(item[metric+'_max'],max(values))
            if backend=='zvec':
                peer=next(r for r in summary if r['count']==n and r['backend']=='microsoft')
                wins=sum(r['qps']>next(m['qps'] for m in runs if m['phase']=='measure' and m['backend']=='microsoft' and m['count']==n and m['shard']==r['shard']) for r in group)
                comparison.append(dict(count=n,qps_ratio=item['qps']/peer['qps'],paired_wins=wins))
    # Cache budget intervention must leave the validation results unchanged.
    memory=[]
    for budget in [0,64]:
        group=[r for r in runs if r['phase']=='memory' and r['cache_mib']==budget]
        assert len(group)==3 and len({r['results_sha256'] for r in group})==1
        memory.append(dict(cache_mib=budget,rss_mib=statistics.median(r['rss_mib'] for r in group),qps=statistics.median(r['qps'] for r in group),recall=group[0]['recall_at_10']))
    assert len({r['results_sha256'] for r in runs if r['backend']=='zvec' and r['count']==1000000 and r['phase']!='measure' and r['effort']==selection['settings']['zvec-1000000']['effort']})==1
    profile_row=next(r for r in runs if r['phase']=='profile')
    profile_text=(src/profile_row['raw_file']).with_suffix('.stderr').read_text()
    profile=json.loads(re.search(r'DISKANN_READ_PROFILE (\{.*\})',profile_text).group(1))
    # Independent full-sort GT check on one previously unchecked query per prefix.
    base_file=Path('/tmp/zvec-cohere-million/data/base.f32')
    assert sha(base_file)==manifest['base_sha256']
    base=np.memmap(base_file,dtype='<f4',mode='r',shape=(1000000,768))
    q=np.fromfile(data/'test-3.f32',dtype='<f4').reshape(500,768)[217].astype(np.float64)
    q/=np.linalg.norm(q)
    all_scores=[]
    gt_spotchecks=[]
    for n in COUNTS:
        for start in range(n-200000,n,20000):
            b=base[start:start+20000].astype(np.float64)
            b/=np.linalg.norm(b,axis=1)[:,None]
            all_scores.extend(np.einsum('ij,j->i',b,q,optimize=False).tolist())
        order=np.argsort(-np.asarray(all_scores),kind='stable')[:10]
        truth=json.loads((data/f'test-3-truth-{n}.json').read_text())[217]
        assert truth==[f'v{i:08}' for i in order]
        gt_spotchecks.append(dict(count=n,shard=3,query_position=217,method='independent scalar dot and full stable sort',passed=True))
    dest=HERE/'results/disjoint-comparison'
    dest.mkdir(parents=True,exist_ok=True)
    for name in ['configuration.json','selection.json','summary.json','environment.json','index-inventory.json','runs.json']:
        shutil.copy2(src/name,dest/name)
    shutil.copy2(data/'manifest.json',dest/'data-manifest.json')
    for name in ['loader.json','overlap-rejection.json']:
        shutil.copy2(a.base/name,dest/name)
    evidence=dict(records=records,truth={f.name:json.loads(f.read_text()) for f in data.glob('*truth-*.json')},query_fingerprints=query_fingerprints)
    with (dest/'raw-evidence.json.gz').open('wb') as f:
        with gzip.GzipFile(fileobj=f,mode='wb',mtime=0,filename='') as g:
            g.write(json.dumps(evidence,separators=(',',':')).encode())
    audit=dict(passed=True,query_rows_unique=4000,fp32_and_fp16_unique=True,previous_queries_excluded=True,
               formal_processes=60,formal_queries_per_backend_scale=3000,records_checked=len(runs),
               fresh_process_single_test_pass=True,test_queries_never_used_for_warmup_or_selection=True,
               gt_independent_spotchecks=gt_spotchecks,summary_recomputed=True,source_and_binary_hashes_verified=True,
               memory_source_sha256={memory_source:hashlib.sha256(memory_blob).hexdigest()},
               indexes_unchanged=True,comparison=comparison,memory_control=memory,profile=profile,
               raw_evidence_sha256=sha(dest/'raw-evidence.json.gz'))
    save(dest/'audit.json',audit)
    build=dict(rustc=subprocess.check_output(['/Users/xingrui/.cargo/bin/rustc','--version'],text=True).strip(),
               source_commit=config['source_commit'],native_library_sha256=config['library']['sha256'],
               native_library_reused_from='results/node-comparison/build.json',new_harness=True,
               command='ZVEC_LIB_DIR=/tmp/zvec-node-cohere-20261104/optimized ZVEC_AUTO_BUILD=0 CARGO_TARGET_DIR=/tmp/zvec-disjoint-cohere-20261105/rust-target RUSTFLAGS="-C link-arg=-Wl,-headerpad_max_install_names" cargo build --release --manifest-path harness/Cargo.toml --features experimental-pq --bin disjoint-bench',
               overlap_rejection_passed=True,source_and_binary=config)
    save(dest/'build.json',build)
    shutil.copy2(a.base/'build.log',dest/'build.txt')
    # Preserve the startup-check failure; it occurred after one validation run, before test queries.
    failed=a.base/'measure-loader-check-failed'
    save(dest/'preflight-record.json',dict(reason='DYLD_PRINT_LIBRARIES was stripped when launched through /usr/bin/time; use a direct loader-only overlap-rejection probe instead',
        formal_test_queries_executed=0,log=(a.base/'measure-loader-check-failed.log').read_text(),
        validation_records=[dict(name=f.name,contents=f.read_text()) for f in (failed/'raw').iterdir()],
        data_preparation_failures=[(a.base/f).read_text() for f in ['prepare-path-failed.log','prepare-manifest-failed.log']]))
    def row(n,b): return next(r for r in summary if r['count']==n and r['backend']==b)
    table='\n'.join(f"| {n//10000} 万 | {row(n,'zvec')['qps']:.2f} | {row(n,'microsoft')['qps']:.2f} | {row(n,'zvec')['rss_mib']:.2f} | {row(n,'microsoft')['rss_mib']:.2f} | {row(n,'zvec')['recall_at_10']:.2%} / {row(n,'microsoft')['recall_at_10']:.2%} |" for n in COUNTS)
    ranges='\n'.join(f"| {n//10000} 万 | {row(n,'zvec')['qps_min']:.2f}–{row(n,'zvec')['qps_max']:.2f} | {row(n,'microsoft')['qps_min']:.2f}–{row(n,'microsoft')['qps_max']:.2f} | {row(n,'zvec')['recall_at_10_min']:.2%} / {row(n,'microsoft')['recall_at_10_min']:.2%} | {next(c['paired_wins'] for c in comparison if c['count']==n)} / 6 |" for n in COUNTS)
    valtable='\n'.join(f"| {n//10000} 万 | {selection['settings'][f'zvec-{n}']['effort']} | {selection['settings'][f'zvec-{n}']['validation_recall']:.2%} | 128 | {selection['settings'][f'microsoft-{n}']['validation_recall']:.2%} |" for n in COUNTS)
    stages='\n'.join(f"| {b} | {row(1000000,b)['rss_peak_after_open_mib']:.2f} | {row(1000000,b)['rss_peak_after_warmup_mib']:.2f} | {row(1000000,b)['rss_mib']:.2f} |" for b in ['zvec','microsoft'])
    z=row(1000000,'zvec');m=row(1000000,'microsoft')
    faster='、'.join(f"{c['count']//10000} 万" for c in comparison if c['qps_ratio']>1) or '无'
    slower='、'.join(f"{c['count']//10000} 万" for c in comparison if c['qps_ratio']<=1) or '无'
    minrec=min(r['recall_at_10'] for r in runs if r['phase']=='measure')
    faults=[r for r in runs if r['phase']=='measure' and r['major_faults']>0]
    report=f'''# Cohere：独立预热、单遍未知查询的 DiskANN 对比

历史基线：本文使用源码 `7d3a413e`，保留首次未知查询测试的数据和当时结论。后续源码 `047ebaf4` 在 60 万至 100 万下的优化、全新查询确认及单遍回归见[最新报告](REPORT_NODE_CN.md)。

测试日期：2026-11-05。Apple M3 Pro、36 GiB、macOS，本地 zvec 源码 `{config['source_commit'][:8]}`。

本轮使用互不重叠的 **600 条预热查询、400 条参数验证查询和 3,000 条正式查询**。正式查询分成六组，每组 500 条，在新的查询进程中只执行一遍。两种实现使用相同分组与顺序，正式查询没有用于预热或调参。

本轮结果为：**20 万时 zvec 更快，40 万基本持平，60 万至 100 万时微软方案更快。** 百万组为 **{z['qps']:.2f} 对 {m['qps']:.2f} QPS**，zvec 为微软方案的 **{z['qps']/m['qps']:.2f} 倍**。全部 60 个正式进程的 Recall@10 均超过 90%，最低为 **{minrec:.2%}**。此前“全部规模超过微软”的结论仅适用于[重复热查询测试](REPORT_NODE_WARM_CN.md)，不能沿用到本轮未知查询。

40 万组的合并 QPS 仅相差 **{(row(400000,'zvec')['qps']/row(400000,'microsoft')['qps']-1)*100:.2f}%**，六个分组 zvec 胜出 4 组、落后 2 组，应视为本轮基本持平，不能声称稳定领先。

## QPS、RSS 与召回率

QPS = 3,000 / 六个分组的计时秒数之和；RSS 为六个新进程的查询进程峰值中位数，单位 MiB。Recall@10 按 3,000 × 10 个真实邻居合并计算。

| 向量数量 | zvec DiskANN QPS | 微软 QPS | zvec RSS（MiB） | 微软 RSS（MiB） | zvec / 微软 Recall@10 |
|---|---:|---:|---:|---:|---:|
{table}

![未知查询的 QPS 和 RSS](results/disjoint-comparison/qps-memory.png)

| 向量数量 | zvec 分组 QPS 范围 | 微软分组 QPS 范围 | 分组最低 Recall：zvec / 微软 | zvec 配对领先组数 |
|---|---:|---:|---:|---:|
{ranges}

这里的六组使用不同查询，范围同时反映查询难度差异和系统波动，不是同一查询集的重复测量置信区间。{len(faults)} 个正式进程在计时阶段记录到 major faults，全部保留，没有按结果筛选进程。

80 万组波动较大：zvec 有两个分组低于 300 QPS，微软也有一个分组约 302 QPS，均未出现计时阶段 major faults，且这些分组的 CPU 用时也增加了。这使合并 QPS 低于百万组；现有记录不能区分查询工作量、CPU 调度等因素的各自贡献。全部慢分组仍计入结果，这条曲线不能解释为“增加到百万向量会提高性能”。

## 为什么结果与此前不同

此前先把计分查询完整预热两遍，再重复计时五遍，缓存可以反复复用这些查询的热点节点。本轮每个测试查询此前都未在该进程执行过，缓存只能受益于独立预热和其他不同查询共享的节点。原来测出的 1.37–4.65 倍优势不能代表这样的未知查询负载。

本轮还更换了查询样本，并基于独立验证集把 60 万、80 万的 zvec L 从 96 提高到 128；其他规模保持原 L。更大的 L 通常需要搜索更多候选。因此两轮差额包含预热方式、查询难度和部分搜索参数变化，不能把差额全部归因于缓存。图、PQ、向量精度和缓存实现沿用上一轮，全部正式测试结果保留。

## 为什么微软方案 RSS 较低

百万组 zvec 为 **{z['rss_mib']:.2f} MiB**，微软为 **{m['rss_mib']:.2f} MiB**，相差 **{z['rss_mib']-m['rss_mib']:.2f} MiB**。已确认的原因包括：

1. zvec 有额外的 **64 MiB 节点缓存预算**。微软适配层的 [SQLite 读取配置](../../crates/zg-diskann/src/lib.rs) 为 256 KiB 堆页缓存、最多 16 MiB mmap，没有同样的 64 MiB 节点缓存。
2. 两边都使用 PQ64，百万条 PQ 编码本体都是约 **61.04 MiB**；微软的 [PqIndex](../../crates/zg-diskann/src/experimental_pq.rs) 将这些编码放在常驻 `Vec<u8>` 中。因此微软 RSS 较低不能解释为“没有加载 PQ”。
3. zvec 还加载节点 key 和 key→ID 映射，[源码](https://github.com/richyreachy/zvec/blob/{config['source_commit']}/src/core/algorithm/diskann/diskann_searcher_entity.cc#L243)中分别为每条 8 和 4 字节，百万条合计约 **11.44 MiB** 的数组数据。完整 Collection 的其他元数据、读取临时缓冲、线程及分配器也会进入进程 RSS，但本轮没有把剩余部分逐项归因。

另外在百万索引上，用独立验证查询做了三组缓存开关对照，均为 8 个读取工作线程、有序结果一致：

| zvec 缓存预算 | 查询进程峰值 RSS 中位数（MiB） | 验证查询 QPS 中位数 |
|---|---:|---:|
| 0 MiB | {memory[0]['rss_mib']:.2f} | {memory[0]['qps']:.2f} |
| 64 MiB | {memory[1]['rss_mib']:.2f} | {memory[1]['qps']:.2f} |

关闭这项缓存后 RSS 降低 **{memory[1]['rss_mib']-memory[0]['rss_mib']:.2f} MiB**。这直接验证了节点缓存的主要内存代价；上表使用验证查询，不能替代主表的正式查询成绩。

百万组的阶段累计峰值中位数如下。数值为进程从启动至该阶段的最高 RSS，不能当作该阶段新增分配量：

| 实现 | 打开索引后（MiB） | 独立预热后（MiB） | 整个进程峰值（MiB） |
|---|---:|---:|---:|
{stages}

zvec 保留 `F_NOCACHE`、关闭预读；微软 SQLite 读取仍可使用系统文件缓存。RSS 不覆盖进程外全部系统文件缓存，主表证明的是进程 RSS 差异，不能单独据此比较对整机其他应用的全部内存影响。

## 数据、参数与计时口径

- 沿用此前 Wikipedia-Cohere / Big ANN 的同一组 20／40／60／80／100 万样本索引，768 维、cosine、FP16、PQ64、top-10。索引文件大小与修改时间前后未变。
- 从已有的官方 5,000 条查询中，固定 PCG64 seed=20261105，排除此前使用的 440 条查询、与库内向量完全相同的查询、重复查询，再抽取 4,000 条。预热、验证及六个测试分片在原始行号、归一化 FP32 内容和 FP16 内容三个层面互不重叠。这里只排除完全相同的向量，不承诺语义相近查询彼此独立。
- 每个规模均重新计算精确 GT：对实际 FP32 样本重新以 FP64 归一化，穷举 cosine，保留 top-10，平分时按 ID 排序。生成阶段每个规模用 4 条查询独立计算标量点积交叉检查；发布核验另用一条不同查询对每个规模做标量点积与全排序检查。
- 参数只在 400 条验证查询上选择：从原 L 开始，验证 Recall@10 达到 92% 后冻结，为正式分组的 90% 要求留余量。正式查询不用于选择参数。双方实际召回不同，本报告不声称完全相同召回率。

| 向量数量 | zvec L | zvec 验证 Recall | 微软 L | 微软验证 Recall |
|---|---:|---:|---:|---:|
{valtable}

- zvec 图出度配置 32、构图候选数 100、每个规模独立 PQ；微软图出度配置 48、构图候选数 100、FP16 重排 128、共享此前训练的 PQ64。微软组仍是 **微软 Rust DiskANN 0.61.0 + 自定义 SQLite/PQ 适配层**，并非官方原生 SSD 存储引擎。
- zvec 固定 64 MiB 节点缓存、8 个 I/O 工作线程；微软查询路径是同步 SQLite。查询并发均为 1，不能将本报告称为同线程数、同缓存策略的比较。
- 新测试程序对双方复用相同本地 zvec 动态库及运行时初始化，微软的向量搜索仍调用原 Rust/SQLite/PQ 路径，相关源码摘要与上一轮一致。
- 每个正式进程先执行独立预热集 600 条一次，然后执行自己的 500 条测试查询一次。不同测试查询之间可以自然共享缓存；这代表持续接收新查询的场景，没有在每条查询之间清空缓存。
- QPS 包含 FP16 转换、搜索和结果 ID 提取，不含索引打开、输入校验、独立预热及离线 GT 比较。RSS 使用 `/usr/bin/time -l` 的完整查询进程峰值，包含打开、查询数据加载、预热、搜索及退出，不含建库或 GT 计算。
- 六个分片交替规模顺序，并交替双方顺序。每次均为新进程；没有清空系统文件缓存，因此不是完全冷盘测试，也不是单个长进程的长期稳态测试。正式计时时没有并行构建或 GT 计算；其他桌面应用未隔离。

## 复现与核验

正式查询共 60 个进程、每个实现每个规模 3,000 条；本轮总共核验 {len(runs)} 份有效进程记录，包括独立验证、内存对照与单独的读取诊断。额外诊断统计覆盖 600 条预热加 400 条验证，不能与正式计时混用：逻辑请求 {profile['requests']:,} 次，`pread` {profile['pread_syscalls']:,} 次，API 读取 {profile['read_bytes']/2**20:.2f} MiB。

[原始结果、GT 与查询指纹](results/disjoint-comparison/raw-evidence.json.gz)、[数据分组与摘要](results/disjoint-comparison/data-manifest.json)、[逐进程记录](results/disjoint-comparison/runs.json)、[汇总](results/disjoint-comparison/summary.json)、[独立核验](results/disjoint-comparison/audit.json)、[冻结参数](results/disjoint-comparison/selection.json)、[构建记录](results/disjoint-comparison/build.json)。

程序 [disjoint-bench.rs](harness/src/bin/disjoint-bench.rs) 在查询开始前拒绝重叠或重复输入；该拒绝路径已实际验证。复现依次使用 [prepare_disjoint.py](prepare_disjoint.py)、[run_disjoint_comparison.py](run_disjoint_comparison.py) 和 [publish_disjoint_comparison.py](publish_disjoint_comparison.py)，参数见 `--help`，保留原 Cohere 数据、索引和记录中的本地库路径。测量脚本需要 `BASE/data`、`BASE/rust-target/release` 中编译的程序及动态库，以及直接启动程序记录的 `loader.json`；本次完整实验目录为 `/tmp/zvec-disjoint-cohere-20261105`。输入摘要、动态库加载路径、返回 ID、召回率、计时和汇总均独立检查。

初次准备时修正了历史查询文件路径和清单来源；首次加载检查由于 `/usr/bin/time` 未传递 dyld 输出而失败，改为直接启动的加载检查后确认了库路径。这些[预检查记录](results/disjoint-comparison/preflight-record.json)发生在正式查询之前，未计为正式成绩。

本轮未改动 zvec 核心库，复用此前已通过 [40 项 C++ 测试](results/node-comparison/cpp-tests.json)的 `7d3a413e` 动态库。已发布 Rust／Node SDK 二进制尚未替换。
'''
    (HERE/'REPORT_UNSEEN_BASELINE_CN.md').write_text(report)
    import matplotlib
    matplotlib.use('Agg')
    import matplotlib.pyplot as plt
    from matplotlib import font_manager
    font='/System/Library/Fonts/STHeiti Light.ttc'
    font_manager.fontManager.addfont(font)
    plt.rcParams.update({'font.family':font_manager.FontProperties(fname=font).get_name(),'font.size':11,
                         'axes.spines.top':False,'axes.spines.right':False,'axes.unicode_minus':False,'svg.hashsalt':'zvec-disjoint-comparison'})
    fig,axes=plt.subplots(1,2,figsize=(13,6.4))
    series=[('microsoft','微软 Rust DiskANN + SQLite/PQ','#163B65','s','--'),('zvec','zvec DiskANN','#E87722','o','-')]
    for ax,metric,title,unit in [(axes[0],'qps','未知查询吞吐量 · 越高越好','QPS（次/秒）'),(axes[1],'rss_mib','查询进程峰值 RSS · 越低越好','MiB')]:
        for backend,label,color,marker,style in series:
            xs=[n/10000 for n in COUNTS];ys=[row(n,backend)[metric] for n in COUNTS]
            ax.plot(xs,ys,color=color,marker=marker,linestyle=style,label=label,linewidth=2.3,markersize=6)
            for n,x,y in zip(COUNTS,xs,ys):
                peer=row(n,'zvec' if backend=='microsoft' else 'microsoft')[metric]
                ax.annotate(f'{y:.2f}',(x,y),xytext=(0,11 if y>=peer else -21),textcoords='offset points',ha='center',fontsize=10,color=color)
        ax.set_xticks([20,40,60,80,100],['20 万','40 万','60 万','80 万','100 万'])
        ax.set_xlim(12,108);ax.set_ylim(0,max(row(n,b)[metric] for n in COUNTS for b,_,_,_,_ in series)*1.22)
        ax.set(title=title,xlabel='向量数量（768 维）',ylabel=unit);ax.grid(axis='y',alpha=.18)
    fig.suptitle('Cohere：独立预热、单遍未知查询\nFP16 · PQ64 · 查询并发 1 · 所有分组 Recall@10 ≥ 90%',fontsize=15,y=.98)
    fig.legend(*axes[0].get_legend_handles_labels(),loc='lower center',bbox_to_anchor=(.5,.10),ncol=2,frameon=False)
    fig.text(.5,.073,'600 条独立预热；3,000 条正式查询分 6 组，每条只执行一次。',ha='center',fontsize=10)
    fig.text(.5,.039,'QPS = 总查询数 / 总计时；RSS = 六个进程峰值的中位数。图参数、缓存和 I/O 线程配置不同。',ha='center',fontsize=10)
    fig.text(.5,.007,'本地 zvec 7d3a413e · Apple M3 Pro / 36 GiB / macOS · 2026-11-05',ha='center',fontsize=9,color='#555555')
    fig.subplots_adjust(left=.075,right=.985,top=.80,bottom=.27,wspace=.25)
    fig.savefig(dest/'qps-memory.png',dpi=200)
    fig.savefig(dest/'qps-memory.svg',metadata={'Date':None})
    plt.close(fig)
    f=dest/'qps-memory.svg';f.write_text('\n'.join(line.rstrip() for line in f.read_text().splitlines())+'\n')
    print(json.dumps(audit,ensure_ascii=False,indent=2))

if __name__=='__main__': main()
