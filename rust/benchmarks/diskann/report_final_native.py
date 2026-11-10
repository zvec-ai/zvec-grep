"""Audit, archive and render the final-branch DiskANN/HNSW experiment."""
import argparse
import gzip
import hashlib
import json
import re
import shutil
import statistics
from datetime import datetime, timezone
from pathlib import Path

import numpy as np

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib import font_manager

parser = argparse.ArgumentParser()
parser.add_argument("root", type=Path)
parser.add_argument("--font", type=Path, default=Path("/System/Library/Fonts/Supplemental/Songti.ttc"))
args = parser.parse_args()
root = args.root.resolve()
here = Path(__file__).resolve().parent
out = here / "results/final-native"
out.mkdir(exist_ok=True)
read = lambda p: json.loads(p.read_text())
config = read(root/"configuration.json")
rows = read(root/"measure/runs.json")
summary = read(root/"measure/summary.json")
manifest = read(root/"data/manifest.json")
build = read(root/"build/runs.json")
selection = read(root/"selection.json")
counts = [200000,400000,600000,800000,1000000]
assert len(rows) == 70 and len(summary) == 10

def sha(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f,"sha256").hexdigest()

# Independently recompute every recall and all headline aggregates from raw IDs.
keys = set()
raw = {}
for r in rows:
    key = r["backend"],r["count"],r["shard"]
    assert key not in keys
    keys.add(key)
    path = root/r["raw_file"]
    records = [json.loads(x) for x in path.read_text().splitlines() if x.startswith("{")]
    result = next(x["ids"] for x in records if x.get("stage") == "results")
    measured = next(x for x in records if x.get("stage") == "queried")
    truth = read(root/f"data/test-{r['shard']}-truth-{r['count']}.json")
    hits = sum(len(set(x).intersection(y)) for x,y in zip(result,truth,strict=True))
    assert hits == r["recall_hits"] and hits/5000 == r["recall_at_10"]
    assert measured["queries"] == 500 and measured["test_passes"] == 1 and measured["test_warmup_passes"] == 0
    assert measured["batch_s"] == r["batch_s"]
    rss = int(re.search(r"(\d+)\s+maximum resident set size",path.with_suffix(".stderr").read_text()).group(1))/2**20
    assert rss == r["rss_mib"]
    loaded = next(x["path"] for x in records if x.get("stage") == "library")
    assert Path(loaded).resolve() == Path(config["library"]["path"]).resolve()
    assert r["effort"] == selection[f"{r['backend']}-{r['count']}"]["effort"]
    assert abs(r["qps"] - 500/r["batch_s"]) < 1e-9
    raw[str(path.relative_to(root))] = dict(records=records,stderr=path.with_suffix(".stderr").read_text())
for s in summary:
    g = [r for r in rows if r["backend"] == s["backend"] and r["count"] == s["count"]]
    assert len(g) == 7 and sorted(r["shard"] for r in g) == list(range(7))
    assert s["queries"] == 3500 and abs(s["qps"]-3500/sum(r["batch_s"] for r in g)) < 1e-9
    assert s["recall_at_10"] == sum(r["recall_hits"] for r in g)/35000
    assert s["rss_mib"] == statistics.median(r["rss_mib"] for r in g)
    assert s["recall_at_10"] >= .90
ids = [i for group in manifest["query_source_row_ids"].values() for i in group]
assert len(ids) == len(set(ids)) == 4500
for dtype in ["<f4", "<f2"]:
    fingerprints = set()
    for group in manifest["query_source_row_ids"]:
        p = root/f"data/{group}.f32"
        assert sha(p) == manifest["files"][p.name]["sha256"]
        for row in np.fromfile(p,dtype="<f4").reshape(-1,768).astype(dtype):
            key = hashlib.sha256(row.tobytes()).digest()
            assert key not in fingerprints
            fingerprints.add(key)
    assert len(fingerprints) == 4500
for field in ["library","binary","builder"]:
    assert sha(Path(config[field]["path"])) == config[field]["sha256"]
source_build = read(root/"source-build.json")
assert config["source_commit"] == source_build["source_commit"]
assert config["library"]["sha256"] == source_build["library_sha256"]
# Hash the finished index files after all timed work, so the audit itself does
# not prefetch their contents into the filesystem cache before measurement.
index_files = {str(p.relative_to(root/"indexes")):dict(bytes=p.stat().st_size,sha256=sha(p))
               for p in sorted((root/"indexes").rglob("*")) if p.is_file()}
(out/"index-files.json").write_text(json.dumps(index_files,indent=2)+"\n")

for name in ["configuration.json","selection.json","cpp-tests.json","source-build.json"]:
    shutil.copyfile(root/name,out/name)
for phase in ["build","validation","measure"]:
    (out/phase).mkdir(exist_ok=True)
    for name in ["runs.json","summary.json","audit.json"]:
        p = root/phase/name
        if p.exists(): shutil.copyfile(p,out/phase/name)
shutil.copyfile(root/"data/manifest.json",out/"dataset.json")
for phase in ["build","validation"]:
    for p in (root/phase/"raw").glob("*.jsonl"):
        raw[str(p.relative_to(root))] = dict(stdout=p.read_text(),stderr=p.with_suffix(".stderr").read_text())
archive = dict(records=raw,truth={p.name:read(p) for p in (root/"data").glob("*truth-*.json")},
    cpp_logs={p.name:p.read_text() for p in (root/"cpp-tests").glob("*.log")},
    restoration_log=(root/"restore.log").read_text())
with gzip.open(out/"raw-evidence.json.gz","wt") as f:
    json.dump(archive,f,separators=(",",":"))

font_manager.fontManager.addfont(str(args.font))
plt.rcParams.update({"font.family":font_manager.FontProperties(fname=str(args.font)).get_name(),
    "font.size":11,"axes.unicode_minus":False,"axes.spines.top":False,"axes.spines.right":False,
    "svg.hashsalt":"final-native-20261110"})
fig, axes = plt.subplots(1,2,figsize=(13,6.1))
for ax,metric,title,unit in [(axes[0],"qps","查询吞吐量 · 越高越好","QPS（次/秒）"),
                            (axes[1],"rss_mib","查询峰值 RSS · 越低越好","MiB")]:
    maximum = max(r[metric] for r in summary)
    for backend,label,color,marker in [("hnsw","HNSW","#163B65","o"),("diskann","DiskANN","#E87722","s")]:
        g = sorted((r for r in summary if r["backend"] == backend),key=lambda r:r["count"])
        assert [r["count"] for r in g] == counts
        x,y = [r["count"]/10000 for r in g],[r[metric] for r in g]
        ax.plot(x,y,marker=marker,color=color,linewidth=2.3,markersize=6,label=label)
        for px,py in zip(x,y):
            ax.annotate(f"{py:.1f}",(px,py),xytext=(0,11),textcoords="offset points",ha="center",color=color,fontsize=10)
    ax.set_xticks([20,40,60,80,100],["20 万","40 万","60 万","80 万","100 万"])
    ax.set(xlim=(12,108),ylim=(0,maximum*1.2),title=title,ylabel=unit,xlabel="向量数量（768 维）")
    ax.grid(axis="y",alpha=.18)
fig.suptitle("zvec HNSW 与 DiskANN：最终分支版本对比\nCohere Wikipedia · 双 FP16 · 单并发 · Recall@10 ≥ 90%",fontsize=15,y=.98)
fig.legend(*axes[0].get_legend_handles_labels(),loc="lower center",bbox_to_anchor=(.5,.095),ncol=2,frameon=False)
fig.text(.5,.065,"600 条独立查询预热；3,500 条测试查询各执行一次。QPS 为总查询数 / 总耗时；RSS 为 7 个进程峰值的中位数。",ha="center",fontsize=10)
fig.text(.5,.025,f"本地 zvec {config['source_commit'][:8]} · Apple M3 Pro / 36 GiB · macOS；未清空系统文件缓存。",ha="center",fontsize=10,color="#555555")
fig.subplots_adjust(left=.075,right=.985,top=.80,bottom=.26,wspace=.25)
fig.savefig(out/"qps-memory.png",dpi=200)
fig.savefig(out/"qps-memory.svg",metadata={"Date":None})
plt.close(fig)
svg=out/"qps-memory.svg"
svg.write_text("\n".join(line.rstrip() for line in svg.read_text().splitlines())+"\n")

audit = dict(verified_at=datetime.now(timezone.utc).isoformat(),
    source_commit=config["source_commit"], formal_processes=len(rows),
    validation_processes=len(read(root/"validation/runs.json")),
    raw_ids_and_exact_gt_recomputed=True, qps_recomputed_from_total_queries_and_seconds=True,
    rss_reparsed_from_time_l=True, query_groups_disjoint_by_source_fp32_fp16=True,
    unique_query_rows=len(ids), test_queries_per_backend_scale=3500,
    all_formal_shards_above_90=all(r["recall_at_10"] >= .90 for r in rows),
    min_formal_shard_recall=min(r["recall_at_10"] for r in rows),
    indexed_files_hashed_after_timing=len(index_files),
    binary_library_builder_hashes_rechecked=True,
    chart_sha256=sha(out/"qps-memory.png"),
    chart_palette={"HNSW":"#163B65","DiskANN":"#E87722"}, no_error_bars=True)
(out/"report-audit.json").write_text(json.dumps(audit,indent=2)+"\n")

get = lambda n,b: next(r for r in summary if r["count"]==n and r["backend"]==b)
memory_savings=[1-get(n,"diskann")["rss_mib"]/get(n,"hnsw")["rss_mib"] for n in counts]
qps_ratios=[get(n,"diskann")["qps"]/get(n,"hnsw")["qps"] for n in counts]
recalls=[r["recall_at_10"] for r in summary]
table="\n".join(f"| {n//10000} 万 | {get(n,'hnsw')['qps']:.1f} | {get(n,'diskann')['qps']:.1f} | {get(n,'hnsw')['rss_mib']:.1f} | {get(n,'diskann')['rss_mib']:.1f} | {get(n,'hnsw')['recall_at_10']:.2%} | {get(n,'diskann')['recall_at_10']:.2%} |" for n in counts)
parameters="\n".join(f"| {n//10000} 万 | {get(n,'hnsw')['effort']} | {get(n,'diskann')['effort']} |" for n in counts)
build_table="\n".join(f"| {n//10000} 万 | "+" | ".join(f"{next(r for r in build if r['count']==n and r['engine']==b)['rss_mib']:.1f}" for b in ['hnsw','diskann'])+" |" for n in counts)
tests=read(root/"cpp-tests.json")
assert tests["failed"] == 0
min_shard=min(r['recall_at_10'] for r in rows)
report=f'''# zvec DiskANN 与 HNSW 测试报告

测试日期：2026-11-10。本轮使用本地 zvec 分支 `{config['source_branch']}` 的最终提交 `{config['source_commit']}`，两种索引均重新建库并通过同一动态库的 `Collection::query` 测试。

## 结论

在 20 万至 100 万条 Cohere 向量上，DiskANN 的查询峰值 RSS 比 HNSW 低 **{min(memory_savings):.1%}–{max(memory_savings):.1%}**；QPS 为 HNSW 的 **{min(qps_ratios):.2f}–{max(qps_ratios):.2f} 倍**。两种索引各规模的整体 Recall@10 为 **{min(recalls):.2%}–{max(recalls):.2%}**，均超过 90%。表中保留实际召回率，并非宣称两边召回率完全相等。

## 查询结果

| 向量数 | HNSW QPS | DiskANN QPS | HNSW RSS（MiB） | DiskANN RSS（MiB） | HNSW Recall@10 | DiskANN Recall@10 |
|---|---:|---:|---:|---:|---:|---:|
{table}

![QPS 与查询峰值 RSS 曲线](results/final-native/qps-memory.png)

QPS = 每种索引、每个规模的 3,500 次查询 / 7 个测试进程的总查询耗时。RSS = 7 个独立进程生命周期内峰值 RSS 的中位数，包含打开、预热和测试；不是稳态内存，也不是进程内存上限。单个 500 查询分片的最低 Recall@10 为 {min_shard:.2%}。每轮原始值及范围见 [测试记录](results/final-native/measure/runs.json) 和 [汇总](results/final-native/measure/summary.json)。

## 测试方法

- 数据来自 [Wikipedia-Cohere / Big ANN Benchmarks](https://comp21storage.z5.web.core.windows.net/wiki-cohere-35M/)，768 维。恢复此前同一百万样本，完整 FP32 文件 SHA-256 为 `cfdb2041e2bde4d93d5f2f75feb949701298817a51147512895da3c05ce03d2b`；五档规模取其前缀。
- 两边均存储 FP16、使用 Cosine、返回 top-10 文档 ID。GT 按实际抽样子集的 FP32 输入进行穷举 FP64 余弦计算；数据逐字节恢复后复用已校验的 GT，新增第 7 分片在 20/40 万上的 GT 重新穷举，并用独立标量计算核对。
- 600 条预热、400 条验证、3,500 条测试查询互不重叠，已检查源行号及 FP32/FP16 内容。正式测试分为 7 组，每组 500 条；每条测试查询在每种索引、每个规模下只执行一次，不参与该轮预热或调参。这些查询曾用于前轮比较，因此本轮是回归验证。
- 在验证集上按预先固定的递增候选列表，选择首个 Recall@10 ≥92% 的参数，为正式测试的 90% 门槛留出余量；随后冻结参数。测试交替索引顺序和数据规模顺序，不删除慢轮次，也不为了使曲线单调而替换数据。
- HNSW：`M=50`、`ef_construction=500`。DiskANN：构图 `R=64`、`L=200`、PQ64；每个图文件的读取与范数缓存合计 8 MiB、每个标量 IPC 文件页缓存 1 MiB、每个图 reader 24 个 I/O 工作线程。后一组是显式测试配置，不是默认参数。
- 单个并发查询、一个查询计算线程，建库两边均为 8 个工作线程。HNSW 保留当前分支原有 mmap 路径，DiskANN 使用分支中的有界缓存读取。两边均包含 Collection、文档 ID 映射和结果回填成本。
- Apple M3 Pro / 36 GiB / macOS。正式计时期间没有本任务的编译、下载或建库；其他应用正常运行，不清空系统文件缓存，结果不代表全冷启动或多并发吞吐。

| 向量数 | HNSW ef | DiskANN 查询 L |
|---|---:|---:|
{parameters}

## 建库峰值 RSS

| 向量数 | HNSW（MiB） | DiskANN（MiB） |
|---|---:|---:|
{build_table}

每种索引、每个规模新建一次，以 `/usr/bin/time -l` 记录完整建库进程峰值；这是单次观测。查询侧低 RSS 不意味着建库也同样低，建库与查询应分别评估。

## 本轮代码精简

合并节点缓存命中与并发发布检查中的重复查找和 Clock 更新；未命中读取共用同一逻辑，错误数组仅按未命中数量分配。保留缓存准入、淘汰、PQ 兼容与预算回退、线程池和诊断能力。预算、搜索策略、距离计算及文件格式均未改动。

这是小范围代码整理，本轮没有测精简前后差值，不能把 DiskANN 与 HNSW 的性能差异归因于这次精简。相关 C++ 测试 {tests['passed']} 项通过、{tests['skipped']} 项跳过（x86 AVX 专用测试），详见 [测试结果](results/final-native/cpp-tests.json)。两种索引的小规模建库与 Collection 查询烟雾测试均通过。

## 复现与证据

- [构建与源码标识](results/final-native/source-build.json)、[冻结配置](results/final-native/configuration.json)、[调参结果](results/final-native/validation/runs.json)、[数据清单](results/final-native/dataset.json)、[索引文件哈希](results/final-native/index-files.json)。
- [完整原始返回 ID、GT 和日志](results/final-native/raw-evidence.json.gz)、[独立核验](results/final-native/report-audit.json)、[执行审计](results/final-native/measure/audit.json)、[SVG 曲线图](results/final-native/qps-memory.svg)。
- 脚本：`restore_cohere.py` 恢复数据，`native-harness` 链接指定的本地 zvec 库，`run_final_native.py` 分阶段建库／验证／测试，`report_final_native.py` 独立核算并生成本报告。运行目录：`{root}`。
'''
(here/"REPORT_HNSW_FINAL_CN.md").write_text(report)
print(json.dumps(dict(report=str(here/"REPORT_HNSW_FINAL_CN.md"),all_scales_above_90=True,
    min_shard_recall=min_shard,memory_savings=memory_savings,qps_ratios=qps_ratios),indent=2))
