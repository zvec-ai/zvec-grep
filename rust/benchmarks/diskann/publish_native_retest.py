"""Audit warm-workload retests and publish the replacement report and evidence."""
import argparse
import gzip
import hashlib
import json
from pathlib import Path
import re
import shutil
import statistics

parser = argparse.ArgumentParser()
parser.add_argument("experiment", type=Path)
parser.add_argument("data", type=Path)
args = parser.parse_args()
here = Path(__file__).resolve().parent
source = args.experiment
destination = here / "results/native-retest"
assert json.loads((source / "status.json").read_text())["stage"] == "complete"
config = json.loads((source / "configuration.json").read_text())
runs = json.loads((source / "runs.json").read_text())
summary = json.loads((source / "summary.json").read_text())
fixed = json.loads((source / "fixed-ef-summary.json").read_text())
old = here / "results/native"
old_config = json.loads((old / "configuration.json").read_text())
assert config["native_library_sha256"] == old_config["native_library_sha256"]
assert config["zvec_source_commit"] == old_config["zvec_source_commit"]
assert config["primary_parameters"] == json.loads((old / "selection.json").read_text())["selected"]
assert len(runs) == 90 and len(summary) == 10 and len(fixed) == 5
assert hashlib.sha256((args.data / "manifest.json").read_bytes()).hexdigest() == config["data_manifest_sha256"]
for name, metadata in json.loads((args.data / "manifest.json").read_text())["files"].items():
    with (args.data / name).open("rb") as stream:
        assert hashlib.file_digest(stream, "sha256").hexdigest() == metadata["sha256"]
truth = {str(n): json.loads((args.data / f"truth-{n}.json").read_text()) for n in config["counts"]}
raw = []
for row in runs:
    path = source / row["raw_file"]
    stages = [json.loads(line) for line in path.read_text().splitlines()]
    result = next(s for s in stages if s["stage"] == "queried")
    ids = next(s["ids"] for s in stages if s["stage"] == "results")
    assert all(row[key] == value for key, value in result.items())
    assert len(ids) == 200 and all(len(set(i)) == len(i) == 10 for i in ids)
    recall = sum(len(set(a) & set(b)) for a, b in zip(ids, truth[str(row["count"])])) / 2000
    assert row["recall_at_10"] == recall and recall >= .9
    phases = result["phases"]
    assert len(phases) == 7 and [p["pass"] for p in phases] == list(range(7))
    assert [p["phase"] for p in phases] == ["warmup"] * 2 + ["measured"] * 5
    assert result["queries"] == 1000 and result["full_warmup_passes"] == 2
    for phase in phases:
        assert abs(phase["qps"] - 200 / phase["wall_s"]) < 1e-8
        assert all(v >= 0 for v in phase["resources"].values())
    assert abs(result["batch_s"] - sum(p["wall_s"] for p in phases[2:])) < 1e-8
    assert abs(result["qps"] - 1000 / result["batch_s"]) < 1e-8
    assert row["measured_major_faults"] == sum(p["resources"]["major_faults"] for p in phases[2:])
    assert row["first_pass_major_faults"] == phases[0]["resources"]["major_faults"]
    assert row["first_pass_qps"] == phases[0]["qps"]
    stderr = path.with_suffix(".stderr").read_text()
    assert row["rss_mib"] == int(re.search(r"(\d+)\s+maximum resident set size", stderr).group(1)) / 2**20
    chosen = next(r for r in config["primary_parameters"] if r["count"] == row["count"] and r["engine"] == row["engine"])
    assert row["effort"] == (chosen["effort"] if row["series"] == "primary" else config["fixed_hnsw_ef"])
    raw.append(dict(file=row["raw_file"], stages=stages, time_stderr=stderr))
for series, rows in [("primary", summary), ("fixed-ef", fixed)]:
    for row in rows:
        group = [r for r in runs if r["series"] == series and r["engine"] == row["engine"] and r["count"] == row["count"]]
        assert len(group) == 6 and {r["repeat"] for r in group} == set(range(6))
        for metric in ["qps", "rss_mib", "recall_at_10", "mean_ms", "p50_ms", "p95_ms",
                       "measured_major_faults", "first_pass_qps", "first_pass_major_faults"]:
            assert row[metric] == statistics.median(r[metric] for r in group)
            assert row[metric + "_min"] == min(r[metric] for r in group)
            assert row[metric + "_max"] == max(r[metric] for r in group)
destination.mkdir(parents=True, exist_ok=True)
for name in ["configuration.json", "runs.json", "summary.json", "fixed-ef-summary.json", "environment.json", "index-inventory.json"]:
    shutil.copy2(source / name, destination / name)
payload = json.dumps(dict(runs=raw, truth=truth), separators=(",", ":")).encode()
(destination / "raw-evidence.json.gz").write_bytes(gzip.compress(payload, mtime=0))
audit = dict(all_90_processes_verified=True, primary_processes=60, fixed_ef_processes=30,
    raw_evidence_uncompressed_sha256=hashlib.sha256(payload).hexdigest(),
    actual_recall_min=min(r["recall_at_10"] for r in runs),
    measured_major_faults_max=max(r["measured_major_faults"] for r in runs),
    first_pass_major_faults_max=max(r["first_pass_major_faults"] for r in runs))
(destination / "audit.json").write_text(json.dumps(audit, indent=2) + "\n")

table = ["| 向量数量 | HNSW QPS | DiskANN QPS | HNSW RSS（MiB） | DiskANN RSS（MiB） | HNSW / DiskANN Recall@10 |",
         "|---|---:|---:|---:|---:|---:|"]
ranges = ["| 向量数量 | HNSW QPS 范围 | DiskANN QPS 范围 | HNSW RSS 范围（MiB） | DiskANN RSS 范围（MiB） |",
          "|---|---:|---:|---:|---:|"]
parameters = ["| 向量数量 | HNSW ef | DiskANN L |", "|---|---:|---:|"]
for n in config["counts"]:
    h, d = [next(r for r in summary if r["count"] == n and r["engine"] == e) for e in ["hnsw", "diskann"]]
    table.append(f"| {n//10000} 万 | {h['qps']:.2f} | {d['qps']:.2f} | {h['rss_mib']:.2f} | {d['rss_mib']:.2f} | {h['recall_at_10']:.2%} / {d['recall_at_10']:.2%} |")
    ranges.append(f"| {n//10000} 万 | {h['qps_min']:.2f}–{h['qps_max']:.2f} | {d['qps_min']:.2f}–{d['qps_max']:.2f} | {h['rss_mib_min']:.2f}–{h['rss_mib_max']:.2f} | {d['rss_mib_min']:.2f}–{d['rss_mib_max']:.2f} |")
    parameters.append(f"| {n//10000} 万 | {h['effort']} | {d['effort']} |")
h1, d1 = [next(r for r in summary if r["count"] == 1000000 and r["engine"] == e) for e in ["hnsw", "diskann"]]
fixed_table = ["| 向量数量 | 固定 ef=48 QPS | QPS 范围 | Recall@10 |", "|---|---:|---:|---:|"]
for r in fixed:
    fixed_table.append(f"| {r['count']//10000} 万 | {r['qps']:.2f} | {r['qps_min']:.2f}–{r['qps_max']:.2f} | {r['recall_at_10']:.2%} |")
fixed_values = " → ".join(f"{r['qps']:.2f}" for r in fixed)
fixed_desc = "本轮中位数随规模下降" if all(a['qps'] >= b['qps'] for a, b in zip(fixed, fixed[1:])) else "本轮未呈严格单调下降，应结合波动范围判断"
f8, f10 = [next(r for r in fixed if r["count"] == n) for n in [800000, 1000000]]
old_runs = json.loads((old / "runs.json").read_text())
old_raw = json.loads(gzip.decompress((old / "raw-evidence.json.gz").read_bytes()))
million = sorted([r for r in old_runs if r["engine"] == "hnsw" and r["count"] == 1000000], key=lambda r: r["qps"])
old_table = ["| 初次测量轮次 | 百万 HNSW QPS | 进程全程缺页次数 |", "|---|---:|---:|"]
for r in sorted(million, key=lambda r: r["repeat"]):
    stderr = next(e["time_stderr"] for e in old_raw["runs"] if e["file"] == r["raw_file"])
    faults = int(re.search(r"(\d+)\s+page faults", stderr).group(1))
    old_table.append(f"| {r['repeat']+1} | {r['qps']:.2f} | {faults} |")
phases_table = ["| 复测轮次 | 百万 HNSW 首遍 QPS | 首遍缺页次数 | 预热后 QPS | 正式计时缺页次数 |", "|---|---:|---:|---:|---:|"]
for r in runs:
    if r["series"] == "primary" and r["engine"] == "hnsw" and r["count"] == 1000000:
        phases_table.append(f"| {r['repeat']+1} | {r['first_pass_qps']:.2f} | {r['first_pass_major_faults']} | {r['qps']:.2f} | {r['measured_major_faults']} |")
env = json.loads((source / "environment.json").read_text())
brief = f"""# zvec DiskANN 与 HNSW：Cohere 复测报告

测试日期：{env['finished'][:10]}。使用本地 zvec 源码 `{config['zvec_source_commit'][:8]}` 编译的原生库；双 FP16、768 维、cosine、top-10、单并发，DiskANN 启用 PQ64。沿用相同 Cohere 样本、GT、索引和验证集选出的参数，覆盖 20／40／60／80／100 万，测试集 **Recall@10 均 ≥ 90%**。

这次先用完整的 200 条测试查询预热 2 遍，再计时 5 遍，共 1000 次查询。两种引擎均按此流程重新测量，每组 6 个独立进程，取中位数。

{chr(10).join(table)}

![QPS 和查询峰值 RSS 曲线](results/brief/qps-memory.png)

## 为什么上次百万 HNSW 的 QPS 反而更高

上次只用另外 20 条查询预热，就将测试集第一次完整遍历计入 QPS，混入了缺页开销不同的运行状态。百万组 6 轮恰好是 3 次慢、3 次快：中位数 1205.41 来自中间的 **{million[2]['qps']:.2f} 和 {million[3]['qps']:.2f}** 的平均，不是某次典型运行；不同规模的快慢轮次比例不同，使曲线出现回升。

本次把完整查询集预热与正式计时分开，并逐遍记录耗时和缺页。百万 HNSW 预热后为 **{h1['qps']:.2f} QPS**，6 次范围 **{h1['qps_min']:.2f}–{h1['qps_max']:.2f}**。初次结果及逐遍证据见[详细记录](REPORT_RETEST_NATIVE_CN.md)。

各规模的搜索参数仍与上轮相同，并非固定值：HNSW ef 为 40／48／40／40／48，所以主曲线也不能当作仅改变数据量的实验。另做固定 **ef=48** 对照，20 万至 100 万的 QPS 为 **{fixed_values}**，{fixed_desc}；没有删除或调整任何测量点。

100 万时，DiskANN 查询峰值 RSS 为 **{d1['rss_mib']:.2f} MiB**，HNSW 为 **{h1['rss_mib']:.2f} MiB**，DiskANN 低 **{1-d1['rss_mib']/h1['rss_mib']:.2%}**；本轮 DiskANN 的 QPS 低于 HNSW。

RSS 为 macOS `/usr/bin/time -l` 的进程峰值驻留内存，含打开索引、预热和查询，不含建库。QPS 仅计预热后的查询阶段。机器为 {env['cpu']}、{env['memory_bytes']/2**30:g} GiB 内存、macOS，未隔离其他应用，也未清空系统文件缓存。这是固定查询工作集的预热后吞吐量，不代表首次访问、随机新查询或高并发性能。

完整参数、范围、原始数据及复现步骤见[复测详细记录](REPORT_RETEST_NATIVE_CN.md)。
"""
(here / "REPORT_BRIEF_CN.md").write_text(brief)
detail = f"""# HNSW QPS 回升排查与 zvec 复测记录

本轮完成于 {env['finished']}。[最新对比表与曲线](REPORT_BRIEF_CN.md)。

## 初次测试的问题

首次每进程仅用 20 条独立查询预热；随后把测试集 200 条查询连续运行 5 遍，首遍也计入 QPS。新进程不代表同样的系统缓存状态，交替规模会改变工作集。原始百万组如下；缺页次数来自原 `/usr/bin/time -l`，范围为整个进程。

{chr(10).join(old_table)}

慢轮与快轮的 CPU 计算时间接近，缺页和墙钟耗时差异显著。百万组中位数落在两种状态之间，而 60 万组有 4 次慢轮、2 次快轮，跨规模直接连接这些中位数产生了回升。这不能证明规模越大算法越快。原始数据、图及完整旧方法保留在[初次测试记录](REPORT_NATIVE_CN.md)。

## 复测控制

- 使用完全相同的本地 C++ 库（源码 `{config['zvec_source_commit']}`，库 SHA-256 `{config['native_library_sha256']}`），动态链接固定为本地库绝对路径。
- 复用原来的 10 个只读索引，未重建、未修改图或 PQ。实验前后核对全部索引文件大小和修改时间一致；输入向量、查询及各规模 GT 的哈希重新核验。GT 是首次实验穷举生成并复核的原始 FP32 cosine top-10。
- 主对比保持上次冻结的各规模参数，不根据本轮测试结果调参。每进程先运行 20 条独立预热查询，再完整运行同一组 200 条测试查询 2 遍，最后计时 5 遍。
- QPS = 1000 / 5 个正式计时遍次的累计秒数，包含 FP16 编码、向量查询、ID 提取及结果一致性检查。每遍分别记录 CPU、缺页和上下文切换；这些计数器的读取在计时区间外。7 遍返回 ID 必须逐项一致，召回始终对照原 FP32 GT。
- 每组 6 个独立进程，按轮次倒转规模和引擎顺序，共 60 次。另有 30 次 HNSW 固定 ef=48 对照；48 是此前验证集各规模所选 ef 的最大值，方案在本轮测量前固定。
- 两组都记录完整进程峰值 RSS，含打开、首次访问、预热和正式查询；QPS 仅报告完整工作集预热后的部分。所有运行均保留，无慢轮剔除。

{chr(10).join(parameters)}

HNSW 构图 M=50、ef_construction=500；DiskANN max_degree=32、build_list=100、PQ64。两组都是 FP16/cosine/top-10、只读 mmap、单查询线程与单并发。编译及建库来源见[初次构建参数](results/native/native-build.json)和[建库核验](results/native/build-audit.json)。

## 逐遍证据

下表仅取主对比的百万 HNSW。同一进程内比较首遍完整查询与预热后查询；这里的缺页是 `getrusage(RUSAGE_SELF).ru_majflt` 的阶段差值，与上表的整个进程范围不同。首遍未执行清缓存，不能称为严格冷缓存测试。

{chr(10).join(phases_table)}

全部 90 个进程正式计时阶段的缺页次数最大为 **{audit['measured_major_faults_max']}**；首遍最大为 **{audit['first_pass_major_faults_max']}**。以上证据支持将原先明显波动的主要原因归于缓存/缺页状态混合；实验没有固定系统负载，不能声称消除了所有波动。

## 主对比的波动范围

{chr(10).join(ranges)}

## 固定 ef 的 HNSW 对照

此表全部固定 ef=48，使用相同预热方式，每个规模 6 次，召回均达到 90% 目标。用于隔离主对比中 ef 在 40 和 48 之间变化的影响。{fixed_desc}，不把严格单调作为数据筛选条件。

{chr(10).join(fixed_table)}

固定 ef 对照中，80 万的 QPS 范围为 {f8['qps_min']:.2f}–{f8['qps_max']:.2f}，100 万为 {f10['qps_min']:.2f}–{f10['qps_max']:.2f}，实测范围明显重叠；两者实际 Recall@10 为 {f8['recall_at_10']:.2%} 和 {f10['recall_at_10']:.2%}。因此，小幅中位数回升不足以支持“百万更快”的结论，也不能宣称固定 ef 后每个点必然下降。

## 证据与复现

[configuration.json](results/native-retest/configuration.json) 保存冻结方案、库和脚本摘要；[runs.json](results/native-retest/runs.json) 保存 90 次进程的全部分阶段计数；[summary.json](results/native-retest/summary.json) 和 [fixed-ef-summary.json](results/native-retest/fixed-ef-summary.json) 保存汇总；[environment.json](results/native-retest/environment.json) 保存环境；[audit.json](results/native-retest/audit.json) 记录独立核验结果。

[raw-evidence.json.gz](results/native-retest/raw-evidence.json.gz) 保存全部返回 ID、GT、阶段计时及原始 `/usr/bin/time -l` 输出，可以脱离完整向量数据复核召回和统计。发布程序独立核对全部 90 次返回、QPS、RSS、阶段计数与汇总。实际召回最小值为 {audit['actual_recall_min']:.2%}。

以下从仓库根目录运行，复用已完成的首次实验目录。该目录中的索引与原生库需保留；新环境先按[首次构建步骤](REPORT_NATIVE_CN.md)准备相同源码、库和索引。`OUT` 必须是新目录。

```bash
BASE=/tmp/zvec-local-cohere-20261104
DATA=/tmp/zvec-cohere-million/data
OUT=/tmp/zvec-local-cohere-retest-reproduction
ZVEC_LIB_DIR="$BASE/build/lib" ZVEC_INCLUDE_DIR="$PWD/../zvec/src/include" ZVEC_AUTO_BUILD=0 \\
  RUSTFLAGS='-C link-arg=-Wl,-headerpad_max_install_names' \\
  cargo build --offline --release --manifest-path rust/benchmarks/diskann/harness/Cargo.toml \\
  --bin native-retest --target-dir "$BASE/rust-target"
install_name_tool -change @rpath/libzvec_c_api.dylib "$BASE/build/lib/libzvec_c_api.dylib" \\
  "$BASE/rust-target/release/native-retest"
codesign --force --sign - "$BASE/rust-target/release/native-retest"
python3 rust/benchmarks/diskann/run_native_retest.py \\
  --binary "$BASE/rust-target/release/native-retest" --data "$DATA" \\
  --previous "$BASE/experiment" --output "$OUT" --source-root "$PWD/../zvec" \\
  --native-library "$BASE/build/lib/libzvec_c_api.dylib"
python3 rust/benchmarks/diskann/publish_native_retest.py "$OUT" "$DATA"
python3 rust/benchmarks/diskann/plot_brief.py rust/benchmarks/diskann/results \\
  --dataset native-retest --font '/System/Library/Fonts/STHeiti Light.ttc'
```

本轮 QPS 是固定 200 条查询的预热后结果，不能外推到新查询、冷启动或并发负载。不同规模实际召回不完全相同，见简要报告；固定参数的规模对照已单独列出。
"""
(here / "REPORT_RETEST_NATIVE_CN.md").write_text(detail)
print("Published retest: 60 primary and 30 fixed-ef processes; all raw evidence audited")
