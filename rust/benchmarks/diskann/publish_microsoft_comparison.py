"""Audit, report and plot the paired zvec / Microsoft Rust+SQLite comparison."""
import argparse
import gzip
import hashlib
import json
from pathlib import Path
import re
import shutil
import statistics

parser = argparse.ArgumentParser()
parser.add_argument("experiment",type=Path)
parser.add_argument("data",type=Path)
parser.add_argument("--font",type=Path,default=Path('/System/Library/Fonts/STHeiti Light.ttc'))
a=parser.parse_args()
here=Path(__file__).resolve().parent
source=a.experiment
dest=here/'results/microsoft-comparison'
assert json.loads((source/'status.json').read_text())['stage']=='complete'
config=json.loads((source/'configuration.json').read_text())
runs=json.loads((source/'runs.json').read_text())
validation=json.loads((source/'validation.json').read_text())
summary=json.loads((source/'summary.json').read_text())
assert len(runs)==60 and len(validation)==10 and len(summary)==10
assert hashlib.sha256((a.data/'manifest.json').read_bytes()).hexdigest()==config['data_manifest_sha256']
for folder in [a.data,a.data/'validation']:
    for filename,meta in json.loads((folder/'manifest.json').read_text())['files'].items():
        with (folder/filename).open('rb') as f:
            assert hashlib.file_digest(f,'sha256').hexdigest()==meta['sha256']
truth={phase:{str(n):json.loads((folder/f'truth-{n}.json').read_text()) for n in config['counts']}
       for phase,folder in [('measure',a.data),('validation',a.data/'validation')]}
raw=[]
for row in validation+runs:
    path=source/row['raw_file']
    stages=[json.loads(line) for line in path.read_text().splitlines()]
    measured=next(r for r in stages if r['stage']=='queried')
    ids=next(r['ids'] for r in stages if r['stage']=='results')
    assert all(row[k]==v for k,v in measured.items())
    assert len(ids)==200 and all(len(set(r))==len(r)==10 for r in ids)
    recall=sum(len(set(x)&set(y)) for x,y in zip(ids,truth[row['phase']][str(row['count'])]))/2000
    assert recall==row['recall_at_10'] and recall >= (.92 if row['phase']=='validation' else .9)
    phases=measured['phases']
    assert [r['phase'] for r in phases]==['warmup']*2+['measured']*5
    assert [r['pass'] for r in phases]==list(range(7))
    assert measured['queries']==1000 and measured['passes']==5 and measured['full_warmup_passes']==2
    for phase in phases:
        assert abs(phase['qps']-200/phase['wall_s'])<1e-8
        assert all(v>=0 for v in phase['resources'].values())
    assert abs(measured['batch_s']-sum(p['wall_s'] for p in phases[2:]))<1e-8
    assert abs(measured['qps']-1000/measured['batch_s'])<1e-8
    for metric,keys in [('cpu_s',['user_s','system_s']),('measured_major_faults',['major_faults']),('voluntary_switches',['voluntary_switches'])]:
        value=sum(sum(p['resources'][k] for k in keys) for p in phases[2:])
        assert abs(row[metric]-value)<1e-8
    setting=next(r for r in config['parameters'] if r['backend']==row['backend'] and r['count']==row['count'])
    assert measured['effort']==setting['effort']
    if row['phase']=='validation': assert recall==setting['expected_validation_recall']
    if row['backend']=='microsoft': assert measured['rerank']==128 and measured['pq_codes_bytes']==row['count']*64
    stderr=path.with_suffix('.stderr').read_text()
    assert row['rss_mib']==int(re.search(r'(\d+)\s+maximum resident set size',stderr).group(1))/2**20
    raw.append(dict(file=row['raw_file'],stages=stages,time_stderr=stderr))
for row in summary:
    group=[r for r in runs if r['backend']==row['backend'] and r['count']==row['count']]
    assert len(group)==6 and {r['repeat'] for r in group}==set(range(6))
    for metric in ['qps','rss_mib','recall_at_10','mean_ms','p50_ms','p95_ms','cpu_s','batch_s','measured_major_faults','voluntary_switches']:
        assert row[metric]==statistics.median(r[metric] for r in group)
        assert row[metric+'_min']==min(r[metric] for r in group)
        assert row[metric+'_max']==max(r[metric] for r in group)
for path,digest in config['source_sha256'].items():
    assert hashlib.sha256((here.parents[2]/path).read_bytes()).hexdigest()==digest
dest.mkdir(parents=True,exist_ok=True)
for filename in ['configuration.json','validation.json','runs.json','summary.json','environment.json','index-inventory.json']:
    shutil.copy2(source/filename,dest/filename)
payload=json.dumps(dict(runs=raw,truth=truth),separators=(',',':')).encode()
(dest/'raw-evidence.json.gz').write_bytes(gzip.compress(payload,mtime=0))
(dest/'audit.json').write_text(json.dumps(dict(all_60_measured_processes_verified=True,all_10_validation_processes_verified=True,
    raw_evidence_uncompressed_sha256=hashlib.sha256(payload).hexdigest(),
    test_recall_min=min(r['recall_at_10'] for r in runs),
    measured_major_faults_max=max(r['measured_major_faults'] for r in runs)),indent=2)+'\n')

table=['| 向量数量 | zvec QPS | 微软 Rust + SQLite QPS | zvec RSS（MiB） | 微软 Rust + SQLite RSS（MiB） | zvec / 微软 Recall@10 |',
       '|---|---:|---:|---:|---:|---:|']
ranges=['| 向量数量 | zvec QPS 范围 | 微软方案 QPS 范围 | zvec RSS 范围（MiB） | 微软方案 RSS 范围（MiB） |',
        '|---|---:|---:|---:|---:|']
params=['| 向量数量 | zvec L | 微软 L / 重排数 | zvec / 微软验证集 Recall@10 |','|---|---:|---:|---:|']
for n in config['counts']:
    z,m=[next(r for r in summary if r['count']==n and r['backend']==b) for b in ['zvec','microsoft']]
    zv,mv=[next(r for r in validation if r['count']==n and r['backend']==b) for b in ['zvec','microsoft']]
    table.append(f"| {n//10000} 万 | {z['qps']:.2f} | {m['qps']:.2f} | {z['rss_mib']:.2f} | {m['rss_mib']:.2f} | {z['recall_at_10']:.2%} / {m['recall_at_10']:.2%} |")
    ranges.append(f"| {n//10000} 万 | {z['qps_min']:.2f}–{z['qps_max']:.2f} | {m['qps_min']:.2f}–{m['qps_max']:.2f} | {z['rss_mib_min']:.2f}–{z['rss_mib_max']:.2f} | {m['rss_mib_min']:.2f}–{m['rss_mib_max']:.2f} |")
    params.append(f"| {n//10000} 万 | {z['effort']} | {m['effort']} / 128 | {zv['recall_at_10']:.2%} / {mv['recall_at_10']:.2%} |")
z1,m1=[next(r for r in summary if r['count']==1000000 and r['backend']==b) for b in ['zvec','microsoft']]
env=json.loads((source/'environment.json').read_text())
report=f"""# zvec DiskANN 与微软 Rust DiskANN + SQLite 对比

测试日期：{env['finished'][:10]}。设备：{env['cpu']}、{env['memory_bytes']/2**30:g} GiB 内存、macOS。

**微软组是此前的微软 Rust DiskANN + 自定义 SQLite/PQ 适配方案，不是微软官方原生 SSD 存储实现。** zvec 使用本地源码 `{config['zvec_source_commit'][:8]}` 编译的原生库。本报告比较两套静态向量查询方案，不能据此给两种图算法或完整产品能力排位。

## 实测结果

相同 Cohere Wikipedia 样本，768 维、cosine、top-10、FP16 存储、查询先舍入 FP16、PQ64、单并发。沿用各自独立验证集已选参数，再次验证全部达到 92%；测试集目标为 Recall@10 ≥90%，未按测试结果调参。实际召回率不同，逐项列出。

两组均先用 20 条独立查询预热，再用完整 200 条测试查询预热 2 遍，最后计时 5 遍。每组 6 个新进程，按轮次交替规模和实现顺序；以下均为本轮中位数，没有拼接旧测试的计时数据。

{chr(10).join(table)}

![QPS 和查询峰值 RSS](results/microsoft-comparison/qps-memory.png)

100 万条时，微软方案的 QPS 为 zvec 的 **{m1['qps']/z1['qps']:.2f} 倍**。这描述本机、当前参数和有限查询工作集的结果，不是通用性能倍数。两组实际 Recall@10 分别为 zvec **{z1['recall_at_10']:.2%}**、微软方案 **{m1['recall_at_10']:.2%}**。

## zvec QPS 是否有提升空间

本轮百万组每 1000 次查询的墙钟耗时中位数为 zvec **{z1['batch_s']:.3f} 秒**、微软方案 **{m1['batch_s']:.3f} 秒**；对应 CPU 时间为 **{z1['cpu_s']:.3f} / {m1['cpu_s']:.3f} 秒**，主动上下文切换为 **{z1['voluntary_switches']:.0f} / {m1['voluntary_switches']:.0f} 次**。CPU 与墙钟时间不是同一种计量，也不把二者差额全部算成磁盘时间。

结合本地源码，**优先验证读盘与缓存优化的收益**更有依据：

- 当前 zvec DiskANN 的 macOS 读取路径默认请求 `F_NOCACHE`、关闭预读，读取批次走同步 `pread`；热点节点缓存默认值为 0。这有助于控制缓存占用，也可能增加重复查询的等待。`F_NOCACHE` 是请求内核尽量减少缓存，不保证每次读取都访问物理设备。
- 微软旧适配层采用 SQLite 256 KiB 堆页缓存和最多 16 MiB 文件映射，并可受益于系统文件缓存；PQ 编码常驻内存，前 128 个候选读取 FP16 原向量重排。其预热后优势不能仅归因于 PQ 算术或图搜索。
- 本地 zvec 已有 `BufferPoolAlignedFileReader` 和节点缓存路径，可以进一步测试有明确容量上限的热点页/节点缓存，例如 16／32／64 MiB，观察 QPS、RSS 和召回的变化。本轮未修改缓存策略，也未实测这些容量的收益。
- 应用当前默认 `L=max(300,topk)`，高于本轮 zvec 的 96／128。可将搜索候选数开放为配置，用真实工作区查询验证召回；不能直接把 Cohere 的最优参数当作应用通用参数。

以上是基于测量与代码的优化优先级判断，没有承诺优化后能达到微软组的 QPS。源码证据见 `zvec/src/core/algorithm/diskann/diskann_file_reader.cc`、`diskann_indexer.cc`、`diskann_searcher.cc`；微软适配见 [lib.rs](../../crates/zg-diskann/src/lib.rs)和[experimental_pq.rs](../../crates/zg-diskann/src/experimental_pq.rs)。

## 两套方案的差异

| 项目 | zvec DiskANN | 微软 Rust + SQLite |
|---|---|---|
| 实现 | 本地 zvec C++ `{config['zvec_source_commit'][:8]}` | `diskann` / `diskann-quantization` 0.61.0，加自定义存储与 PQ 查询适配 |
| 图参数 | max_degree=32，build_list=100 | max_degree=48，build_list=100 |
| PQ | 内置 PQ64，各规模独立建库 | PQ64，共享从前 20 万条抽样训练的码本 |
| 存储 | zvec 原生磁盘布局 | SQLite 保存图边和完整 FP16 向量 |
| 场景 | 本轮只读、无过滤向量查询 | 实验性只读、无过滤、连续且全存活节点的静态 PQ 路径 |
| 动态功能 | 本轮未测增删改与过滤 | 该实验性 PQ 路径尚未实现增删改、过滤和码本更新 |

双方复用此前各自构建的索引，实验前后文件大小、修改时间一致；码本、编码、数据及 GT 哈希重新核对。图参数、PQ 训练、存储及缓存策略不同，因此这是同一最低召回目标下的方案比较，不是只替换算法内核的单变量实验。

{chr(10).join(params)}

## 计量边界与波动

QPS = 1000 / 5 个计时遍次的累计秒数，含 FP16 查询转换、搜索、重排（微软组）、ID 提取和一致性检查，输入文件提前读入。两组都使用同一 zvec 库初始化单线程应用运行时；微软组的向量搜索调用微软 Rust 图/PQ 库。每遍结果 ID 一致，召回对照同一原始 FP32 的精确 cosine GT。

RSS 为 `/usr/bin/time -l` 的进程峰值驻留内存，包含打开、PQ 编码加载、预热和查询，不包含建库与训练。**RSS 不包含进程之外的全部系统文件缓存；微软组较低的 RSS 不能据此推导为较低的系统总内存占用。** 两组没有清空系统缓存或隔离其他应用，只报告固定查询工作集的预热后吞吐量，不代表冷启动、随机新查询或高并发。

{chr(10).join(ranges)}

## 原始证据与复现

[configuration.json](results/microsoft-comparison/configuration.json) 保存全部参数、两份二进制、库和源文件摘要；[validation.json](results/microsoft-comparison/validation.json)、[runs.json](results/microsoft-comparison/runs.json)、[summary.json](results/microsoft-comparison/summary.json) 保存验证、60 次正式测量和汇总；[audit.json](results/microsoft-comparison/audit.json) 记录独立核验。[raw-evidence.json.gz](results/microsoft-comparison/raw-evidence.json.gz) 包含 70 个进程的返回 ID、对应 GT、分阶段计数与原始计时输出。

建库、码本来源分别见 [zvec 初次记录](REPORT_NATIVE_CN.md)和[微软方案 PQ 记录](REPORT_PQ_CN.md)。复现本次对比需要保留这些实验目录，输出目录必须不存在。运行前按 [zvec 复测说明](REPORT_RETEST_NATIVE_CN.md)构建并固定链接 `native-retest`；微软适配程序使用同一构建环境：

```bash
BASE=/tmp/zvec-local-cohere-20261104
ZVEC_LIB_DIR="$BASE/build/lib" ZVEC_INCLUDE_DIR="$PWD/../zvec/src/include" ZVEC_AUTO_BUILD=0 \\
  RUSTFLAGS='-C link-arg=-Wl,-headerpad_max_install_names' \\
  cargo build --offline --release --manifest-path rust/benchmarks/diskann/harness/Cargo.toml \\
  --features experimental-pq --bin microsoft-retest --target-dir "$BASE/rust-target"
install_name_tool -change @rpath/libzvec_c_api.dylib "$BASE/build/lib/libzvec_c_api.dylib" \\
  "$BASE/rust-target/release/microsoft-retest"
codesign --force --sign - "$BASE/rust-target/release/microsoft-retest"
python3 rust/benchmarks/diskann/run_microsoft_comparison.py \\
  --native-binary "$BASE/rust-target/release/native-retest" \\
  --microsoft-binary "$BASE/rust-target/release/microsoft-retest" \\
  --data /tmp/zvec-cohere-million/data --native-experiment "$BASE/experiment" \\
  --microsoft-experiment /tmp/zvec-cohere-million/experiment \\
  --pq-experiment /tmp/zvec-cohere-pq64-20261104/experiment \\
  --pq-preparation /tmp/zvec-cohere-pq64-20261104/preparation \\
  --native-library "$BASE/build/lib/libzvec_c_api.dylib" --source-root "$PWD/../zvec" \\
  --output /tmp/zvec-microsoft-comparison-reproduction
python3 rust/benchmarks/diskann/publish_microsoft_comparison.py \\
  /tmp/zvec-microsoft-comparison-reproduction /tmp/zvec-cohere-million/data
```
"""
(here/'REPORT_MICROSOFT_CN.md').write_text(report)

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
from matplotlib import font_manager
font_manager.fontManager.addfont(str(a.font))
plt.rcParams.update({'font.family':font_manager.FontProperties(fname=str(a.font)).get_name(),
    'font.size':11,'axes.unicode_minus':False,'axes.spines.top':False,'axes.spines.right':False,
    'svg.hashsalt':'diskann-microsoft-comparison'})
fig,axes=plt.subplots(1,2,figsize=(13,6.4))
for ax,metric,title,unit in [(axes[0],'qps','查询吞吐量 · 越高越好','QPS（次/秒）'),
                            (axes[1],'rss_mib','查询峰值 RSS · 越低越好','MiB')]:
    for backend,label,color in [('zvec','zvec DiskANN','#E87722'),('microsoft','微软 Rust + SQLite','#163B65')]:
        group=[r for r in summary if r['backend']==backend]
        assert [r['count'] for r in group]==config['counts']
        x=[r['count']/10000 for r in group]; y=[r[metric] for r in group]
        ax.plot(x,y,'o-',label=label,color=color,linewidth=2.4,markersize=6)
        for px,py in zip(x,y):
            peer=next(r[metric] for r in summary if r['count']==int(px*10000) and r['backend']!=backend)
            offset=-22 if metric=='rss_mib' and py<peer else 11
            ax.annotate(f'{py:.2f}',(px,py),xytext=(0,offset),textcoords='offset points',ha='center',fontsize=10,color=color)
    ax.set_xticks([20,40,60,80,100],['20 万','40 万','60 万','80 万','100 万'])
    ax.set_xlim(12,108);ax.set_ylim(0,max(r[metric] for r in summary)*1.23)
    ax.set(title=title,ylabel=unit,xlabel='向量数量（768 维）');ax.grid(axis='y',alpha=.18)
fig.suptitle('zvec DiskANN 与微软 Rust + SQLite：QPS 和查询峰值 RSS\nCohere Wikipedia · FP16 · PQ64 · 单并发 · Recall@10 ≥ 90%',fontsize=15,y=.98)
fig.legend(*axes[0].get_legend_handles_labels(),loc='lower center',bbox_to_anchor=(.5,.105),ncol=2,frameon=False)
fig.text(.5,.078,'完整查询集预热 2 遍，再计时 5 遍；各组 6 次独立进程，取中位数。',ha='center',fontsize=10)
fig.text(.5,.045,'图参数、PQ 训练和缓存策略不同；实际召回率及完整范围见报告。',ha='center',fontsize=10)
fig.text(.5,.012,f"本地 zvec {config['zvec_source_commit'][:8]} / 微软 Rust 0.61.0 + SQLite · Apple M3 Pro / 36 GiB / macOS。",ha='center',fontsize=10,color='#555555')
fig.subplots_adjust(left=.075,right=.985,top=.80,bottom=.28,wspace=.25)
fig.savefig(dest/'qps-memory.png',dpi=200)
fig.savefig(dest/'qps-memory.svg',metadata={'Date':None})
plt.close(fig)
svg=dest/'qps-memory.svg';svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines())+'\n')
print('Audited 10 validation and 60 measured processes; published comparison report and plots')
