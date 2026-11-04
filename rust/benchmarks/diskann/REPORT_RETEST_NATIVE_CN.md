# HNSW QPS 回升排查与 zvec 复测记录

本轮完成于 2026-11-04T16:02:51.397022+08:00。[最新对比表与曲线](REPORT_BRIEF_CN.md)。

## 初次测试的问题

首次每进程仅用 20 条独立查询预热；随后把测试集 200 条查询连续运行 5 遍，首遍也计入 QPS。新进程不代表同样的系统缓存状态，交替规模会改变工作集。原始百万组如下；缺页次数来自原 `/usr/bin/time -l`，范围为整个进程。

| 初次测量轮次 | 百万 HNSW QPS | 进程全程缺页次数 |
|---|---:|---:|
| 1 | 199.13 | 109404 |
| 2 | 2236.50 | 72 |
| 3 | 179.05 | 115924 |
| 4 | 2211.70 | 177 |
| 5 | 153.94 | 115921 |
| 6 | 2629.89 | 190 |

慢轮与快轮的 CPU 计算时间接近，缺页和墙钟耗时差异显著。百万组中位数落在两种状态之间，而 60 万组有 4 次慢轮、2 次快轮，跨规模直接连接这些中位数产生了回升。这不能证明规模越大算法越快。原始数据、图及完整旧方法保留在[初次测试记录](REPORT_NATIVE_CN.md)。

## 复测控制

- 使用完全相同的本地 C++ 库（源码 `960ecbc23f566df729429bdff655ece6dbb2261c`，库 SHA-256 `d9ad81542a3b186a7caf68f9ae9bd3eba67ea4f9e414920fd7cbc944087e3496`），动态链接固定为本地库绝对路径。
- 复用原来的 10 个只读索引，未重建、未修改图或 PQ。实验前后核对全部索引文件大小和修改时间一致；输入向量、查询及各规模 GT 的哈希重新核验。GT 是首次实验穷举生成并复核的原始 FP32 cosine top-10。
- 主对比保持上次冻结的各规模参数，不根据本轮测试结果调参。每进程先运行 20 条独立预热查询，再完整运行同一组 200 条测试查询 2 遍，最后计时 5 遍。
- QPS = 1000 / 5 个正式计时遍次的累计秒数，包含 FP16 编码、向量查询、ID 提取及结果一致性检查。每遍分别记录 CPU、缺页和上下文切换；这些计数器的读取在计时区间外。7 遍返回 ID 必须逐项一致，召回始终对照原 FP32 GT。
- 每组 6 个独立进程，按轮次倒转规模和引擎顺序，共 60 次。另有 30 次 HNSW 固定 ef=48 对照；48 是此前验证集各规模所选 ef 的最大值，方案在本轮测量前固定。
- 两组都记录完整进程峰值 RSS，含打开、首次访问、预热和正式查询；QPS 仅报告完整工作集预热后的部分。所有运行均保留，无慢轮剔除。

| 向量数量 | HNSW ef | DiskANN L |
|---|---:|---:|
| 20 万 | 40 | 96 |
| 40 万 | 48 | 96 |
| 60 万 | 40 | 96 |
| 80 万 | 40 | 96 |
| 100 万 | 48 | 128 |

HNSW 构图 M=50、ef_construction=500；DiskANN max_degree=32、build_list=100、PQ64。两组都是 FP16/cosine/top-10、只读 mmap、单查询线程与单并发。编译及建库来源见[初次构建参数](results/native/native-build.json)和[建库核验](results/native/build-audit.json)。

## 逐遍证据

下表仅取主对比的百万 HNSW。同一进程内比较首遍完整查询与预热后查询；这里的缺页是 `getrusage(RUSAGE_SELF).ru_majflt` 的阶段差值，与上表的整个进程范围不同。首遍未执行清缓存，不能称为严格冷缓存测试。

| 复测轮次 | 百万 HNSW 首遍 QPS | 首遍缺页次数 | 预热后 QPS | 正式计时缺页次数 |
|---|---:|---:|---:|---:|
| 1 | 34.41 | 70947 | 2528.01 | 0 |
| 2 | 1040.67 | 976 | 2302.92 | 0 |
| 3 | 33.83 | 70947 | 2495.88 | 0 |
| 4 | 1522.36 | 0 | 2552.52 | 0 |
| 5 | 32.91 | 71707 | 2362.13 | 0 |
| 6 | 130.68 | 22229 | 2517.96 | 0 |

全部 90 个进程正式计时阶段的缺页次数最大为 **0**；首遍最大为 **71707**。以上证据支持将原先明显波动的主要原因归于缓存/缺页状态混合；实验没有固定系统负载，不能声称消除了所有波动。

## 主对比的波动范围

| 向量数量 | HNSW QPS 范围 | DiskANN QPS 范围 | HNSW RSS 范围（MiB） | DiskANN RSS 范围（MiB） |
|---|---:|---:|---:|---:|
| 20 万 | 2745.85–3754.48 | 76.24–79.79 | 416.31–416.38 | 61.70–61.95 |
| 40 万 | 2370.90–2757.68 | 69.35–74.75 | 793.89–793.97 | 85.08–85.20 |
| 60 万 | 2514.19–3011.11 | 66.76–72.69 | 1156.09–1156.31 | 107.09–107.28 |
| 80 万 | 2564.85–2874.58 | 70.71–73.06 | 1500.28–1501.39 | 129.27–129.42 |
| 100 万 | 2302.92–2552.52 | 52.34–55.16 | 1834.17–1856.08 | 151.38–151.52 |

## 固定 ef 的 HNSW 对照

此表全部固定 ef=48，使用相同预热方式，每个规模 6 次，召回均达到 90% 目标。用于隔离主对比中 ef 在 40 和 48 之间变化的影响。本轮未呈严格单调下降，应结合波动范围判断，不把严格单调作为数据筛选条件。

| 向量数量 | 固定 ef=48 QPS | QPS 范围 | Recall@10 |
|---|---:|---:|---:|
| 20 万 | 3153.96 | 2916.92–3237.33 | 95.90% |
| 40 万 | 2848.18 | 2687.72–2905.43 | 95.45% |
| 60 万 | 2700.50 | 2500.65–2826.97 | 95.00% |
| 80 万 | 2575.90 | 2476.39–2816.02 | 95.25% |
| 100 万 | 2705.81 | 2317.06–2831.57 | 93.10% |

固定 ef 对照中，80 万的 QPS 范围为 2476.39–2816.02，100 万为 2317.06–2831.57，实测范围明显重叠；两者实际 Recall@10 为 95.25% 和 93.10%。因此，小幅中位数回升不足以支持“百万更快”的结论，也不能宣称固定 ef 后每个点必然下降。

## 证据与复现

[configuration.json](results/native-retest/configuration.json) 保存冻结方案、库和脚本摘要；[runs.json](results/native-retest/runs.json) 保存 90 次进程的全部分阶段计数；[summary.json](results/native-retest/summary.json) 和 [fixed-ef-summary.json](results/native-retest/fixed-ef-summary.json) 保存汇总；[environment.json](results/native-retest/environment.json) 保存环境；[audit.json](results/native-retest/audit.json) 记录独立核验结果。

[raw-evidence.json.gz](results/native-retest/raw-evidence.json.gz) 保存全部返回 ID、GT、阶段计时及原始 `/usr/bin/time -l` 输出，可以脱离完整向量数据复核召回和统计。发布程序独立核对全部 90 次返回、QPS、RSS、阶段计数与汇总。实际召回最小值为 93.10%。

以下从仓库根目录运行，复用已完成的首次实验目录。该目录中的索引与原生库需保留；新环境先按[首次构建步骤](REPORT_NATIVE_CN.md)准备相同源码、库和索引。`OUT` 必须是新目录。

```bash
BASE=/tmp/zvec-local-cohere-20261104
DATA=/tmp/zvec-cohere-million/data
OUT=/tmp/zvec-local-cohere-retest-reproduction
ZVEC_LIB_DIR="$BASE/build/lib" ZVEC_INCLUDE_DIR="$PWD/../zvec/src/include" ZVEC_AUTO_BUILD=0 \
  RUSTFLAGS='-C link-arg=-Wl,-headerpad_max_install_names' \
  cargo build --offline --release --manifest-path rust/benchmarks/diskann/harness/Cargo.toml \
  --bin native-retest --target-dir "$BASE/rust-target"
install_name_tool -change @rpath/libzvec_c_api.dylib "$BASE/build/lib/libzvec_c_api.dylib" \
  "$BASE/rust-target/release/native-retest"
codesign --force --sign - "$BASE/rust-target/release/native-retest"
python3 rust/benchmarks/diskann/run_native_retest.py \
  --binary "$BASE/rust-target/release/native-retest" --data "$DATA" \
  --previous "$BASE/experiment" --output "$OUT" --source-root "$PWD/../zvec" \
  --native-library "$BASE/build/lib/libzvec_c_api.dylib"
python3 rust/benchmarks/diskann/publish_native_retest.py "$OUT" "$DATA"
python3 rust/benchmarks/diskann/plot_brief.py rust/benchmarks/diskann/results \
  --dataset native-retest --font '/System/Library/Fonts/STHeiti Light.ttc'
```

本轮 QPS 是固定 200 条查询的预热后结果，不能外推到新查询、冷启动或并发负载。不同规模实际召回不完全相同，见简要报告；固定参数的规模对照已单独列出。
