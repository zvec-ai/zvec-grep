"""Independently audit native runs and publish portable evidence and Chinese reports."""
import argparse
import gzip
import hashlib
import json
import pathlib
import re
import shutil
import statistics

parser = argparse.ArgumentParser()
parser.add_argument("experiment", type=pathlib.Path)
parser.add_argument("data", type=pathlib.Path)
args = parser.parse_args()
source = args.experiment
here = pathlib.Path(__file__).resolve().parent
destination = here / "results/native"
destination.mkdir(parents=True, exist_ok=True)
assert json.loads((source / "status.json").read_text())["stage"] == "complete"
config = json.loads((source / "configuration.json").read_text())
assert config["local_source_build"]
source_commit = config["zvec_source_commit"]
selection = json.loads((source / "selection.json").read_text())
runs = json.loads((source / "runs.json").read_text())
summary = json.loads((source / "summary.json").read_text())
assert selection["frozen_before_test"] and len(selection["selected"]) == 10
assert len(runs) == 60 and len(summary) == 10
for folder, name in [(args.data, "dataset.json"), (args.data / "validation", "validation-dataset.json")]:
    manifest = json.loads((folder / "manifest.json").read_text())
    assert manifest == json.loads((source / name).read_text())
    for filename, metadata in manifest["files"].items():
        with (folder / filename).open("rb") as stream:
            assert hashlib.file_digest(stream, "sha256").hexdigest() == metadata["sha256"]
for chosen in selection["selected"]:
    trials = [r for r in selection["trials"] if r["count"] == chosen["count"] and r["engine"] == chosen["engine"]]
    assert [r["effort"] for r in trials] == config["candidate_grid"][:config["candidate_grid"].index(chosen["effort"]) + 1]
    assert all(r["recall_at_10"] < .92 for r in trials[:-1]) and trials[-1] == chosen
query_sets = []
for path in [args.data / "queries.f32", args.data / "validation/queries.f32"]:
    query_bytes = path.read_bytes()
    assert len(query_bytes) == 220 * 768 * 4
    vectors = {query_bytes[i:i + 768 * 4] for i in range(0, len(query_bytes), 768 * 4)}
    assert len(vectors) == 220
    query_sets.append(vectors)
assert not query_sets[0] & query_sets[1], "Exact query vectors overlap across validation/test"
evidence = []
for phase, records in [("validation", selection["trials"]), ("test", runs)]:
    for row in records:
        raw = source / row["raw_file"]
        stages = [json.loads(line) for line in raw.read_text().splitlines()]
        result = next(s for s in stages if s["stage"] == "queried")
        for key in ["engine", "count", "effort", "queries", "passes", "batch_s", "qps", "mean_ms", "p50_ms", "p95_ms"]:
            assert result[key] == row[key], (raw, key)
        ids = next(s["ids"] for s in stages if s["stage"] == "results")
        truth_root = args.data / "validation" if phase == "validation" else args.data
        truth = json.loads((truth_root / f"truth-{row['count']}.json").read_text())
        recall = sum(len(set(a) & set(b)) for a, b in zip(ids, truth)) / 2000
        assert len(ids) == len(truth) == 200 and all(len(set(x)) == len(x) == 10 for x in ids)
        assert recall == row["recall_at_10"]
        stderr = raw.with_suffix(".stderr").read_text()
        rss = int(re.search(r"(\d+)\s+maximum resident set size", stderr).group(1)) / 2**20
        assert rss == row["rss_mib"]
        assert abs(result["qps"] - result["queries"] / result["batch_s"]) < 1e-8
        if phase == "test":
            assert result["queries"] == 1000 and result["passes"] == 5 and recall >= .9
            chosen = next(r for r in selection["selected"] if r["count"] == row["count"] and r["engine"] == row["engine"])
            assert result["effort"] == chosen["effort"] and chosen["recall_at_10"] >= .92
        evidence.append(dict(phase=phase, file=row["raw_file"], stages=stages, time_stderr=stderr))
for row in summary:
    group = [r for r in runs if r["count"] == row["count"] and r["engine"] == row["engine"]]
    assert {r["repeat"] for r in group} == set(range(6)) and len(group) == 6
    for metric in ["qps", "rss_mib", "recall_at_10", "mean_ms", "p50_ms", "p95_ms"]:
        assert row[metric] == statistics.median(r[metric] for r in group)
        assert row[metric + "_min"] == min(r[metric] for r in group)
        assert row[metric + "_max"] == max(r[metric] for r in group)
for name in ["configuration.json", "selection.json", "runs.json", "summary.json", "environment.json",
             "build-audit.json", "data-audit.json", "dataset.json", "validation-dataset.json", "native-build.json"]:
    shutil.copy2(source / name, destination / name)
if (source / "loader-audit.json").exists():
    loader_audit = json.loads((source / "loader-audit.json").read_text())
    assert loader_audit["native_library_sha256"] == config["native_library_sha256"]
    assert loader_audit["local_library_verified_in_actual_benchmark_process"]
    shutil.copy2(source / "loader-audit.json", destination / "loader-audit.json")
if (source / "implementation.json").exists():
    implementation = json.loads((source / "implementation.json").read_text())
    repo = here.parents[2]
    for relative, metadata in implementation.items():
        if isinstance(metadata, dict) and "sha256" in metadata:
            assert hashlib.sha256((repo / relative).read_bytes()).hexdigest() == metadata["sha256"], relative
    shutil.copy2(source / "implementation.json", destination / "implementation.json")
truth_bundle = {
    phase: {str(count): json.loads((folder / f"truth-{count}.json").read_text()) for count in config["counts"]}
    for phase, folder in [("test", args.data), ("validation", args.data / "validation")]
}
payload = json.dumps(dict(runs=evidence, truth=truth_bundle), separators=(",", ":")).encode()
(destination / "raw-evidence.json.gz").write_bytes(gzip.compress(payload, mtime=0))
audit = dict(all_60_measured_runs_verified=True, all_validation_trials_verified=True,
             all_queries_unique_and_validation_test_vectors_disjoint=True,
             actual_test_recall_at_10_min=min(r["recall_at_10"] for r in runs),
             raw_evidence_uncompressed_sha256=hashlib.sha256(payload).hexdigest())
(destination / "audit.json").write_text(json.dumps(audit, indent=2) + "\n")

table = ["| 向量数量 | HNSW QPS | DiskANN QPS | HNSW RSS（MiB） | DiskANN RSS（MiB） | HNSW / DiskANN Recall@10 |",
         "|---|---:|---:|---:|---:|---:|"]
parameters = ["| 向量数量 | HNSW ef | DiskANN L | 验证集 Recall@10：HNSW / DiskANN |",
              "|---|---:|---:|---:|"]
ranges = ["| 向量数量 | HNSW QPS 范围 | DiskANN QPS 范围 | HNSW RSS 范围（MiB） | DiskANN RSS 范围（MiB） |",
          "|---|---:|---:|---:|---:|"]
for count in config["counts"]:
    h, d = [next(r for r in summary if r["count"] == count and r["engine"] == engine) for engine in ["hnsw", "diskann"]]
    hv, dv = [next(r for r in selection["selected"] if r["count"] == count and r["engine"] == engine) for engine in ["hnsw", "diskann"]]
    table.append(f"| {count // 10000} 万 | {h['qps']:.2f} | {d['qps']:.2f} | {h['rss_mib']:.2f} | {d['rss_mib']:.2f} | {h['recall_at_10']:.2%} / {d['recall_at_10']:.2%} |")
    parameters.append(f"| {count // 10000} 万 | {h['effort']} | {d['effort']} | {hv['recall_at_10']:.2%} / {dv['recall_at_10']:.2%} |")
    ranges.append(f"| {count // 10000} 万 | {h['qps_min']:.2f}–{h['qps_max']:.2f} | {d['qps_min']:.2f}–{d['qps_max']:.2f} | {h['rss_mib_min']:.2f}–{h['rss_mib_max']:.2f} | {d['rss_mib_min']:.2f}–{d['rss_mib_max']:.2f} |")
h1, d1 = [next(r for r in summary if r["count"] == 1000000 and r["engine"] == engine) for engine in ["hnsw", "diskann"]]
h0, d0 = [next(r for r in summary if r["count"] == 200000 and r["engine"] == engine) for engine in ["hnsw", "diskann"]]
env = json.loads((source / "environment.json").read_text())
date = env["finished"][:10]
report = f"""# zvec DiskANN 与 HNSW：Cohere 测试报告

测试日期：{date}。两组均使用 **本地 zvec 源码 `{source_commit[:8]}` 编译的原生库、FP16、768 维、cosine、top-10、单并发**。本报告全部数据来自本轮重新建库与测量，DiskANN 已启用内置 PQ64。

数据沿用此前 Cohere Wikipedia 抽样：20／40／60／80／100 万是同一百万条语料的嵌套前缀。每个规模的测试集和验证集精确 GT 均重新计算，并与原文件逐项核对一致。独立验证集先选取达到 92% Recall@10 的最低候选参数，再冻结参数测测试集，要求 **Recall@10 ≥ 90%**；实际召回率见表，不表示精确匹配在 90%。

{chr(10).join(table)}

![QPS 和查询峰值 RSS 曲线](results/brief/qps-memory.png)

在 100 万条时，DiskANN 的 QPS 为 HNSW 的 **{d1['qps']/h1['qps']:.2f} 倍**，查询峰值 RSS 为 HNSW 的 **{d1['rss_mib']/h1['rss_mib']:.2%}**。请结合上表各规模的实际召回率比较；这不是完全相同召回率下的吞吐量测试。

从 20 万增至 100 万条，DiskANN 的 RSS 由 **{d0['rss_mib']:.2f} 增至 {d1['rss_mib']:.2f} MiB**，HNSW 由 **{h0['rss_mib']:.2f} 增至 {h1['rss_mib']:.2f} MiB**。本轮 DiskANN 的内存也随数据量增长，增幅小于 HNSW。

本轮 QPS 有明显波动：100 万条 HNSW 六次为 **{h1['qps_min']:.2f}–{h1['qps_max']:.2f}**，DiskANN 为 **{d1['qps_min']:.2f}–{d1['qps_max']:.2f}**。全部轮次均保留，中位数不能视为稳定吞吐量或固定的性能倍数；完整范围见详细记录。
HNSW 曲线在较大规模回升，不能解释为向量越多查询越快：本轮单次测量的波动大于相邻规模的中位数差异。

每组运行 6 个独立进程，轮换引擎和规模顺序；各进程预热 20 次，然后将同一组 200 条测试查询顺序执行 5 遍，共计时 1000 次。表格和曲线取 6 次测量的中位数。QPS 包含 FP16 查询编码、原生查询及结果 ID 提取，查询输入在计时前读入。

RSS 使用 macOS `/usr/bin/time -l` 的 maximum resident set size，单位 MiB = 2²⁰ 字节，包含打开索引、预热及查询阶段，**不含建库进程**。这是进程峰值驻留内存，不包含进程之外的全部系统文件缓存，不能单凭它保证其他应用不受影响。

环境：{env['cpu']}、{env['memory_bytes']/2**30:g} GiB 内存、macOS。查询并发和 zvec 查询线程均为 1；未清空系统文件缓存、未隔离其他应用负载。本轮固定查询工作集、测量调用数及线程数，不能与旧版微软 DiskANN / SQLite 报告直接归因比较，也不代表冷缓存或高并发性能。

完整参数、原始证据和复现步骤见 [详细记录](REPORT_NATIVE_CN.md)。
"""
(here / "REPORT_BRIEF_CN.md").write_text(report)
details = f"""# zvec 原生 DiskANN / HNSW 测试详细记录

结果与曲线见 [简要报告](REPORT_BRIEF_CN.md)。本轮完成于 {env['finished']}。

## 数据和 GT

沿用 [Big ANN Benchmarks 的 Wikipedia-Cohere 数据定义](https://github.com/harsha-simhadri/big-ann-benchmarks/blob/main/benchmark/datasets.py)。本地数据抽样、来源行号、随机种子和文件 SHA-256 均保存在 [dataset.json](results/native/dataset.json)，验证查询见 [validation-dataset.json](results/native/validation-dataset.json)。没有重新随机抽样。

对 100 万 FP32 输入执行分块 FP64 cosine 穷举，在每个目标前缀重新生成 top-10，与之前 GT 逐项匹配。并列得分以 ID 排序。独立验证集和测试集各 200 条计分查询，另各有 20 条预热查询。GT 使用原始 FP32；两组索引和查询均为 FP16，因此召回包含量化误差。[核对结果](results/native/data-audit.json)。

## 构建与查询

两种索引均由本地 zvec 源码提交 `{source_commit}` 编译的同一套 Release C++ 原生库执行。Rust 调用层为 `zvec-rust 0.8.0`，通过 `ZVEC_LIB_DIR` 链接本地库，可执行文件中的动态库依赖固定为本地库的绝对路径，并以 `otool` 校验。动态库 SHA-256 为 `{config['native_library_sha256']}`；程序检查可执行文件旁的库与本地构建产物一致，并在实验结束后再次核验库与源码提交。编译器、CMake 参数及第三方依赖提交见 [native-build.json](results/native/native-build.json)。

每个规模从空集合新建，仅含相同的主键和 embedding 字段。测量范围为向量索引查询，不包含完整 zg 的 embedding、FTS、过滤和重排流程。SDK 预编译库的未完成实验未纳入本报告。

| 设置 | HNSW | DiskANN |
|---|---|---|
| 向量 / 查询 | FP16 / FP16 | FP16 / FP16 |
| 距离 | cosine | cosine |
| 图参数 | M=50，ef_construction=500 | max_degree=32，build_list=100 |
| PQ | 无 | 64 段，内置训练和编码 |
| 建库线程 | 8 | 8 |
| 查询线程 / 并发 | 1 / 1 | 1 / 1 |
| 打开方式 | 只读，mmap 开启 | 只读，mmap 开启 |

建库后 flush、optimize、flush，验证文档数量、索引完整度；查询时再检查 FP16 和索引类型。DiskANN 文件核对了 `diskann.vector`、`diskann.pq_data` 段，见 [build-audit.json](results/native/build-audit.json)。使用本地 zvec 的实现，未调用微软 Rust DiskANN 或自建 SQLite 向量存储。

## 参数选择

候选值为 `{{GRID_PLACEHOLDER}}`。依升序选择独立验证集首次达到 92% Recall@10 的值，所有参数在查看本轮测试结果前冻结；测试结果不用于再次调参。目标为测试集至少 90%，保留 2 个百分点的验证余量。不同算法、规模的最终召回率不完全相同。

{chr(10).join(parameters)}

[全部验证试验与所选参数](results/native/selection.json)。基准独立调查询参数，未修改应用默认值。

## 计量与核验

6 轮，每轮每个规模、每种引擎启动新进程。奇数轮倒转规模与引擎顺序，减轻固定顺序偏差；建库和 GT 计算在全部查询测量前结束。每进程 20 次预热、200 条不同测试查询 × 5 遍；每遍返回 ID 一致性由程序断言。重复查询意味着这是有限工作集的预热后测试，不等于 1000 条不同查询或全库冷访问。

各进程 QPS = 1000 / 计时秒数。RSS 为新查询进程整个生命周期的 maximum resident set size，含索引加载和预热；报告分别取 QPS 与峰值 RSS 的中位数，二者不必来自同一进程。只用 RSS，不展示 footprint。未固定文件缓存及系统负载，保留每轮数据和极值供核查，曲线不添加误差棒。

[summary.json](results/native/summary.json) 含中位数与最小／最大值；[runs.json](results/native/runs.json) 含 60 次测量；[environment.json](results/native/environment.json) 含系统信息；[configuration.json](results/native/configuration.json) 含二进制和数据摘要。

以下为 6 个独立进程的最小值至最大值，未剔除慢运行。打开新进程不等于清空系统文件缓存，轮换规模也会改变缓存工作集；本轮没有控制系统缓存和其他应用负载，因此不能仅凭这些日志把波动归因于某一种因素。

{chr(10).join(ranges)}

[raw-evidence.json.gz](results/native/raw-evidence.json.gz) 保存全部验证和测试返回 ID、对应 GT、计时输出及原始 `/usr/bin/time -l` 输出；可用 Python `gzip.open` 读取，无需下载完整向量即可复核召回。发布程序独立重算召回、RSS、QPS 和汇总，见 [audit.json](results/native/audit.json)。

## 复现

在仓库根目录运行；`DATA` 应指向此前保留的百万条 Cohere 数据（含 `base.f32`、查询、manifest、各规模 GT 和 validation 子目录），`OUT` 使用新目录。复现本文版本时，使用上述 zvec 提交的独立检出；脚本记录实际 HEAD。

```bash
DATA=/tmp/zvec-cohere-million/data
OUT=/tmp/zvec-native-cohere-reproduction
ZVEC_SOURCE="$PWD/../zvec"
CBUILD="$OUT/build"
git -C "$ZVEC_SOURCE" submodule update --init --depth 1
cmake -S "$ZVEC_SOURCE" -B "$CBUILD" -G Ninja -DCMAKE_BUILD_TYPE=Release \\
  -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DUSE_OSS_MIRROR=ON \\
  -DBUILD_C_BINDINGS=ON -DBUILD_PYTHON_BINDINGS=OFF -DBUILD_TOOLS=OFF \\
  -DBUILD_ZVEC_SHARED=OFF -DBUILD_ZVEC_CORE_SHARED=OFF -DBUILD_ZVEC_AILEGO_SHARED=OFF
cmake --build "$CBUILD" --target zvec_c_api --parallel 4
ZVEC_LIB_DIR="$CBUILD/lib" ZVEC_INCLUDE_DIR="$ZVEC_SOURCE/src/include" ZVEC_AUTO_BUILD=0 \\
  RUSTFLAGS="-C link-arg=-Wl,-headerpad_max_install_names" \\
  cargo build --release --manifest-path rust/benchmarks/diskann/harness/Cargo.toml \\
  --bin native-bench --target-dir "$OUT/rust-target"
install_name_tool -change @rpath/libzvec_c_api.dylib "$CBUILD/lib/libzvec_c_api.dylib" \\
  "$OUT/rust-target/release/native-bench"
codesign --force --sign - "$OUT/rust-target/release/native-bench"
VECLIB_MAXIMUM_THREADS=4 OPENBLAS_NUM_THREADS=4 python3 rust/benchmarks/diskann/run_native.py \\
  --binary "$OUT/rust-target/release/native-bench" --data "$DATA" --output "$OUT/experiment" \\
  --source-root "$ZVEC_SOURCE" --native-library "$CBUILD/lib/libzvec_c_api.dylib"
python3 rust/benchmarks/diskann/publish_native.py "$OUT/experiment" "$DATA"
python3 rust/benchmarks/diskann/plot_brief.py rust/benchmarks/diskann/results --dataset native \\
  --font '/System/Library/Fonts/STHeiti Light.ttc'
```

脚本只复用具有完成标记且输入摘要一致的阶段；未完成的索引不会静默复用。脚本不会删除旧实验。不同构建的图可能有差异，复测数值不保证逐位一致。
""".replace("{GRID_PLACEHOLDER}", ", ".join(map(str, config["candidate_grid"])))
(here / "REPORT_NATIVE_CN.md").write_text(details)
print("Audited and published", len(runs), "measured runs to", destination)
