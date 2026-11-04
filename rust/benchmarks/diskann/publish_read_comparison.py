"""Independently audit the bounded read-parallelism benchmark and publish the Chinese report."""
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
dest=here/'results/read-comparison'
metrics=['qps','rss_mib','recall_at_10','batch_s','cpu_s','open_s','p50_ms','p95_ms','measured_major_faults','voluntary_switches']
configs={}
summaries={}
all_raw=[]
truth={phase:{str(n):json.loads((folder/f'truth-{n}.json').read_text()) for n in [200000,400000,600000,800000,1000000]}
       for phase,folder in [('validation',a.data/'validation'),('measure',a.data)]}
for folder in [a.data,a.data/'validation']:
    for filename,meta in json.loads((folder/'manifest.json').read_text())['files'].items():
        with (folder/filename).open('rb') as f: assert hashlib.file_digest(f,'sha256').hexdigest()==meta['sha256']
for stage,expected in [('pilot',45),('measure',105)]:
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
        group=[r for r in runs if r['count']==row['count'] and r['backend']==row['backend'] and r['workers']==row['workers'] and r['phase']==('validation' if stage=='pilot' else 'measure')]
        assert len(group)==row['repeats']
        if stage=='measure': assert len(group)==6 and {r['repeat'] for r in group}==set(range(6))
        for metric in metrics:
            values=[r[metric] for r in group]
            assert row[metric]==statistics.median(values)
            assert row[metric+'_min']==min(values) and row[metric+'_max']==max(values)
    assert len(summary)==(25 if stage=='pilot' else 15)
    assert hashlib.sha256((folder/'runner.py').read_bytes()).hexdigest()==config['runner_sha256']
selection=json.loads((a.base/'pilot/selection.json').read_text())
scores={b:math.exp(statistics.mean(math.log(r['qps']) for r in summaries['pilot'] if r['backend']=='optimized' and r['workers']==b)) for b in [1,2,4,8]}
for b,v in scores.items(): assert abs(v-selection['validation_geometric_mean_qps'][str(b)])<1e-8
workers=min(b for b,v in scores.items() if v>=max(scores.values())*.9)
assert workers==selection['workers']
assert json.loads((a.base/'measure/selection.json').read_text())==selection
config=configs['measure']
for filename,digest in config['source_sha256'].items():
    assert hashlib.sha256((Path(config['source'])/filename).read_bytes()).hexdigest()==digest
assert hashlib.sha256((here/'run_read_comparison.py').read_bytes()).hexdigest()==config['runner_sha256']
assert hashlib.sha256((here/'harness/src/bin/native-retest.rs').read_bytes()).hexdigest()==config['harness_sha256']
for name,library in config['libraries'].items():
    assert hashlib.sha256(Path(library['path']).read_bytes()).hexdigest()==library['sha256']
assert json.loads((a.base/'pilot/index-inventory.json').read_text())=={k:v for k,v in json.loads((a.base/'measure/index-inventory.json').read_text()).items() if not k.startswith('microsoft/')}
for name,digest in config['microsoft_source_sha256'].items():
    assert hashlib.sha256((here.parents[2]/name).read_bytes()).hexdigest()==digest
dest.mkdir(parents=True,exist_ok=True)
cpp_tests=json.loads((a.base/'cpp-tests.json').read_text())
assert cpp_tests['total_passed']==33 and cpp_tests['source_commit']==config['source_commit']
shutil.copy2(a.base/'cpp-tests.json',dest/'cpp-tests.json')
shutil.copy2(a.base/'build.json',dest/'build.json')
if (a.base/'startup-failed').exists():
    (dest/'startup-failed').mkdir(exist_ok=True)
    shutil.copy2(a.base/'startup-failed/parallel4-1m.stderr',dest/'startup-failed/parallel4-1m.stderr')
for stage in ['pilot','measure']:
    (dest/stage).mkdir(exist_ok=True)
    for name in ['configuration.json','runs.json','summary.json','selection.json','index-inventory.json','environment.json','source.patch','runner.py']:
        shutil.copy2(a.base/stage/name,dest/stage/name)
payload=json.dumps(dict(runs=all_raw,truth=truth),separators=(',',':')).encode()
(dest/'raw-evidence.json.gz').write_bytes(gzip.compress(payload,mtime=0))
measured=[r for r in json.loads((a.base/'measure/runs.json').read_text()) if r['phase']=='measure']
audit=dict(processes_verified=150,pilot_processes=45,final_validation_processes=15,measured_processes=90,
           ordered_ids_identical_between_zvec_variants=True,minimum_test_recall=min(r['recall_at_10'] for r in measured),
           raw_evidence_uncompressed_sha256=hashlib.sha256(payload).hexdigest(),cache_budget_mib=64,read_workers=workers,
           measured_major_faults_max=max(r['measured_major_faults'] for r in measured))
(dest/'audit.json').write_text(json.dumps(audit,indent=2)+'\n')
summary=summaries['measure']
def row(n,backend): return next(r for r in summary if r['count']==n and r['backend']==backend)

# Audit the opt-in diagnostic runs separately from formal QPS timings.
profile_config=json.loads((a.base/'profile/configuration.json').read_text())
profiles={}
profile_raw={}
profile_dest=dest/'profile';profile_dest.mkdir(exist_ok=True)
for name in ['serial-1m','parallel4-1m','parallel-selected-1m']:
    raw=[json.loads(line) for line in (a.base/'profile'/f'{name}.jsonl').read_text().splitlines()]
    timing=next(r for r in raw if r['stage']=='queried')
    ids=next(r['ids'] for r in raw if r['stage']=='results')
    assert ids==ids_by_group[('validation',1000000,'zvec')]
    stderr=(a.base/'profile'/f'{name}.stderr').read_text()
    records=re.findall(r'DISKANN_READ_PROFILE (.*)',stderr)
    assert len(records)==1
    counter=json.loads(records[0])
    assert counter['requested_bytes']==4096*counter['requests']
    assert counter['read_bytes']==4096*counter['miss_reads']
    assert counter['miss_reads']==counter['pread_syscalls']
    profiles[name]=dict(**counter,qps=timing['qps'],batch_s=timing['batch_s'])
    profile_raw[name]=dict(jsonl=raw,stderr=stderr)
    for ext in ['jsonl','stderr']:
        shutil.copy2(a.base/'profile'/f'{name}.{ext}',profile_dest/f'{name}.{ext}')
assert profiles['serial-1m']['requests']==profiles['parallel-selected-1m']['requests']
for name in ['configuration.json','source.patch']:
    shutil.copy2(a.base/'profile'/name,profile_dest/name)
(profile_dest/'summary.json').write_text(json.dumps(profiles,indent=2)+'\n')
audit.update(diagnostic_processes_verified=3,diagnostic_ids_identical=True)
(dest/'audit.json').write_text(json.dumps(audit,indent=2)+'\n')

main_table=['| 向量数量 | zvec 串行 QPS | zvec 并行 QPS | 提升 | 微软 QPS | zvec / 微软 Recall@10 |',
            '|---|---:|---:|---:|---:|---:|']
mem_table=['| 向量数量 | zvec 串行 RSS（MiB） | zvec 并行 RSS（MiB） | 微软 RSS（MiB） |',
           '|---|---:|---:|---:|']
ranges=['| 向量数量 | 串行 QPS 范围 | 并行 QPS 范围 | 微软 QPS 范围 |',
        '|---|---:|---:|---:|']
mem_ranges=['| 向量数量 | 串行 RSS 范围（MiB） | 并行 RSS 范围（MiB） | 微软 RSS 范围（MiB） |',
            '|---|---:|---:|---:|']
validation=['| 向量数量 | 1 线程 QPS | 2 线程 QPS | 4 线程 QPS | 8 线程 QPS | 验证 Recall@10 |',
            '|---|---:|---:|---:|---:|---:|']
for n in config['counts']:
    b,o,m=[row(n,k) for k in ['baseline','optimized','microsoft']]
    main_table.append(f"| {n//10000} 万 | {b['qps']:.2f} | {o['qps']:.2f} | {o['qps']/b['qps']:.2f}× | {m['qps']:.2f} | {o['recall_at_10']:.2%} / {m['recall_at_10']:.2%} |")
    mem_table.append(f"| {n//10000} 万 | {b['rss_mib']:.2f} | {o['rss_mib']:.2f} | {m['rss_mib']:.2f} |")
    ranges.append(f"| {n//10000} 万 | "+' | '.join(f"{r['qps_min']:.2f}–{r['qps_max']:.2f}" for r in [b,o,m])+' |')
    mem_ranges.append(f"| {n//10000} 万 | "+' | '.join(f"{r['rss_mib_min']:.2f}–{r['rss_mib_max']:.2f}" for r in [b,o,m])+' |')
    vals=[next(r for r in summaries['pilot'] if r['backend']=='optimized' and r['count']==n and r['workers']==w) for w in [1,2,4,8]]
    validation.append(f"| {n//10000} 万 | "+' | '.join(f"{r['qps']:.2f}" for r in vals)+f" | {vals[0]['recall_at_10']:.2%} |")
b,o,m=[row(1000000,k) for k in ['baseline','optimized','microsoft']]
ratios=[row(n,'optimized')['qps']/row(n,'baseline')['qps'] for n in config['counts']]
ps,pp=profiles['serial-1m'],profiles['parallel-selected-1m']
env=json.loads((a.base/'measure/environment.json').read_text())
report=f'''# zvec DiskANN 读取瓶颈与 QPS/RSS 对比

测试日期：{env['finished'][:10]}。Apple M3 Pro、36 GiB 内存、macOS。本地 zvec 源码从 `e31ce799` 到 `{config['source_commit'][:8]}`，两版均使用 **64 MiB 节点页缓存**。

上一版缓存已经减少了重复读取，但 macOS 上剩余的节点页仍逐个同步读取。这轮加入有上限的读取线程池，在不改变索引、PQ、L 和结果顺序的情况下，20 万至 100 万向量的 QPS 提升 **{min(ratios):.2f}–{max(ratios):.2f} 倍**。百万组从 **{b['qps']:.2f}** 提升到 **{o['qps']:.2f} QPS**，RSS 从 **{b['rss_mib']:.2f}** 到 **{o['rss_mib']:.2f} MiB**，变化 **{o['rss_mib']-b['rss_mib']:+.2f} MiB**。

同轮微软方案为 **{m['qps']:.2f} QPS**，与 zvec 的差距由 **{m['qps']/b['qps']:.2f} 倍**缩小到 **{m['qps']/o['qps']:.2f} 倍**。两版 zvec 的有序 top-10 逐项一致，测试集 Recall@10 为 **{min(row(n,'optimized')['recall_at_10'] for n in config['counts']):.2%}–{max(row(n,'optimized')['recall_at_10'] for n in config['counts']):.2%}**，没有降低召回来换速度。

## 同轮结果

三组在每个规模各跑 6 个新进程，以下均取中位数。串行组是上一轮已经开启缓存的 zvec；并行组为本轮 zvec，使用验证集选出的 **{workers} 个 I/O 工作线程**。查询并发数仍为 1，但工作线程资源不同，因此不是同线程数的效率对比。

{chr(10).join(main_table)}

{chr(10).join(mem_table)}

![zvec 串行与并行读取的 QPS 和 RSS](results/read-comparison/qps-memory.png)

![zvec 与微软方案的 QPS 和 RSS](results/read-comparison/qps-memory-microsoft.png)

## 定位到的瓶颈

单独在百万向量的独立验证集打开读取诊断。该统计覆盖打开后的读取、20 条独立预热及 200 条查询的 7 遍执行，不能与正式计时的 1000 次查询混算；诊断开销也不纳入正式 QPS 表。

| 指标 | 串行读取 | {workers} 线程读取 |
|---|---:|---:|
| 逻辑节点页请求数 | {ps['requests']:,} | {pp['requests']:,} |
| 实际 pread 调用数 | {ps['pread_syscalls']:,} | {pp['pread_syscalls']:,} |
| pread 成功读取字节（MiB） | {ps['read_bytes']/2**20:.2f} | {pp['read_bytes']/2**20:.2f} |
| 各线程读取函数累计耗时（秒） | {ps['read_ns']/1e9:.3f} | {pp['read_ns']/1e9:.3f} |
| 读取批次累计墙钟时间（秒） | {ps['batch_ns']/1e9:.3f} | {pp['batch_ns']/1e9:.3f} |

串行组中，**{ps['miss_reads']/ps['requests']:.1%}** 的逻辑页请求仍进入 `pread`；读取函数耗时占批次墙钟时间的 **{ps['read_ns']/ps['batch_ns']:.1%}**，每次 `pread` 平均约 **{ps['read_ns']/ps['pread_syscalls']/1000:.1f} 微秒**。并行后逻辑请求数不变，实际读取量接近，但批次等待缩短到 **{pp['batch_ns']/ps['batch_ns']:.1%}**。这直接支持本轮主要收益来自重叠读取等待，而不是更少的图遍历或更低的精度。并发导致缓存淘汰时序不同，实际未命中数允许略有差异。

这些计时包含系统调用、等待和线程调度，不是纯 SSD 延迟；读取字节是 API 返回的数据量，也不等于物理介质实际读取量。并行的读取累计耗时是多线程相加，可能大于墙钟时间。

正式百万组每 1000 次查询的墙钟时间为 **{b['batch_s']:.3f} → {o['batch_s']:.3f} 秒**，CPU 时间为 **{b['cpu_s']:.3f} → {o['cpu_s']:.3f} 秒**；微软分别为 **{m['batch_s']:.3f} / {m['cpu_s']:.3f} 秒**。读取并行需要额外调度与同步，CPU 成本和 QPS 应一起看。

## 为什么与微软仍有差距

1. **缓存策略不同。** zvec 保留 `F_NOCACHE` 和关闭预读，64 MiB 用户态节点缓存未命中后仍需读取；微软 SQLite 适配层使用普通文件读取，可受益于系统文件缓存。其 SQLite 堆页缓存为 256 KiB、mmap 上限为 16 MiB，这些上限不限制整个系统文件缓存。固定查询反复预热会放大缓存带来的收益，微软较低的 RSS 不能解释为系统总内存也更低。
2. **搜索和数据布局不同。** zvec 读取的页包含 FP16 向量与邻接表，并为展开节点计算完整向量距离；微软实验路径先做 PQ 遍历，只对最后 128 个候选做 FP16 重排。两者的图出度为 32 / 48，PQ 码本也不同。这些差异可能影响剩余差距，但本轮没有逐项消融，不能为每项分配精确贡献。
3. **本轮没有消除读取依赖。** 同一批可以并行，但下一批仍依赖本批的图搜索结果；有限的并行度不能让磁盘等待完全消失。

这里的微软方案是 **微软 Rust DiskANN 0.61.0 + 自定义 SQLite/PQ 适配层**，不是微软官方原生 SSD 存储引擎。该 PQ 路径仅支持静态、只读、无过滤的本轮测试；不宜把本表解释为完整产品能力或算法本身的公平排名。两边都超过 90%，但实际召回率不同。

## 本轮代码修改

- 每个 macOS FileReadStorage 图文件复用固定工作线程，默认 4 个，可配置 1–8；同一 reader 的多个查询共享线程池。线程栈与调度开销不属于 64 MiB 节点缓存预算。
- 批次轮转分配任务，不为每个节点创建线程或分配任务对象。读取完成后仍按原提交顺序处理节点，保持原搜索行为。
- 批次出错后先等待所有任务结束，再返回错误，避免释放缓冲区后仍有后台写入。重叠目标缓冲区保持串行，关闭文件时先回收线程。
- 增加可关闭的读取诊断。正式测试关闭诊断；没有扩大缓存、开启全文件 mmap、调整 L 或重建索引。已有 BufferPool、Linux、Windows、iOS 默认路径不自动启用此线程池。

## 参数选择与测试口径

固定缓存 64 MiB，在独立验证集对 1／2／4／8 个工作线程各测 2 个新进程、覆盖五个规模，并另测 5 个上一版进程。预先固定规则：选择五规模 QPS 几何平均达到最高值 90% 的最少线程数。该性能阈值与 Recall@10 ≥ 90% 是两个独立条件。

{chr(10).join(validation)}

1／2／4／8 线程的几何平均 QPS 分别为 **{scores[1]:.2f}／{scores[2]:.2f}／{scores[4]:.2f}／{scores[8]:.2f}**，选择 **{workers} 线程**。所有线程配置的有序结果一致；随后冻结配置，未用正式测试集选参数。

沿用之前的 Wikipedia-Cohere / Big ANN 样本：20／40／60／80／100 万，768 维、cosine、FP16、PQ64、top-10。精确 GT 针对各样本规模计算，沿用原 FP32 cosine GT；本轮重新核验数据、GT 和 PQ 文件摘要。zvec 的 L 为 96／96／96／96／128，最大出度 32、构图候选数 100；微软 L=128、重排 128、最大出度 48、构图候选数 100。zvec 各规模独立训练 PQ，微软共享此前训练的 PQ64 码本。索引文件大小和修改时间前后一致。

每个进程先跑 20 条独立预热查询，再把完整 200 条计分查询预热 2 遍，最后计时 5 遍，共 1000 次。QPS 包含 FP16 转换、向量查询、结果提取与一致性检查，不包含打开和预热。RSS 为 `/usr/bin/time -l` 的整个查询进程峰值，包含打开、PQ／缓存加载、预热和查询，不含建库。

正式阶段先重新验证三组全部 15 个配置，再做 90 次测量；6 轮中交替规模顺序，并轮换／反转实现顺序。全部三组均为本轮重新测量，没有拼接历史计时。未清空系统文件缓存，未隔离桌面其他应用；正式测量期间没有构建任务。完整查询集重复预热的结果不代表首次访问、不断变化的工作集或多查询并发。

{chr(10).join(ranges)}

{chr(10).join(mem_ranges)}

## 使用与证据

本地重新编译 zvec 后，在打开集合前设置：

```bash
export ZVEC_DISKANN_READ_CACHE_MIB=64
export ZVEC_DISKANN_READ_WORKERS={workers}
```

源码默认缓存为 32 MiB、读取工作线程为 4；上述值是本轮显式配置。设 `ZVEC_DISKANN_READ_WORKERS=1` 可恢复串行读取。线程和缓存预算按图文件累加，不是整个进程的总资源上限。已发布的 Rust／Node SDK 0.8.0 二进制尚未替换。

[正式记录](results/read-comparison/measure/runs.json)、[配置与源码摘要](results/read-comparison/measure/configuration.json)、[构建与加载路径](results/read-comparison/build.json)、[验证参数选择](results/read-comparison/pilot/selection.json)、[读取诊断](results/read-comparison/profile/summary.json)、[独立核验](results/read-comparison/audit.json)、[源码补丁](results/read-comparison/measure/source.patch)、[全部计时原始记录与 GT](results/read-comparison/raw-evidence.json.gz)。150 个有效基准进程及 3 个额外诊断进程独立核验通过。

[33 项 C++ 测试](results/read-comparison/cpp-tests.json)通过。ThreadSanitizer 在运行时初始化、进入 main 前崩溃，连列出测试也失败，因此未获得 TSan 验证，不能将其记为通过。最初复制并改写链接路径的程序被 macOS 完整性检查拒绝启动；随后从源码正常重新链接程序和库，核对实际加载路径后再开始基准。[失败启动记录](results/read-comparison/startup-failed/parallel4-1m.stderr)不计入有效查询结果。

复现脚本为 [run_read_comparison.py](run_read_comparison.py) 和 [publish_read_comparison.py](publish_read_comparison.py)。保留原数据和索引路径，准备配置中对应的固定二进制与库；新实验目录中的 `pilot`、`measure` 必须不存在。脚本先运行 `--stage pilot` 再运行 `--stage measure`，发布脚本另需 `cpp-tests.json` 与 `profile` 中的诊断记录。具体绝对路径、库摘要、源文件摘要和参数均保存在配置中。
'''
(here/'REPORT_READ_CN.md').write_text(report)

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
from matplotlib import font_manager
font='/System/Library/Fonts/STHeiti Light.ttc'
font_manager.fontManager.addfont(font)
plt.rcParams.update({'font.family':font_manager.FontProperties(fname=font).get_name(),'font.size':11,
                     'axes.spines.top':False,'axes.spines.right':False,'axes.unicode_minus':False,
                     'svg.hashsalt':'zvec-read-comparison'})
for name,series,title,note in [
    ('qps-memory', [('baseline','zvec DiskANN · 串行读取','#163B65','s'),
                    ('optimized',f'zvec DiskANN · {workers} 线程读取','#E87722','o')],
     'zvec DiskANN：串行与并行读取',
     '两版均为 64 MiB 节点页缓存，有序结果一致；RSS 包含整个查询进程。'),
    ('qps-memory-microsoft',[('microsoft','微软 Rust DiskANN + SQLite/PQ','#163B65','s'),
                             ('optimized',f'zvec DiskANN · {workers} 线程读取','#E87722','o')],
     'zvec DiskANN 与微软 Rust + SQLite/PQ',
     '图参数、码本、I/O 线程及缓存策略不同；RSS 不涵盖进程外全部文件缓存。')]:
    fig,axes=plt.subplots(1,2,figsize=(13,6.3))
    for ax,metric,subtitle,unit in [(axes[0],'qps','查询吞吐量 · 越高越好','QPS（次/秒）'),
                                   (axes[1],'rss_mib','查询峰值 RSS · 越低越好','MiB')]:
        for backend,label,color,marker in series:
            xs=[n/10000 for n in config['counts']]
            values=[row(n,backend)[metric] for n in config['counts']]
            ax.plot(xs,values,marker=marker,color=color,label=label,linewidth=2.3,markersize=6)
            peer_backend=next(s[0] for s in series if s[0]!=backend)
            for n,x,y in zip(config['counts'],xs,values):
                peer=row(n,peer_backend)[metric]
                offset=-22 if y<peer or (y==peer and backend==series[0][0]) else 11
                ax.annotate(f'{y:.2f}',(x,y),xytext=(0,offset),textcoords='offset points',
                            ha='center',fontsize=10,color=color)
        ax.set_xticks([20,40,60,80,100],['20 万','40 万','60 万','80 万','100 万'])
        ax.set_xlim(12,108)
        ax.set_ylim(0,max(row(n,s[0])[metric] for n in config['counts'] for s in series)*1.23)
        ax.set(title=subtitle,xlabel='向量数量（768 维）',ylabel=unit)
        ax.grid(axis='y',alpha=.18)
    fig.suptitle(title+'\nCohere Wikipedia · FP16 · PQ64 · 查询并发 1 · Recall@10 ≥ 90%',fontsize=15,y=.98)
    fig.legend(*axes[0].get_legend_handles_labels(),loc='lower center',bbox_to_anchor=(.5,.10),ncol=2,frameon=False)
    fig.text(.5,.070,'完整查询集预热 2 遍，再计时 5 遍；每组 6 个新进程，取中位数。',ha='center',fontsize=10)
    fig.text(.5,.036,note,ha='center',fontsize=10)
    fig.text(.5,.006,f"本地 zvec {config['source_commit'][:8]} · Apple M3 Pro / 36 GiB / macOS",ha='center',fontsize=9,color='#555555')
    fig.subplots_adjust(left=.075,right=.985,top=.80,bottom=.27,wspace=.25)
    fig.savefig(dest/(name+'.png'),dpi=200)
    fig.savefig(dest/(name+'.svg'),metadata={'Date':None})
    plt.close(fig)
    svg=dest/(name+'.svg')
    svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines())+'\n')
print(json.dumps(audit,ensure_ascii=False))
