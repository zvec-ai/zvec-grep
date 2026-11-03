# HNSW 与 DiskANN Cohere Wikipedia 测试报告

测试日期：2026-11-03（Asia/Shanghai）。生产实现基线：`4d6c155`。本轮使用 Cohere 的真实文本向量，固定抽样并为每个规模重新生成精确 GT，只比较 **HNSW** 与 **DiskANN**。

## 结论

DiskANN Recall@10 为 **99.95%–100.00%**，使用的都是产品默认查询 `L=512`。10 万条时，HNSW 与 DiskANN 的 Recall@10 分别为 **99.75% / 99.95%**。这些是同一组 200 个测量查询的结果，不意味着整个源数据集或任意查询都能达到相同召回率。

10 万条时，HNSW 与 DiskANN 的 QPS 为 **411.51 / 15.78**，HNSW 约高 **26.1 倍**。DiskANN 的 RSS 为 44.05 MiB，较 HNSW 的 225.19 MiB 低约 **80.4%**；footprint 为 16.17 MiB，较 HNSW 的 20.53 MiB 低约 **21.2%**。

真实文本向量上的召回率明显好于此前的随机向量实验。当前实现的内存收益和吞吐量差距仍同时存在，不能把高召回率解释成 QPS 已经追平。

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

测量完成后，校验了数据清单内全部文件的 SHA256，并逐条检查五个 DiskANN 索引的存储向量与对应 GT 输入一致；同时从 30 份原始结果重新核算召回率与 QPS。[校验记录](results/audit.json)。

两组使用同一份归一化输入，HNSW 存为 FP16，DiskANN 存为 FP32，统一对照 FP32 精确真值。精度差异可能影响 HNSW 的少量召回损失；本轮没有做量化消融，不能把这部分差异全部归因于图算法。

## 测试协议

| 项目 | 设置 |
|---|---|
| 设备 | Apple M3 Pro，36 GiB RAM，macOS 26.6.2 |
| 构建 | Rust 1.98.0，release；两组使用同一查询二进制 |
| 查询 | 768 维，cosine，top-10，无元数据过滤，不返回向量 |
| HNSW | FP16，M=50，efConstruction=500，mmap=true，原有 API 默认查询参数 |
| DiskANN | FP32，degree=48，build L=100，所有规模查询 L=512 |
| 只读缓存 | DiskANN SQLite cache 目标 256 KiB，mmap 上限 16 MiB |
| 重复 | 每个引擎、每个规模 3 个独立进程，共 30 个进程 |
| 并发 | 单个在途查询；按进程顺序运行，交替引擎顺序，查询期间不运行建库 |
| 每进程查询 | 20 次预热 + 200 次计时；合计 6,000 次计时查询 |
| 系统缓存 | 未清空文件缓存，未人为制造内存压力 |

所有索引均通过生产后端构建。HNSW 各规模分别建库；DiskANN 从上一个已关闭数据库复制快照，再按同一输入顺序追加到下个规模。没有使用额外全量向量缓存或延后刷边的特殊 builder。这里测量查询，不比较增量建库耗时与全量建库耗时。

**QPS = 200 / 计时批次秒数**，包含读取查询向量、查询调用和结果 ID 收集；不包含打开索引、预热和 JSON 输出。查询文件通过有界缓冲读取，不把全部库向量加载进查询进程。P50/P95 计量单次查询调用及 ID 转换，不包含读取查询向量。

每进程将 200 个延迟排序，取零基下标 100 和 190 作为 P50/P95。表中为三次进程指标的中位数，而非合并 600 个延迟后重算分位数。RSS 和 footprint 为 `/usr/bin/time -l` 的进程峰值，包含打开与预热；MiB = 2²⁰ 字节。图中误差线为三次最小值至最大值，不是置信区间。

## QPS 延迟与召回率

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

## 查询内存

| 向量数 | HNSW RSS MiB | DiskANN RSS MiB | HNSW footprint MiB | DiskANN footprint MiB |
|---:|---:|---:|---:|---:|
| 2,000 | 39.20 | 35.17 | 19.27 | 15.75 |
| 10,000 | 54.34 | 43.73 | 19.38 | 16.17 |
| 20,000 | 73.33 | 44.00 | 19.52 | 16.16 |
| 50,000 | 130.28 | 44.05 | 19.91 | 16.20 |
| 100,000 | 225.19 | 44.05 | 20.53 | 16.17 |

![Cohere 查询峰值 RSS 与 footprint](results/memory.png)

RSS 与 footprint 的统计口径不同：可回收的干净文件映射页可以增加 RSS，而通常不计入 footprint。RSS 的降幅不能直接解释为系统需要为进程保留的内存成本有相同降幅，应同时比较两项指标。[Apple 对 footprint 的说明](https://developer.apple.com/videos/play/wwdc2021/10180/)

## 结果说明

**召回率需要用真实向量分布判断。** 抽样只选择现成的文本向量，不会把向量改造成随机方向。本轮的几何结构不同于高维随机向量，不能继续把历史实验的低召回结论用于当前数据。

**当前 DiskANN 的查询内存增长更平缓，吞吐量仍显著较低。** SQLite 按需读取与有界缓存设计符合较平缓的内存曲线，但本轮没有逐项消融，不能量化每项优化的贡献。细粒度 SQL 读取和距离计算是后续应剖析的开销来源，尚未测定各自占比。

**单并发不是服务容量承诺。** 同一索引实例的完整搜索目前受互斥锁保护。提高线程数后的 QPS、尾延迟和每连接内存需要另测；本次也不包含 Node 运行时、嵌入模型、全文检索和服务排队。

**真实 Wikipedia 向量仍不等于代码搜索负载。** 本轮查询是 Simple Wikipedia 段落 embedding，不是人类搜索问题；仅用了 200 个固定测试查询和一个抽样种子。更高置信度、不同语言、真实代码嵌入、过滤查询和冷缓存需要补充实验。

默认 L=512 已能在这组查询上取得较高召回率。后续可用独立验证集比较 L=64/128/256/512 的 recall-latency 曲线；本轮没有根据这 200 个测量查询做候选数调参。





[历史合成随机向量报告](REPORT_SYNTHETIC_CN.md)单独归档。新旧实验的数据分布、查询输入方式和较大规模的 L 均有差异，不能将跨实验的全部性能变化归因于某一个因素。生产设计与后续优化方向见[中文设计说明](../../crates/zg-diskann/DESIGN_CN.md)。

## 数据与复现

- [汇总 CSV](results/summary.csv)、[汇总 JSON](results/summary.json)、[30 次进程指标](results/runs.json)。
- [数据来源与抽样清单](results/dataset.json)、[测试环境与二进制哈希](results/environment.json)。
- [性能图 SVG](results/performance.svg)、[内存图 SVG](results/memory.svg)。
- [抽样与 GT 生成器](prepare_cohere.py)、[生产建库程序](harness/src/bin/build-index.rs)、[查询程序](harness/src/main.rs)、[顺序运行器](run_cohere.py)、[绘图程序](plot.py)。

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
