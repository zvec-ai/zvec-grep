# HNSW 与 DiskANN Cohere Wikipedia 测试报告

测试日期：2026-11-03（Asia/Shanghai）。生产实现基线：`4d6c155`。本轮使用 Cohere 的真实文本向量，固定抽样并为每个规模重新生成精确 GT，只比较 **HNSW** 与 **DiskANN**。内存统一使用峰值 **RSS**，新增 **Recall@10 ≥ 90%** 目标实验，并保留默认参数结果供参考。

## 结论

90% 目标实验的五个规模、两个引擎都在测试集达到门槛。10 万条时，HNSW `ef=32` / DiskANN `L=20` 的实际 Recall@10 为 **94.85% / 93.15%**，QPS 为 **1584.76 / 164.34**；HNSW 吞吐量约高 **9.6 倍**。峰值 RSS 为 **225.05 / 42.60 MiB**，DiskANN 低约 **81.1%**。这些结果只针对同一组 200 条固定测试查询。

默认参数实验使用 DiskANN `L=512`，Recall@10 为 **99.95%–100.00%**；10 万条时 QPS 为 **15.78**，RSS 为 **44.05 MiB**。降低召回目标主要改善查询速度，RSS 变化较小。两轮运行时间和重复次数不同，跨轮比值只是观察值，不是严格隔离系统负载的消融结果。生产默认值仍为 `L=512`。

## 数据从哪里来

采用 [Big ANN Benchmarks 的 Wikipedia-Cohere 分发版本](https://github.com/harsha-simhadri/big-ann-benchmarks/blob/main/benchmark/datasets.py#L460-L501)。其库包含 3,500 万条英文 Wikipedia 标题与正文的 Cohere 向量；独立查询文件包含 5,000 条 Simple Wikipedia 的 Cohere 向量，维度均为 768。

- 库文件：[wikipedia_base.bin](https://comp21storage.z5.web.core.windows.net/wiki-cohere-35M/wikipedia_base.bin)。
- 查询文件：[wikipedia_query.bin](https://comp21storage.z5.web.core.windows.net/wiki-cohere-35M/wikipedia_query.bin)。
- 实际抽样块编号、源文件 ETag、查询源行号、随机种子和输出 SHA256 记录在 [dataset.json](results/dataset.json)。

分发基准的距离类型为 inner product；本项目使用 cosine，因此本轮显式归一化向量，并按归一化后的实际 FP32 数据重算 cosine GT。这是本地抽样实验，不是官方完整数据集配置的复刻。

## 抽样与 GT 设计

1. 固定种子 `20261103`，使用 NumPy PCG64。从全库的 35,000 个连续数据块中随机选 120 个块，每块 1,000 条，用 HTTP Range 下载约 12 万条候选。它是分块概率抽样，块内样本有关联，不能视为全库逐行独立抽样。
2. 用 FP64 计算范数并归一化，再保存为 FP32。打乱候选顺序，去掉归一化后字节完全相同的向量，选出 100,000 条。在选满这 10 万条前跳过了 14 个重复候选；源行号映射另存本地。
3. 从独立查询文件打乱选出 220 条唯一向量，排除与选中库向量完全相同的记录；本次未遇到需要排除的查询。前 20 条用于预热，后 200 条用于正式测量，五个规模共用这组查询。
4. 以打乱后的库向量前缀构成 2k、10k、20k、50k、100k 五个嵌套子集。对每个子集独立进行全量 FP64 cosine 计算和排序，生成该子集的精确 top-10 文档 ID。
5. 对每个规模的第 1、200 个测量查询，另用逐元素乘加求和重新计算全库分数，结果与矩阵乘法 GT 一致。记录第 10 与第 11 名的最小分数间隔，检查边界同分情况。

**没有复用全库的 GT，也没有先取全库 top-10 再过滤抽样 ID。** 抽样会改变最近邻集合，因此每个规模都有单独的 `truth-N.json`。数据准备和 GT 计算完全在查询计时之外。

Recall@10 表示 ANN 返回结果对精确向量近邻的召回率，不是人工相关性或问答正确率；100% 不等于语义搜索准确率 100%。重新计算 GT 消除了抽样后真值不匹配的问题，但不能保证任意抽样分布下的 ANN 召回率。

默认参数实验完成后，校验了数据清单内全部文件的 SHA256，并逐条检查五个 DiskANN 索引的存储向量与对应 GT 输入一致；同时从 30 份原始结果重新核算召回率与 QPS。[默认参数校验记录](results/audit.json)。新增实验另从原始返回 ID 复核了 20 次验证运行、60 次测试运行，确认使用冻结参数、QPS 公式及汇总中位数/范围一致。[90% 目标校验记录](results/recall90/audit.json)。

两组使用同一份归一化输入，HNSW 存为 FP16，DiskANN 存为 FP32，统一对照 FP32 精确真值。精度差异可能影响 HNSW 的少量召回损失；本轮没有做量化消融，不能把这部分差异全部归因于图算法。

## 测试协议

| 项目 | 设置 |
|---|---|
| 设备 | Apple M3 Pro，36 GiB RAM，macOS 26.6.2 |
| 构建 | Rust 1.98.0，release；两组使用同一查询二进制 |
| 查询 | 768 维，cosine，top-10，无元数据过滤，不返回向量 |
| HNSW | FP16，M=50，efConstruction=500，mmap=true；默认实验用原有 API 默认值，90% 目标实验显式设置 ef |
| DiskANN | FP32，degree=48，build L=100；默认实验查询 L=512，90% 目标实验由验证集选择 L |
| 只读缓存 | DiskANN SQLite cache 目标 256 KiB，mmap 上限 16 MiB |
| 重复 | 默认实验每组 3 个独立进程；90% 目标实验每组 6 个独立进程 |
| 并发 | 单个在途查询；按进程顺序运行，交替引擎顺序，查询期间不运行建库 |
| 每进程查询 | 20 次预热 + 200 次计时；默认实验 6,000 次、90% 目标实验 12,000 次计时查询 |
| 系统缓存 | 未清空文件缓存，未人为制造内存压力 |

所有索引均通过生产后端构建。HNSW 各规模分别建库；DiskANN 从上一个已关闭数据库复制快照，再按同一输入顺序追加到下个规模。没有使用额外全量向量缓存或延后刷边的特殊 builder。这里测量查询，不比较增量建库耗时与全量建库耗时。

**QPS = 200 / 计时批次秒数**，包含读取查询向量、查询调用和结果 ID 收集；不包含打开索引、预热和 JSON 输出。查询文件通过有界缓冲读取，不把全部库向量加载进查询进程。P50/P95 计量单次查询调用及 ID 转换，不包含读取查询向量。

每进程将 200 个延迟排序，取零基下标 100 和 190 作为 P50/P95。表中为独立进程指标的中位数，而非合并全部延迟后重算分位数；默认实验为 3 次，90% 目标实验为 6 次。内存指标统一为 `/usr/bin/time -l` 的峰值 RSS，包含打开与预热；MiB = 2²⁰ 字节。图中误差线为各次最小值至最大值，不是置信区间。

## 90% 召回目标实验

目标是测试集 Recall@10 **至少 90%**，不是要求两个引擎恰好达到 90%。调参规则在测量前固定：验证集门槛设为 **92%**，为抽样波动预留 2 个百分点；这不是统计置信保证。两组都按候选参数从小到大选择第一个达到门槛的值，没有按测试集结果重新选择参数，也没有修改或丢弃返回结果来凑召回率。

- 验证集：固定新种子 `20261104`，从同一个 Simple Wikipedia 查询文件另取 220 条，20 条预热、200 条验证。源行号和向量均与原有 220 条测试/预热查询不重叠，并排除与库向量完全相同的记录。
- 对这 200 条验证查询，在五个库子集上分别重新计算 FP64 精确 cosine GT；第 1、200 条用独立逐元素求和复核。测试集沿用前轮的 200 条固定查询及其对应精确 GT。
- HNSW 候选 ef：`10/16/24/32/48/64/96/128/192/256/512`。显式关闭线性搜索和 refiner，使用原有 FP16 图索引。
- DiskANN 候选 L：`20/24/32/48/64/96/128/192/256/512`。当前生产实现对 top-10 使用至少 `2 × top-k = 20` 的候选数，因此 20 已是有效下限；较小规模即使最小 L 也可能远高于 90%。
- 十组参数全部冻结后，才开始本轮测试集测量。初轮 3 次 QPS 波动较大，因此保持参数不变，再增加 3 次独立进程；两轮全部 6 次纳入统计。每轮遍历五个规模，合计 60 个测试进程，两个引擎的先后顺序在六次重复间交替。
- 复用前轮已经构建并校验的生产索引；本轮没有建库操作，验证 GT 生成和调参也不在查询计时内。

| 向量数 | 引擎 | 参数 | 验证 Recall@10 | 测试 Recall@10 | QPS ↑ | P50 ms ↓ | P95 ms ↓ | RSS MiB ↓ |
|---:|---|---|---:|---:|---:|---:|---:|---:|
| 2,000 | HNSW | ef=16 | 94.85% | 95.25% | 7469.92 | 0.13 | 0.17 | 39.17 |
| 2,000 | DiskANN | L=20 | 99.45% | 99.45% | 335.49 | 2.85 | 4.14 | 34.79 |
| 10,000 | HNSW | ef=24 | 95.05% | 96.15% | 3797.00 | 0.25 | 0.42 | 54.36 |
| 10,000 | DiskANN | L=20 | 96.70% | 97.75% | 227.90 | 4.14 | 7.31 | 43.11 |
| 20,000 | HNSW | ef=24 | 94.05% | 94.90% | 2837.24 | 0.33 | 0.58 | 73.31 |
| 20,000 | DiskANN | L=20 | 95.15% | 96.40% | 186.90 | 4.94 | 7.78 | 43.17 |
| 50,000 | HNSW | ef=24 | 92.95% | 94.05% | 2421.23 | 0.37 | 0.73 | 130.23 |
| 50,000 | DiskANN | L=20 | 93.75% | 94.70% | 172.49 | 5.64 | 7.84 | 43.02 |
| 100,000 | HNSW | ef=32 | 93.40% | 94.85% | 1584.76 | 0.56 | 1.22 | 225.05 |
| 100,000 | DiskANN | L=20 | 92.10% | 93.15% | 164.34 | 5.96 | 7.91 | 42.60 |

![90% 召回目标：QPS、P95 与实际召回率](results/recall90/performance.png)

![90% 召回目标：查询峰值 RSS](results/recall90/memory.png)

**计时波动仍然明显。** 10 万条时，HNSW 六次 QPS 范围为 **869.45–1887.31**，DiskANN 为 **70.50–171.23**。报告保留全部运行，未挑选最快结果；图中误差线展示完整范围。没有隔离其他系统活动，也没有证明波动具体由哪项因素造成，因此中位数和倍率不应视为稳定容量承诺。

本轮是同一最低召回要求下的参数对比，实际召回率并不完全相等，也不是对所有可能参数的最优 QPS 搜索。六次重复使用同一组 200 条测试查询，增加的是计时重复次数，不是独立查询样本数。

## 默认参数参考：QPS、延迟与召回率

| 向量数 | 引擎 | QPS ↑ | P50 ms ↓ | P95 ms ↓ | Recall@10 ↑ |
|---:|---|---:|---:|---:|---:|
| 2,000 | HNSW | 2735.50 | 0.36 | 0.40 | 99.60% |
| 2,000 | DiskANN | 85.36 | 11.90 | 12.80 | 100.00% |
| 10,000 | HNSW | 1108.63 | 0.89 | 1.11 | 99.70% |
| 10,000 | DiskANN | 35.53 | 27.97 | 31.02 | 100.00% |
| 20,000 | HNSW | 714.39 | 1.40 | 1.72 | 99.55% |
| 20,000 | DiskANN | 25.18 | 39.47 | 46.82 | 100.00% |
| 50,000 | HNSW | 424.45 | 2.36 | 2.93 | 99.90% |
| 50,000 | DiskANN | 17.91 | 55.22 | 67.72 | 100.00% |
| 100,000 | HNSW | 411.51 | 2.42 | 3.19 | 99.75% |
| 100,000 | DiskANN | 15.78 | 63.35 | 81.19 | 99.95% |

![Cohere 查询吞吐量、P95 延迟与召回率](results/performance.png)

## 默认参数参考：查询 RSS

| 向量数 | HNSW RSS MiB | DiskANN RSS MiB |
|---:|---:|---:|
| 2,000 | 39.20 | 35.17 |
| 10,000 | 54.34 | 43.73 |
| 20,000 | 73.33 | 44.00 |
| 50,000 | 130.28 | 44.05 |
| 100,000 | 225.19 | 44.05 |

![Cohere 查询峰值 RSS](results/memory.png)

RSS 包含当前驻留的文件映射页，但不涵盖全部系统文件缓存。这里没有制造系统内存压力，也没有测量浏览器、IDE 等其他应用的响应时间；RSS 降幅不能直接解释为对其他应用的影响有相同降幅。

## 结果说明

**召回率需要用真实向量分布判断。** 抽样只选择现成的文本向量，不会把向量改造成随机方向。本轮的几何结构不同于高维随机向量，不能继续把历史实验的低召回结论用于当前数据。

**当前 DiskANN 的查询内存增长更平缓，吞吐量仍显著较低。** SQLite 按需读取与有界缓存设计符合较平缓的内存曲线，但本轮没有逐项消融，不能量化每项优化的贡献。细粒度 SQL 读取和距离计算是后续应剖析的开销来源，尚未测定各自占比。

**单并发不是服务容量承诺。** 同一索引实例的完整搜索目前受互斥锁保护。提高线程数后的 QPS、尾延迟和每连接内存需要另测；本次也不包含 Node 运行时、嵌入模型、全文检索和服务排队。

**真实 Wikipedia 向量仍不等于代码搜索负载。** 本轮查询是 Simple Wikipedia 段落 embedding，不是人类搜索问题；仅用了 200 个固定测试查询和一个抽样种子。更高置信度、不同语言、真实代码嵌入、过滤查询和冷缓存需要补充实验。

默认 L=512 已能在这组查询上取得较高召回率；90% 目标实验使用独立验证集选择更小的查询参数。适合这组 Wikipedia 向量的参数不一定适合代码向量或带过滤的查询，不能直接据此下调生产默认值。

[历史合成随机向量报告](REPORT_SYNTHETIC_CN.md)单独归档。新旧实验的数据分布、查询输入方式和较大规模的 L 均有差异，不能将跨实验的全部性能变化归因于某一个因素。生产设计与后续优化方向见[中文设计说明](../../crates/zg-diskann/DESIGN_CN.md)。

## 数据与复现

- 90% 目标：[汇总 CSV](results/recall90/summary.csv)、[汇总 JSON](results/recall90/summary.json)、[60 次进程指标](results/recall90/runs.json)、[参数选择](results/recall90/selection.json)、[环境](results/recall90/environment.json)、[验证数据清单](results/recall90/validation-dataset.json)、[结果复核](results/recall90/audit.json)。
- 默认参数：[汇总 CSV](results/summary.csv)、[汇总 JSON](results/summary.json)、[30 次进程指标](results/runs.json)。
- [数据来源与抽样清单](results/dataset.json)、[测试环境与二进制哈希](results/environment.json)。
- [性能图 SVG](results/performance.svg)、[内存图 SVG](results/memory.svg)。
- [抽样与 GT 生成器](prepare_cohere.py)、[生产建库程序](harness/src/bin/build-index.rs)、[查询程序](harness/src/main.rs)、[顺序运行器](run_cohere.py)、[绘图程序](plot.py)。
- [独立验证集与 GT 生成器](prepare_validation.py)、[90% 目标实验运行器](run_recall90.py)。

查询向量、库向量、源行号映射、GT、逐查询结果与大索引保留在本地原始资料目录，不提交大体积向量文件到 Git。抽样清单保存源文件 ETag，下载范围必须匹配；输出数据保存 SHA256 供校验。以下命令会下载公开数据、创建专用基准索引，不操作工作区检索索引。需要 Python、NumPy、curl、Matplotlib 和仓库 Rust 构建环境：

```bash
python3 rust/benchmarks/diskann/prepare_cohere.py /absolute/path/to/cohere-data
cargo build --release --manifest-path rust/benchmarks/diskann/harness/Cargo.toml --target-dir native/target
python3 rust/benchmarks/diskann/run_cohere.py \
  --binary "$PWD/native/target/release/zvec-qps-bench" \
  --builder "$PWD/native/target/release/build-index" \
  --data /absolute/path/to/cohere-data \
  --indexes /absolute/path/to/new-indexes \
  --output /absolute/path/to/new-results
python3 rust/benchmarks/diskann/plot.py \
  /absolute/path/to/new-results/summary.json /absolute/path/to/figures \
  --dataset 'Cohere Wikipedia 抽样' \
  --font '/System/Library/Fonts/Supplemental/Arial Unicode.ttf'
```

每个输出目录应是新的目录。查询 GT 必须由同一次抽样得到的库向量生成，不应混用不同 seed、规模或距离定义的数据。本轮没有修改生产引擎代码。

完成上述数据准备和建库后，可复用同一份索引运行 90% 召回目标实验：

```bash
python3 rust/benchmarks/diskann/prepare_validation.py \
  --data /absolute/path/to/cohere-data \
  --output /absolute/path/to/new-validation
python3 rust/benchmarks/diskann/run_recall90.py \
  --binary "$PWD/native/target/release/zvec-qps-bench" \
  --indexes /absolute/path/to/new-indexes \
  --data /absolute/path/to/cohere-data \
  --validation /absolute/path/to/new-validation \
  --output /absolute/path/to/new-recall90-results
python3 rust/benchmarks/diskann/plot.py \
  /absolute/path/to/new-recall90-results/summary.json /absolute/path/to/new-recall90-figures \
  --dataset 'Cohere Wikipedia 抽样' --recall-target 0.9 \
  --font '/System/Library/Fonts/Supplemental/Arial Unicode.ttf'
```
