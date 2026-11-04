"""Independently audit the paired cache benchmark and publish the Chinese report."""
import argparse
import gzip
import hashlib
import json
import math
from pathlib import Path
import re
import shutil
import statistics
import subprocess

p=argparse.ArgumentParser()
p.add_argument('base',type=Path)
p.add_argument('data',type=Path)
a=p.parse_args()
here=Path(__file__).resolve().parent
dest=here/'results/cache-comparison'
metrics=['qps','rss_mib','recall_at_10','batch_s','cpu_s','open_s','p50_ms','p95_ms','measured_major_faults','voluntary_switches']
configs={}
summaries={}
all_raw=[]
truth={phase:{str(n):json.loads((folder/f'truth-{n}.json').read_text()) for n in [200000,400000,600000,800000,1000000]}
       for phase,folder in [('validation',a.data/'validation'),('measure',a.data)]}
for folder in [a.data,a.data/'validation']:
    for filename,meta in json.loads((folder/'manifest.json').read_text())['files'].items():
        with (folder/filename).open('rb') as f: assert hashlib.file_digest(f,'sha256').hexdigest()==meta['sha256']
for stage,expected in [('pilot',35),('measure',105)]:
    folder=a.base/stage
    assert json.loads((folder/'status.json').read_text())['stage']=='complete'
    config=json.loads((folder/'configuration.json').read_text()); configs[stage]=config
    for filename,digest in config['source_sha256'].items():
        content=subprocess.check_output(['git','-C',config['source'],'show',config['source_commit']+':'+filename])
        assert hashlib.sha256(content).hexdigest()==digest
    runs=json.loads((folder/'runs.json').read_text())
    summary=json.loads((folder/'summary.json').read_text()); summaries[stage]=summary
    assert len(runs)==expected
    assert config['data_manifest_sha256']==hashlib.sha256((a.data/'manifest.json').read_bytes()).hexdigest()
    ids_by_group={}
    for row in runs:
        path=folder/row['raw_file']
        raw=[json.loads(line) for line in path.read_text().splitlines()]
        measured=next(r for r in raw if r['stage']=='queried')
        ids=next(r['ids'] for r in raw if r['stage']=='results')
        assert all(row[k]==v for k,v in measured.items())
        assert len(ids)==200 and all(len(x)==len(set(x))==10 for x in ids)
        key=(row['phase'],row['count'],'microsoft' if row['backend']=='microsoft' else 'zvec')
        assert key not in ids_by_group or ids_by_group[key]==ids
        ids_by_group[key]=ids
        assert hashlib.sha256(json.dumps(ids,separators=(',',':')).encode()).hexdigest()==row['results_sha256']
        recall=sum(len(set(x)&set(y)) for x,y in zip(ids,truth[row['phase']][str(row['count'])]))/2000
        assert recall==row['recall_at_10'] and recall>=.9
        setting=config['microsoft_settings' if row['backend']=='microsoft' else 'settings'][str(row['count'])]
        assert row['effort']==setting['effort']
        if row['phase']=='validation': assert recall==setting['expected_validation_recall']
        assert measured['queries']==1000 and measured['passes']==5 and measured['full_warmup_passes']==2
        phases=measured['phases']; assert [r['phase'] for r in phases]==['warmup']*2+['measured']*5
        assert [r['pass'] for r in phases]==list(range(7))
        for phase in phases:
            assert abs(phase['qps']-200/phase['wall_s'])<1e-8
            assert all(v>=0 for v in phase['resources'].values())
        assert abs(sum(r['wall_s'] for r in phases[2:])-row['batch_s'])<1e-8
        assert abs(1000/row['batch_s']-row['qps'])<1e-8
        for metric,keys in [('cpu_s',['user_s','system_s']),('measured_major_faults',['major_faults']),('voluntary_switches',['voluntary_switches'])]:
            assert abs(row[metric]-sum(sum(r['resources'][k] for k in keys) for r in phases[2:]))<1e-8
        stderr=path.with_suffix('.stderr').read_text()
        assert row['rss_mib']==int(re.search(r'(\d+)\s+maximum resident set size',stderr).group(1))/2**20
        all_raw.append(dict(stage=stage,file=row['raw_file'],jsonl=raw,time_stderr=stderr))
    for row in summary:
        group=[r for r in runs if r['count']==row['count'] and r['backend']==row['backend'] and r['budget_mib']==row['budget_mib'] and r['phase']==('validation' if stage=='pilot' else 'measure')]
        assert len(group)==row['repeats']
        if stage=='measure': assert len(group)==6 and {r['repeat'] for r in group}==set(range(6))
        for metric in metrics:
            values=[r[metric] for r in group]
            assert row[metric]==statistics.median(values)
            assert row[metric+'_min']==min(values) and row[metric+'_max']==max(values)
    assert len(summary)==(20 if stage=='pilot' else 15)
    assert hashlib.sha256((folder/'runner.py').read_bytes()).hexdigest()==config['runner_sha256']
selection=json.loads((a.base/'pilot/selection.json').read_text())
scores={b:math.exp(statistics.mean(math.log(r['qps']) for r in summaries['pilot'] if r['budget_mib']==b)) for b in [16,32,64]}
for b,v in scores.items(): assert abs(v-selection['validation_geometric_mean_qps'][str(b)])<1e-8
budget=min(b for b,v in scores.items() if v>=max(scores.values())*.9)
assert budget==selection['budget_mib']
assert json.loads((a.base/'measure/selection.json').read_text())==selection
config=configs['measure']
for filename,digest in config['source_sha256'].items():
    assert hashlib.sha256((Path(config['source'])/filename).read_bytes()).hexdigest()==digest
assert hashlib.sha256((here/'run_cache_comparison.py').read_bytes()).hexdigest()==config['runner_sha256']
assert hashlib.sha256((here/'harness/src/bin/native-retest.rs').read_bytes()).hexdigest()==config['harness_sha256']
for name,library in config['libraries'].items():
    assert hashlib.sha256(Path(library['path']).read_bytes()).hexdigest()==library['sha256']
assert json.loads((a.base/'pilot/index-inventory.json').read_text())=={k:v for k,v in json.loads((a.base/'measure/index-inventory.json').read_text()).items() if not k.startswith('microsoft/')}
for name,digest in config['microsoft_source_sha256'].items():
    assert hashlib.sha256((here.parents[2]/name).read_bytes()).hexdigest()==digest
dest.mkdir(parents=True,exist_ok=True)
cpp_tests=json.loads((a.base/'cpp-tests.json').read_text())
assert cpp_tests['total_passed']==26 and cpp_tests['source_commit']==config['source_commit']
shutil.copy2(a.base/'cpp-tests.json',dest/'cpp-tests.json')
if (a.base/'startup-failed').exists():
    shutil.copytree(a.base/'startup-failed',dest/'startup-failed',dirs_exist_ok=True)
for stage in ['pilot','measure']:
    (dest/stage).mkdir(exist_ok=True)
    for name in ['configuration.json','runs.json','summary.json','selection.json','index-inventory.json','environment.json','source.patch','runner.py']:
        shutil.copy2(a.base/stage/name,dest/stage/name)
payload=json.dumps(dict(runs=all_raw,truth=truth),separators=(',',':')).encode()
(dest/'raw-evidence.json.gz').write_bytes(gzip.compress(payload,mtime=0))
measured=[r for r in json.loads((a.base/'measure/runs.json').read_text()) if r['phase']=='measure']
audit=dict(processes_verified=140,pilot_processes=35,final_validation_processes=15,measured_processes=90,
           ordered_ids_identical_between_zvec_variants=True,minimum_test_recall=min(r['recall_at_10'] for r in measured),
           raw_evidence_uncompressed_sha256=hashlib.sha256(payload).hexdigest(),cache_budget_mib=budget,
           measured_major_faults_max=max(r['measured_major_faults'] for r in measured))
(dest/'audit.json').write_text(json.dumps(audit,indent=2)+'\n')
summary=summaries['measure']
def row(n,backend): return next(r for r in summary if r['count']==n and r['backend']==backend)
table=['| 向量数量 | 缓存关闭 QPS | 缓存开启 QPS | QPS 倍数 | 缓存关闭 RSS（MiB） | 缓存开启 RSS（MiB） | 两版 Recall@10 |','|---|---:|---:|---:|---:|---:|---:|']
ranges=['| 向量数量 | 缓存关闭 QPS 范围 | 缓存开启 QPS 范围 | 缓存关闭 RSS 范围（MiB） | 缓存开启 RSS 范围（MiB） |','|---|---:|---:|---:|---:|']
validation_table=['| 向量数量 | 16 MiB：QPS / RSS | 32 MiB：QPS / RSS | 64 MiB：QPS / RSS | 验证 Recall@10 |','|---|---:|---:|---:|---:|']
for n in config['counts']:
    b,o=row(n,'baseline'),row(n,'optimized')
    table.append(f"| {n//10000} 万 | {b['qps']:.2f} | {o['qps']:.2f} | {o['qps']/b['qps']:.2f}× | {b['rss_mib']:.2f} | {o['rss_mib']:.2f} | {o['recall_at_10']:.2%} |")
    ranges.append(f"| {n//10000} 万 | {b['qps_min']:.2f}–{b['qps_max']:.2f} | {o['qps_min']:.2f}–{o['qps_max']:.2f} | {b['rss_mib_min']:.2f}–{b['rss_mib_max']:.2f} | {o['rss_mib_min']:.2f}–{o['rss_mib_max']:.2f} |")
    vals=[next(r for r in summaries['pilot'] if r['count']==n and r['budget_mib']==v) for v in [16,32,64]]
    validation_table.append(f"| {n//10000} 万 | "+' | '.join(f"{r['qps']:.2f} / {r['rss_mib']:.2f} MiB" for r in vals)+f" | {vals[0]['recall_at_10']:.2%} |")
ms_table=['| 向量数量 | zvec 缓存开启 QPS | 微软 QPS | zvec RSS（MiB） | 微软 RSS（MiB） | zvec / 微软 Recall@10 |','|---|---:|---:|---:|---:|---:|']
ms_ranges=['| 向量数量 | 微软 QPS 范围 | 微软 RSS 范围（MiB） |','|---|---:|---:|']
for n in config['counts']:
    z,m=row(n,'optimized'),row(n,'microsoft')
    ms_ranges.append(f"| {n//10000} 万 | {m['qps_min']:.2f}–{m['qps_max']:.2f} | {m['rss_mib_min']:.2f}–{m['rss_mib_max']:.2f} |")
    ms_table.append(f"| {n//10000} 万 | {z['qps']:.2f} | {m['qps']:.2f} | {z['rss_mib']:.2f} | {m['rss_mib']:.2f} | {z['recall_at_10']:.2%} / {m['recall_at_10']:.2%} |")
b,o,m=row(1000000,'baseline'),row(1000000,'optimized'),row(1000000,'microsoft')
ratios=[row(n,'optimized')['qps']/row(n,'baseline')['qps'] for n in config['counts']]
env=json.loads((a.base/'measure/environment.json').read_text())
startup_note = '初次验证时，一份复制并改写库路径后的微软程序被 macOS 完整性检查拒绝启动；改用已验证的原位置程序后重新开始三组验证。[失败启动记录](results/cache-comparison/startup-failed/run.log)单独保留，不纳入成功查询统计。' if (a.base/'startup-failed').exists() else ''
report=f'''# zvec DiskANN 节点页缓存测试报告

测试日期：{env['finished'][:10]}。Apple M3 Pro、36 GiB 内存、macOS。使用本地 zvec 源码，原版 `960ecbc2`，缓存版 `{config['source_commit'][:8]}`。

在同一 Cohere 数据、同一索引和搜索参数下，开启 **{budget} MiB 节点页缓存**后，20 万到 100 万的预热后 QPS 提升到原来的 **{min(ratios):.2f}–{max(ratios):.2f} 倍**。百万组 QPS 从 **{b['qps']:.2f}** 到 **{o['qps']:.2f}**，进程峰值 RSS 从 **{b['rss_mib']:.2f} MiB** 到 **{o['rss_mib']:.2f} MiB**。这是用有限的额外内存减少重复读取；没有以降低召回换取速度。

全部正式测量中，zvec 两版的有序 top-10 结果逐项相同，Recall@10 均超过 90%。本轮是固定查询集预热后的单并发对比，不代表首次查询、新工作集或多并发的性能。

## QPS 与 RSS

每个规模、每版各 6 个新进程，表中取中位数。深蓝为缓存关闭，橙色为缓存开启；两条曲线都是 zvec DiskANN。

{chr(10).join(table)}

![QPS 与查询峰值 RSS](results/cache-comparison/qps-memory.png)

## 与微软方案的同轮对比

微软组是此前的 **微软 Rust DiskANN 0.61.0 + 自定义 SQLite/PQ 适配层**，并非微软官方原生 SSD 存储实现。本轮也重新跑了 6 个进程／规模，使用相同数据、查询、预热和计时流程；下表未复用旧计时数据。

{chr(10).join(ms_table)}

![zvec 与微软方案的 QPS 和 RSS](results/cache-comparison/qps-memory-microsoft.png)

百万组中，微软方案的 QPS 是开启缓存后 zvec 的 **{m['qps']/o['qps']:.2f} 倍**，zvec 仍有差距。双方实际召回率不同，均高于 90%；zvec 图出度为 32，微软为 48，PQ 码本训练方式也不同。微软使用 SQLite 256 KiB 堆页缓存、最多 16 MiB mmap，并可受益于系统文件缓存；其较低的进程 RSS 不代表较低的系统总内存占用。完整实现差异见[此前的方案说明](REPORT_MICROSOFT_CN.md#两套方案的差异)。该微软 PQ 路径只支持本轮静态、无过滤查询，不能据此比较完整产品能力。

## 实际修改

- 为 macOS FileReadStorage 增加按需填充的 4 KiB 节点页缓存。页内含原向量与邻接表，热点页命中后无需再次 `pread`，同一批重复页也会复用首次读取的数据。
- 使用固定四路组相联、second-chance 淘汰，容量包含页数据、条目标记与锁，预算不随向量数量增长；小索引按向量段大小收缩。缓存预算是**每个图文件**的上限，多个集合／字段／分片的预算会累加；不是整个进程的 RSS 上限。
- 缓存锁不覆盖磁盘读取；失败读取不进入缓存，关闭或重开文件时清空缓存。保留文件身份校验、请求顺序、搜索候选数、PQ 和精排计算。
- 未命中仍走原同步读取，并保留 macOS `F_NOCACHE` 与关闭预读设置；没有启用全文件 mmap 或放开系统缓存。已有 BufferPool 路径不重复分配此缓存。

百万组每 1000 次查询，墙钟时间中位数从 **{b['batch_s']:.3f} 秒**降至 **{o['batch_s']:.3f} 秒**，CPU 时间为 **{b['cpu_s']:.3f} / {o['cpu_s']:.3f} 秒**，主动上下文切换为 **{b['voluntary_switches']:.0f} / {o['voluntary_switches']:.0f} 次**。这与缓存减少读取等待的机制一致；本轮未用系统调用追踪计数，不能把 CPU 与墙钟时间的差额全部解释为磁盘耗时。

## 如何选择缓存预算

先在独立验证集对 16／32／64 MiB 各测 2 个进程、覆盖全部 5 个规模；另测 5 个原版验证进程。预先固定的规则是：计算各预算在五个规模的 QPS 几何平均值，选择达到最高值 90% 的最小预算。这里的 90% 是选缓存预算的性能阈值，与召回目标是两个独立条件。选择结果为 **{budget} MiB**，随后冻结配置再测正式测试集。

{chr(10).join(validation_table)}

三个预算的验证 QPS 几何平均值依次为 **{scores[16]:.2f}、{scores[32]:.2f}、{scores[64]:.2f}**。所有预算返回的有序结果相同。参数试验只用于缓存大小选择，未用测试集选择 L 或缓存预算。

## 方法与适用范围

沿用此前 Cohere Wikipedia 样本和针对各规模计算的精确 cosine GT；768 维、FP16 存储与查询舍入、PQ64、top-10、单并发、运行时线程数 1。zvec 图出度 32、构图候选数 100；20／40／60／80 万的 L=96，100 万的 L=128。微软方案的图出度为 48、构图候选数 100、L=128、重排数 128，共享此前训练的 PQ64 码本。索引、PQ 编码与码本均未重建，数据和 GT 哈希重新核对，索引文件大小和修改时间在实验前后保持一致。

每个进程先运行 20 条独立预热查询，再将完整 200 条计分查询预热 2 遍，最后计时 5 遍，共 1000 次查询。QPS=1000/累计计时秒数，包含 FP16 转换、向量查询、结果 ID 提取与一致性检查，不包含索引打开和预热。RSS 为 `/usr/bin/time -l` 的进程峰值驻留内存，包含打开、缓存分配、PQ 加载、预热和查询；不包含建库。内存预算与进程 RSS 不是同一个量。

正式阶段先用三份最终二进制重新验证 15 个进程，再做 90 次测量；6 轮中轮换／反转实现顺序，并交替规模顺序。没有拼接历史计时数据，没有清空系统文件缓存或隔离其他应用，RSS 也不包含进程之外的全部系统文件缓存。zvec 两版沿用同一搜索逻辑，仅改变节点页缓存与读取复用，其有序结果已经逐项核验；实际编译版本、源文件和库摘要保存在配置中。

复用预热查询让缓存受益，实际应用若频繁访问不同节点，提升可能较小。预算有限意味着 RSS 增加可预算，但 PQ、键映射和其他内存仍会随索引规模变化；不能承诺整个进程固定内存，也不能用本测试断言不会影响其他应用。

{chr(10).join(ranges)}

{chr(10).join(ms_ranges)}

## 使用与复现

本次优化在本地 zvec C++ 源码中，默认预算为 32 MiB；本轮按验证结果显式使用 {budget} MiB，不把它当作所有集合的通用最优值。重新编译并链接后，在打开集合前设置 `ZVEC_DISKANN_READ_CACHE_MIB={budget}`；设为 `0` 关闭缓存。值必须是 0–1024 的整数，已有索引不需要重建。已发布的 Rust／Node SDK 二进制尚未替换，不能把本地源码测试当作已发布应用的性能。

[配置与源文件摘要](results/cache-comparison/measure/configuration.json)、[正式逐次记录](results/cache-comparison/measure/runs.json)、[验证预算选择](results/cache-comparison/pilot/selection.json)、[独立核验](results/cache-comparison/audit.json)、[源码补丁](results/cache-comparison/measure/source.patch)、[全部原始结果及 GT](results/cache-comparison/raw-evidence.json.gz)。共核验 140 个有效进程；[26 项 C++ 缓存、读取和端到端测试](results/cache-comparison/cpp-tests.json)通过。{startup_note}

复现须保留此前数据和索引目录，并准备分别链接原版／缓存版库的同一 `native-retest` 程序；构建步骤沿用 [本地源码报告](REPORT_NATIVE_CN.md)和[查询计量说明](REPORT_RETEST_NATIVE_CN.md)。`BASE/baseline`、`BASE/optimized` 各放一份固定绝对链接的 `native-retest` 与对应 `libzvec_c_api.dylib`；微软程序、库、索引和 PQ 路径沿用前次记录的原位置，并核对二进制和库摘要；`BASE/pilot`、`BASE/measure` 必须尚不存在。生成报告还需要源码测试记录 `BASE/cpp-tests.json`；复用本次固定源码时可复制已保存的记录，修改源码后需重新运行测试并记录对应提交：

```bash
python3 rust/benchmarks/diskann/run_cache_comparison.py \\
  --base "$BASE" --data /tmp/zvec-cohere-million/data \\
  --indexes /tmp/zvec-local-cohere-20261104/experiment/indexes \\
  --source "$PWD/../zvec" --stage pilot
python3 rust/benchmarks/diskann/run_cache_comparison.py \\
  --base "$BASE" --data /tmp/zvec-cohere-million/data \\
  --indexes /tmp/zvec-local-cohere-20261104/experiment/indexes \\
  --source "$PWD/../zvec" --stage measure
cp rust/benchmarks/diskann/results/cache-comparison/cpp-tests.json "$BASE/cpp-tests.json"
python3 rust/benchmarks/diskann/publish_cache_comparison.py \\
  "$BASE" /tmp/zvec-cohere-million/data
```
'''
(here/'REPORT_CACHE_CN.md').write_text(report)

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
from matplotlib import font_manager
font='/System/Library/Fonts/STHeiti Light.ttc'
font_manager.fontManager.addfont(font)
plt.rcParams.update({'font.family':font_manager.FontProperties(fname=font).get_name(),'font.size':11,
                     'axes.spines.top':False,'axes.spines.right':False,'axes.unicode_minus':False,'svg.hashsalt':'zvec-cache-comparison'})
fig,axes=plt.subplots(1,2,figsize=(13,6.3))
for ax,metric,title,unit in [(axes[0],'qps','查询吞吐量 · 越高越好','QPS（次/秒）'),(axes[1],'rss_mib','查询峰值 RSS · 越低越好','MiB')]:
    for backend,label,color,marker in [('baseline','缓存关闭','#163B65','s'),('optimized',f'缓存开启（{budget} MiB）','#E87722','o')]:
        values=[row(n,backend)[metric] for n in config['counts']]
        xs=[n/10000 for n in config['counts']]
        ax.plot(xs,values,marker=marker,color=color,label=label,linewidth=2.3,markersize=6)
        for n,x,y in zip(config['counts'],xs,values):
            peer=row(n,'optimized' if backend=='baseline' else 'baseline')[metric]
            offset=-22 if y<peer else 11
            ax.annotate(f'{y:.2f}',(x,y),xytext=(0,offset),textcoords='offset points',ha='center',fontsize=10,color=color)
    ax.set_xticks([20,40,60,80,100],['20 万','40 万','60 万','80 万','100 万'])
    ax.set_xlim(12,108);ax.set_ylim(0,max(r[metric] for r in summary if r['backend']!='microsoft')*1.23)
    ax.set(title=title,xlabel='向量数量（768 维）',ylabel=unit);ax.grid(axis='y',alpha=.18)
fig.suptitle(f'zvec DiskANN：节点页缓存开启前后\nCohere Wikipedia · FP16 · PQ64 · 相同搜索参数 · Recall@10 ≥ 90%',fontsize=15,y=.98)
fig.legend(*axes[0].get_legend_handles_labels(),loc='lower center',bbox_to_anchor=(.5,.10),ncol=2,frameon=False)
fig.text(.5,.070,'完整查询集预热 2 遍，再计时 5 遍；每组 6 个新进程，取中位数。',ha='center',fontsize=10)
fig.text(.5,.036,'两版有序结果一致；缓存预算为每个图文件的上限，进程 RSS 还包含 PQ 等内存。',ha='center',fontsize=10)
fig.text(.5,.006,f"本地 zvec {config['source_base_commit'][:8]} → {config['source_commit'][:8]} · Apple M3 Pro / 36 GiB / macOS",ha='center',fontsize=9,color='#555555')
fig.subplots_adjust(left=.075,right=.985,top=.80,bottom=.27,wspace=.25)
fig.savefig(dest/'qps-memory.png',dpi=200)
fig.savefig(dest/'qps-memory.svg',metadata={'Date':None})
plt.close(fig)
svg=dest/'qps-memory.svg';svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines())+'\n')
fig,axes=plt.subplots(1,2,figsize=(13,6.3))
for ax,metric,title,unit in [(axes[0],'qps','查询吞吐量 · 越高越好','QPS（次/秒）'),(axes[1],'rss_mib','查询峰值 RSS · 越低越好','MiB')]:
    for backend,label,color,marker in [('microsoft','微软 Rust + SQLite','#163B65','s'),('optimized',f'zvec DiskANN（缓存 {budget} MiB）','#E87722','o')]:
        xs=[n/10000 for n in config['counts']];ys=[row(n,backend)[metric] for n in config['counts']]
        ax.plot(xs,ys,marker=marker,color=color,label=label,linewidth=2.3,markersize=6)
        for n,x,y in zip(config['counts'],xs,ys):
            peer=row(n,'optimized' if backend=='microsoft' else 'microsoft')[metric]
            ax.annotate(f'{y:.2f}',(x,y),xytext=(0,-22 if y<peer else 11),textcoords='offset points',ha='center',fontsize=10,color=color)
    ax.set_xticks([20,40,60,80,100],['20 万','40 万','60 万','80 万','100 万'])
    ax.set_xlim(12,108);ax.set_ylim(0,max(r[metric] for r in summary)*1.23)
    ax.set(title=title,xlabel='向量数量（768 维）',ylabel=unit);ax.grid(axis='y',alpha=.18)
fig.suptitle('zvec DiskANN 与微软 Rust + SQLite：同轮复测\nCohere Wikipedia · FP16 · PQ64 · 单并发 · Recall@10 ≥ 90%',fontsize=15,y=.98)
fig.legend(*axes[0].get_legend_handles_labels(),loc='lower center',bbox_to_anchor=(.5,.10),ncol=2,frameon=False)
fig.text(.5,.070,'完整查询集预热 2 遍，再计时 5 遍；每组 6 个新进程，取中位数。',ha='center',fontsize=10)
fig.text(.5,.036,'图参数、码本及缓存策略不同；RSS 不涵盖进程外的全部系统文件缓存。',ha='center',fontsize=10)
fig.text(.5,.006,f"本地 zvec {config['source_commit'][:8]} / 微软 Rust 0.61.0 + SQLite · Apple M3 Pro / 36 GiB / macOS",ha='center',fontsize=9,color='#555555')
fig.subplots_adjust(left=.075,right=.985,top=.80,bottom=.27,wspace=.25)
fig.savefig(dest/'qps-memory-microsoft.png',dpi=200)
fig.savefig(dest/'qps-memory-microsoft.svg',metadata={'Date':None})
plt.close(fig)
svg=dest/'qps-memory-microsoft.svg';svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines())+'\n')
print(json.dumps(audit,ensure_ascii=False))
