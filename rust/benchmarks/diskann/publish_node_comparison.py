"""Independently audit the packed-node cache benchmark and publish the Chinese report."""
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
dest=here/'results/node-comparison'
metrics=['qps','rss_mib','recall_at_10','batch_s','cpu_s','open_s','p50_ms','p95_ms','measured_major_faults','voluntary_switches']
configs={}
summaries={}
all_raw=[]
truth={phase:{str(n):json.loads((folder/f'truth-{n}.json').read_text()) for n in [200000,400000,600000,800000,1000000]}
       for phase,folder in [('validation',a.data/'validation'),('measure',a.data)]}
for folder in [a.data,a.data/'validation']:
    for filename,meta in json.loads((folder/'manifest.json').read_text())['files'].items():
        with (folder/filename).open('rb') as f: assert hashlib.file_digest(f,'sha256').hexdigest()==meta['sha256']
for stage,expected in [('measure',105)]:
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
        group=[r for r in runs if r['count']==row['count'] and r['backend']==row['backend'] and r['workers']==row['workers'] and r['phase']=='measure']
        assert len(group)==row['repeats']
        if stage=='measure': assert len(group)==6 and {r['repeat'] for r in group}==set(range(6))
        for metric in metrics:
            values=[r[metric] for r in group]
            assert row[metric]==statistics.median(values)
            assert row[metric+'_min']==min(values) and row[metric+'_max']==max(values)
    assert len(summary)==15
    assert hashlib.sha256((folder/'runner.py').read_bytes()).hexdigest()==config['runner_sha256']
config=configs['measure'];summary=summaries['measure'];folder=a.base/'measure'
workers=8
selection=json.loads((folder/'selection.json').read_text())
assert selection['workers']==8 and selection['reused_for_both_zvec_variants']
assert config['cache_budget_mib']==64 and config['cache_max_ways']==16
for f,digest in config['source_sha256'].items():
    assert hashlib.sha256((Path(config['source'])/f).read_bytes()).hexdigest()==digest
for kind in ['binaries','libraries']:
    for meta in config[kind].values():assert hashlib.sha256(Path(meta['path']).read_bytes()).hexdigest()==meta['sha256']
assert hashlib.sha256((here/'run_node_comparison.py').read_bytes()).hexdigest()==config['runner_sha256']
assert hashlib.sha256((here/'harness/src/bin/native-retest.rs').read_bytes()).hexdigest()==config['harness_sha256']
for f,digest in config['microsoft_source_sha256'].items():
    assert hashlib.sha256((here.parents[2]/f).read_bytes()).hexdigest()==digest
cpp=json.loads((a.base/'cpp-tests.json').read_text())
assert cpp['source_commit']==config['source_commit'] and cpp['total_passed']==40
assert sum(r['passed'] for r in cpp['tests'].values())==40

def row(n,backend):return next(r for r in summary if r['count']==n and r['backend']==backend)
comparison=json.loads((folder/'comparison.json').read_text())
assert comparison['all_scales_faster']
for r in comparison['scales']:
    z,m=row(r['count'],'optimized'),row(r['count'],'microsoft')
    assert r['qps_ratio']==z['qps']/m['qps']>1 and r['recall_at_10']==z['recall_at_10']>=.9
formal=[r for r in runs if r['phase']=='measure']
wins={}
for n in config['counts']:
    wins[n]=sum(next(r['qps'] for r in formal if r['count']==n and r['repeat']==rep and r['backend']=='optimized')>
                next(r['qps'] for r in formal if r['count']==n and r['repeat']==rep and r['backend']=='microsoft') for rep in range(6))

faulty=[dict(backend=r['backend'],count=r['count'],repeat=r['repeat'],major_faults=r['measured_major_faults']) for r in formal if r['measured_major_faults']]
zero_fault={}
for n in config['counts']:
    values=[]
    for rep in range(6):
        z=next(r for r in formal if r['count']==n and r['repeat']==rep and r['backend']=='optimized')
        m=next(r for r in formal if r['count']==n and r['repeat']==rep and r['backend']=='microsoft')
        if z['measured_major_faults']==m['measured_major_faults']==0:values.append(z['qps']/m['qps'])
    assert values and min(values)>1
    zero_fault[n]=dict(pairs=len(values),minimum_qps_ratio=min(values))

profiles={}
paths={'page8':here/'results/read-comparison/profile/parallel-selected-1m.jsonl',
       'node4_small':a.base/'profile/node8-1m.jsonl',
       'node4_sector':a.base/'profile/sector-node8-1m.jsonl',
       'node16_sector':a.base/'profile/assoc16-node8-1m.jsonl'}
for name,path in paths.items():
    raw=[json.loads(l) for l in path.read_text().splitlines()]
    ids=next(r['ids'] for r in raw if r['stage']=='results')
    assert ids==ids_by_group[('validation',1000000,'zvec')]
    timing=next(r for r in raw if r['stage']=='queried')
    stderr=path.with_suffix('.stderr').read_text()
    counters=json.loads(re.findall(r'DISKANN_READ_PROFILE (.*)',stderr)[0])
    assert counters['requests']==311339 and counters['pread_syscalls']==counters['miss_reads']
    record_bytes=4096 if name=='page8' else 1708
    transfer_bytes=1708 if name=='node4_small' else 4096
    assert counters['requested_bytes']==counters['requests']*record_bytes
    assert counters['read_bytes']==counters['miss_reads']*transfer_bytes
    profiles[name]=dict(**counters,qps=timing['qps'],record_bytes=record_bytes,transfer_bytes=transfer_bytes)
    all_raw.append(dict(stage='diagnostic',file=name,jsonl=raw,time_stderr=stderr))
for variant in ['unaligned','assoc4']:
    meta=json.loads((a.base/'profile'/f'{variant}-configuration.json').read_text())
    assert hashlib.sha256((a.base/'profile'/f'{variant}-source.patch').read_bytes()).hexdigest()==meta['source_patch_sha256']
    assert hashlib.sha256((a.base/'profile'/f'{variant}-lib.dylib').read_bytes()).hexdigest()==meta['library_sha256']
final_profile=json.loads((a.base/'profile/final-configuration.json').read_text())
assert final_profile['source_commit']==config['source_commit']
assert final_profile['library_sha256']==config['libraries']['optimized']['sha256']
assert final_profile['cache_budget_mib']==64 and final_profile['read_workers']==8
assert final_profile['max_cache_ways']==16 and final_profile['record_bytes']==1708
# Exploratory precheck belongs to the earlier four-way node cache, not the final variant.
precheck=json.loads((a.base/'precheck/runs.json').read_text());assert len(precheck)==6
for r in precheck:
    path=a.base/'precheck'/f"{r['backend']}-{r['repeat']}.jsonl"
    raw=[json.loads(l) for l in path.read_text().splitlines()]
    ids=next(x['ids'] for x in raw if x['stage']=='results');timing=next(x for x in raw if x['stage']=='queried')
    key=('validation',1000000,'zvec' if r['backend']=='zvec' else 'microsoft')
    assert ids==ids_by_group[key]
    recall=sum(len(set(x)&set(y)) for x,y in zip(ids,truth['validation']['1000000']))/2000
    assert recall==r['recall']>=.9 and r['qps']==timing['qps']
    assert abs(1000/timing['batch_s']-r['qps'])<1e-8
    all_raw.append(dict(stage='precheck_assoc4',file=path.name,jsonl=raw,time_stderr=path.with_suffix('.stderr').read_text()))

# Keep source snapshots, diagnostics, raw IDs, exact GT, and failed preflight separate.
dest.mkdir(parents=True,exist_ok=True);(dest/'measure').mkdir(exist_ok=True);(dest/'profile').mkdir(exist_ok=True)
for f in ['configuration.json','runs.json','summary.json','selection.json','index-inventory.json','environment.json','source.patch','runner.py','comparison.json']:
    shutil.copy2(folder/f,dest/'measure'/f)
for f in ['cpp-tests.json','build.json','preflight-failed.log']:
    shutil.copy2(a.base/f,dest/f)
for f in (a.base/'profile').iterdir():
    if f.suffix in ['.json','.jsonl','.stderr','.patch']:
        shutil.copy2(f,dest/'profile'/f.name)
(dest/'profile/summary.json').write_text(json.dumps(profiles,indent=2)+'\n')
shutil.copy2(a.base/'precheck/runs.json',dest/'profile/precheck-assoc4.json')
payload=json.dumps(dict(runs=all_raw,truth=truth),separators=(',',':')).encode()
(dest/'raw-evidence.json.gz').write_bytes(gzip.compress(payload,mtime=0))
audit=dict(primary_processes_verified=105,final_validation_processes=15,measured_processes=90,
           exploratory_precheck_processes=6,diagnostic_records=4,one_diagnostic_reused_from_prior_experiment=True,
           all_scales_median_qps_above_microsoft=True,paired_round_wins=wins,
           ordered_ids_identical_between_zvec_variants=True,
           minimum_test_recall=min(r['recall_at_10'] for r in formal),
           measured_major_faults_max=max(r['measured_major_faults'] for r in formal),
           raw_evidence_uncompressed_sha256=hashlib.sha256(payload).hexdigest(),
           cache_budget_mib=64,read_workers=8,cache_max_ways=16,nonzero_major_fault_runs=faulty,zero_fault_paired_sensitivity=zero_fault)
(dest/'audit.json').write_text(json.dumps(audit,indent=2)+'\n')

main=['| 向量数量 | zvec DiskANN QPS | 微软 QPS | QPS 倍数 | zvec RSS（MiB） | 微软 RSS（MiB） | zvec / 微软 Recall@10 |',
      '|---|---:|---:|---:|---:|---:|---:|']
control=['| 向量数量 | 页缓存 QPS | 节点缓存 QPS | 提升 | 页缓存 RSS（MiB） | 节点缓存 RSS（MiB） |',
         '|---|---:|---:|---:|---:|---:|']
ranges=['| 向量数量 | zvec QPS 范围 | 微软 QPS 范围 | zvec RSS 范围（MiB） | 微软 RSS 范围（MiB） | zvec 配对获胜轮数 |',
        '|---|---:|---:|---:|---:|---:|']
for n in config['counts']:
    b,z,m=[row(n,k) for k in ['baseline','optimized','microsoft']]
    main.append(f"| {n//10000} 万 | {z['qps']:.2f} | {m['qps']:.2f} | {z['qps']/m['qps']:.2f}× | {z['rss_mib']:.2f} | {m['rss_mib']:.2f} | {z['recall_at_10']:.2%} / {m['recall_at_10']:.2%} |")
    control.append(f"| {n//10000} 万 | {b['qps']:.2f} | {z['qps']:.2f} | {z['qps']/b['qps']:.2f}× | {b['rss_mib']:.2f} | {z['rss_mib']:.2f} |")
    ranges.append(f"| {n//10000} 万 | {z['qps_min']:.2f}–{z['qps_max']:.2f} | {m['qps_min']:.2f}–{m['qps_max']:.2f} | {z['rss_mib_min']:.2f}–{z['rss_mib_max']:.2f} | {m['rss_mib_min']:.2f}–{m['rss_mib_max']:.2f} | {wins[n]} / 6 |")
diag=['| 百万验证组的读取方式 | pread 次数 | API 读取量（MiB） | 批次累计等待（秒） |',
      '|---|---:|---:|---:|']
for key,label in [('page8','4 KiB 页缓存，4 路'),('node4_small','节点缓存，4 路，小尺寸读盘'),
                  ('node4_sector','节点缓存，4 路，整页读盘'),('node16_sector','节点缓存，16 路，整页读盘')]:
    r=profiles[key]
    diag.append(f"| {label} | {r['pread_syscalls']:,} | {r['read_bytes']/2**20:.2f} | {r['batch_ns']/1e9:.3f} |")
b,z,m=[row(1000000,k) for k in ['baseline','optimized','microsoft']]
ratios=[row(n,'optimized')['qps']/row(n,'microsoft')['qps'] for n in config['counts']]
pg,small,four,final=[profiles[k] for k in ['page8','node4_small','node4_sector','node16_sector']]
env=json.loads((folder/'environment.json').read_text())
report=f'''# Cohere：zvec DiskANN 与微软方案测试报告

测试日期：{env['finished'][:10]}。Apple M3 Pro、36 GiB 内存、macOS；使用本地 zvec 源码 `{config['source_commit'][:8]}`。

在同一 Cohere 20 万至 100 万样本、相同预热与计时流程下，**五个规模的 zvec DiskANN QPS 中位数全部超过微软 Rust/SQLite/PQ 方案，为其 {min(ratios):.2f}–{max(ratios):.2f} 倍**。百万组为 **{z['qps']:.2f} 对 {m['qps']:.2f} QPS**。zvec 的 Recall@10 为 **{min(row(n,'optimized')['recall_at_10'] for n in config['counts']):.2%}–{max(row(n,'optimized')['recall_at_10'] for n in config['counts']):.2%}**，有序 top-10 与上一版逐项相同。

缓存预算仍为每个图文件 **64 MiB**，I/O 工作线程仍为 **8**。百万组 zvec RSS 为 **{z['rss_mib']:.2f} MiB**，微软为 **{m['rss_mib']:.2f} MiB**；QPS 领先并不表示 zvec 的进程 RSS 也更低。本结论适用于本机、单查询并发、固定查询集预热后的这套负载。

## QPS 与 RSS

每个规模、每种实现各跑 6 个新进程，表中取中位数。全部数据来自本轮，未拼接旧计时结果。

{chr(10).join(main)}

![QPS 与查询峰值 RSS](results/node-comparison/qps-memory-microsoft.png)

## 具体原因

**先解决串行等待。** 之前 macOS 的缓存未命中逐个执行 `pread`，大量时间花在读取等待。[上一轮](REPORT_READ_CN.md) 用有上限的线程池并行读取，将百万组从约 81 提升到 337 QPS，但仍落后于同轮微软约 475 QPS。本轮在这一版上继续改动，两版都固定为 8 个 I/O 工作线程，查询并发仍为 1。

**再提高固定缓存的有效容量。** 原缓存为每次请求保留整个 4096 字节页，页里还可能有未访问的邻近节点和填充。本轮只缓存目标节点的完整记录；百万索引实际记录长 **1708 字节**，来自索引元数据，包含 FP16 原向量和邻接信息。缓存条目的数据区缩小约 **{1-1708/4096:.1%}**；没有改变向量精度、PQ、图结构或距离计算。旧页也可能同时服务多个节点，所以不能简单把字节比例当作所有负载的加速比。

**减少缓存冲突淘汰。** 记录采用紧凑数据区和最多 16 路组相联的 second-chance 策略；小预算会自适应减少路数。16 路是每组可选缓存槽位数，不是 16 个线程。更多槽位减少热点节点因哈希冲突被过早淘汰，仍由同一个字节预算约束。

**磁盘传输与内存保留使用不同粒度。** 直接把磁盘请求缩到节点大小，在本机并没有得到同等幅度的加速。诊断中，小尺寸读盘平均每次约 **{small['read_ns']/small['pread_syscalls']/1000:.1f} 微秒**；保留原扇区范围与整页长度后约 **{four['read_ns']/four['pread_syscalls']/1000:.1f} 微秒**。最终方案未命中时仍按原扇区读盘，内存只留下节点记录；命中在调用线程直接复制，只有多个未命中才调度工作线程。具体内核路径未做跟踪，不能把这项差异全部归因于某一个 macOS 内核机制。

百万向量的独立验证诊断如下，全部固定 64 MiB 与 8 个工作线程，逻辑节点请求均为 **{pg['requests']:,} 次**，返回结果相同：

{chr(10).join(diag)}

最终相对页缓存，真实 `pread` 调用数减少 **{1-final['pread_syscalls']/pg['pread_syscalls']:.1%}**，API 读取量从 **{pg['read_bytes']/2**20:.2f}** 降到 **{final['read_bytes']/2**20:.2f} MiB**，批次等待从 **{pg['batch_ns']/1e9:.3f}** 降到 **{final['batch_ns']/1e9:.3f} 秒**。只看 4 路到 16 路的节点缓存，调用数还减少了 **{1-final['pread_syscalls']/four['pread_syscalls']:.1%}**。读取函数计时包含等待和调度，字节数是 API 返回的数据量，不等于物理 SSD 的纯服务时间或真实介质流量。

这些诊断包含初始化读取、20 条独立预热、完整 200 条查询的 2 遍预热与 5 遍计时；每个候选是一份额外诊断进程，不能与正式 1000 次查询的计时范围混算。页缓存诊断来自上一轮相同验证集；其余三份保留了对应源码补丁和库摘要。正式 QPS 表关闭诊断，并另做六轮测量。

## 同轮验证改动收益

以下两组都是本地 zvec，复用同一索引、FP16、PQ64、L、64 MiB 预算与 8 个读取工作线程。页缓存版本为 `37ff6bd5`，节点缓存版本为 `{config['source_commit'][:8]}`；搜索结果没有变化。

{chr(10).join(control)}

![zvec 节点缓存改动前后的 QPS 和 RSS](results/node-comparison/qps-memory.png)

正式百万组每 1000 次查询，zvec 墙钟时间为 **{b['batch_s']:.3f} → {z['batch_s']:.3f} 秒**，CPU 时间为 **{b['cpu_s']:.3f} → {z['cpu_s']:.3f} 秒**；微软分别为 **{m['batch_s']:.3f} / {m['cpu_s']:.3f} 秒**。RSS 变化为 **{z['rss_mib']-b['rss_mib']:+.2f} MiB**。缓存上限包含数据、条目和锁，不包括 PQ、键映射、线程栈或其他内存；多个图文件会累加，不能承诺进程总 RSS 固定。

百万组与较小规模的 QPS 差距仍比较大：百万组 L=128，其他组 L=96，访问的节点工作集不同；固定预算下的缓存命中也可能出现明显变化。因此这条曲线不是仅由向量总数决定的复杂度曲线。

## 对比口径与限制

- 沿用之前的 Wikipedia-Cohere / Big ANN 样本，20／40／60／80／100 万、768 维、cosine、FP16、PQ64、top-10。GT 对各样本规模精确计算，沿用原 FP32 cosine GT。本轮重新核验数据、GT、PQ 文件摘要；索引未重建，文件大小和修改时间前后一致。
- zvec 的构图出度配置为 32、构图候选数 100，各规模独立训练 PQ；L=96／96／96／96／128。微软图出度配置为 48、构图候选数 100、L=128、FP16 重排 128，使用此前共享的 PQ64 码本。双方都超过 90%，实际召回率不同。
- 微软组为 **微软 Rust DiskANN 0.61.0 + 自定义 SQLite/PQ 适配层**，并非微软官方原生 SSD 存储引擎；实验路径限静态、只读、无过滤查询。不能据此给完整产品或算法做普遍排名。
- zvec 保留 `F_NOCACHE` 和关闭预读；微软使用 SQLite 256 KiB 堆页缓存、最多 16 MiB mmap，并可受益于系统文件缓存。RSS 不包括进程外全部文件缓存；比较 RSS 不能直接推断系统总内存压力。
- 查询并发均为 1；zvec 额外使用 8 个 I/O 工作线程，微软实验查询路径为同步 SQLite 读取。因此这不是同线程数或同缓存策略的效率对比。参数 8 沿用上一轮独立验证选择，本轮未通过正式测试集再调 L、线程数或缓存预算。
- 每进程先跑 20 条独立预热查询，再把完整 200 条计分查询预热 2 遍，计时 5 遍，共 1000 次。QPS 包含 FP16 转换、向量查询、结果提取与一致性检查，不含打开与预热。RSS 是 `/usr/bin/time -l` 的整个查询进程峰值，包含打开、PQ／缓存加载、预热和查询，不含建库。
- 三组最终二进制先做 15 次验证，再做 90 次正式测量；六轮交替规模顺序，并轮换／反转实现顺序。未清空系统文件缓存，也未隔离其他桌面应用。正式测量时没有构建任务。重复固定查询集有利于缓存，不能外推到首次查询、持续变化的工作集、多查询并发或完整 zvec-grep 请求流程。

{chr(10).join(ranges)}

微软百万组的一次计时进程记录到 5 次 major faults，主表保留了这次结果。只比较双方计时阶段均无 major faults 的配对后，五个规模仍全部领先；百万组剩余 **{zero_fault[1000000]['pairs']} 组配对全部胜出**，最小 QPS 比值仍为 **{zero_fault[1000000]['minimum_qps_ratio']:.2f} 倍**。该敏感性检查没有替换主表数据。

## 使用与复现证据

本地重新编译 zvec 后，在打开集合前设置：

```bash
export ZVEC_DISKANN_READ_CACHE_MIB=64
export ZVEC_DISKANN_READ_WORKERS=8
```

源码默认值仍是 32 MiB 与 4 个工作线程，上述为本次显式配置；设工作线程为 1 可使用串行读取。节点记录大小来自索引元数据，已有索引不用重建。**已发布的 Rust／Node SDK 0.8.0 二进制尚未替换。**

[正式逐次记录](results/node-comparison/measure/runs.json)、[配置与源码摘要](results/node-comparison/measure/configuration.json)、[QPS 目标检查](results/node-comparison/measure/comparison.json)、[独立核验](results/node-comparison/audit.json)、[读取诊断](results/node-comparison/profile/summary.json)、[本轮源码补丁](results/node-comparison/measure/source.patch)、[原始结果及 GT](results/node-comparison/raw-evidence.json.gz)、[构建记录](results/node-comparison/build.json)。105 个主对比进程全部核验通过，另保留 6 次较早四路缓存的百万验证预检查和 4 份诊断记录，其中一份诊断沿用上一轮。

[40 项 C++ 测试](results/node-comparison/cpp-tests.json)通过，覆盖缓存边界、并发淘汰、整页与节点范围、错误回收、重开文件及小节点／跨页节点的建库加载查询。ThreadSanitizer 在本机的前次尝试于运行时初始化、进入 main 前崩溃，本轮没有获得 TSan 验证。

复现脚本为 [run_node_comparison.py](run_node_comparison.py) 和 [publish_node_comparison.py](publish_node_comparison.py)。保留原数据、索引和配置对应的二进制与库，在新的实验目录运行 `run_node_comparison.py --base BASE --data DATA --indexes INDEXES --source ZVEC_SOURCE --stage measure`；`BASE/measure` 必须不存在。发布脚本另需构建、C++ 测试及诊断记录。首次执行时路径检查曾误将上一版程序的 `@rpath` 链接当成绝对链接，已在任何查询开始前修正；这次预检查失败不计为有效测量。
'''
(here/'REPORT_NODE_WARM_CN.md').write_text('> 历史重复热查询结果。独立预热、单遍未知查询的最新结果见 [当前报告](REPORT_NODE_CN.md)。\n\n'+report)
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
from matplotlib import font_manager
font='/System/Library/Fonts/STHeiti Light.ttc'
font_manager.fontManager.addfont(font)
plt.rcParams.update({'font.family':font_manager.FontProperties(fname=font).get_name(),'font.size':11,
                     'axes.spines.top':False,'axes.spines.right':False,'axes.unicode_minus':False,
                     'svg.hashsalt':'zvec-node-comparison'})
for name,series,title,note in [
    ('qps-memory', [('baseline','zvec DiskANN · 页缓存','#163B65','s'),
                    ('optimized','zvec DiskANN · 节点缓存','#E87722','o')],
     'zvec DiskANN：页缓存与节点缓存',
     '两版均为 64 MiB 缓存、8 个 I/O 工作线程，有序结果一致。'),
    ('qps-memory-microsoft',[('microsoft','微软 Rust DiskANN + SQLite/PQ','#163B65','s'),
                             ('optimized','zvec DiskANN','#E87722','o')],
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
