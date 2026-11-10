# 使用 zvec DiskANN 统一 Rust 和 TypeScript 向量后端

此前向量检索调用微软 Rust DiskANN，并通过 SQLite 单独保存向量与图边。现在 Rust 和 TypeScript 直接使用 zvec DiskANN，向量、全文和元数据共用集合，过滤条件直接进入 zvec 查询，减少跨存储同步与额外结果回查。

- 使用 zvec 0.8.0 SDK、FP16 向量与内置 PQ；768 维时使用 PQ64。应用默认构图参数仍为出度 32、构图候选数 100。
- Rust 索引版本为 4，TypeScript 为 3；拒绝旧 HNSW、SQLite 向量索引及不匹配字段，用户通过 `zg --index --rebuild` 显式重建。
- TypeScript 移除独立 Rust 向量绑定；微软方案只用于历史和对照基准，不属于主引擎运行时依赖。

## 本地 zvec 优化与精简

本地 zvec 的 `perf/diskann-bounded-read-cache` 分支最终提交为 `3b1a3aa6`。有界节点读取、标量页缓存、紧凑 ID 映射、余弦 PQ 导航和缓存命中提前展开均保留。本次合并重复的缓存命中处理与未命中读取逻辑，错误数组仅按未命中数量分配；搜索策略、缓存预算、距离计算和磁盘格式保持不变。

这些 C++ 优化尚未进入已发布的 SDK 0.8.0 二进制。本轮重新编译本地库，并在建库和查询时用 `dladdr` 核对实际加载路径，两种索引均走同一个 `Collection::query` 接口。

## 最终分支的 HNSW 对比（2026-11-10）

恢复此前相同的百万 Cohere 样本并校验 SHA-256，两边均重新建库、存储 FP16。600 条预热、400 条验证和 3,500 条测试查询在源行号及 FP32/FP16 内容上互不重叠。查询参数在验证集选择并冻结；正式测试中每条查询在每种索引、每个规模下只执行一次。这些查询曾用于前轮测试，本轮属于回归验证。

HNSW 构图参数为 M=50、ef_construction=500；DiskANN 为 R=64、构图 L=200、PQ64。DiskANN 每个图 reader 使用 8 MiB 读取与范数预算、24 个 I/O 工作线程，每个标量 IPC 文件缓存 1 MiB；这是显式测试配置，应用默认构图参数仍为 32/100。

| 向量数 | QPS：HNSW / DiskANN | 查询峰值 RSS（MiB）：HNSW / DiskANN | Recall@10：HNSW / DiskANN |
|---|---:|---:|---:|
| 20 万 | 3065.2 / 747.6 | 422.2 / 70.7 | 95.09% / 93.98% |
| 40 万 | 2845.8 / 740.9 | 801.5 / 84.9 | 94.49% / 92.10% |
| 60 万 | 2658.2 / 628.3 | 1181.1 / 99.1 | 94.26% / 93.68% |
| 80 万 | 2595.6 / 612.1 | 1560.3 / 113.3 | 93.97% / 93.57% |
| 100 万 | 2579.2 / 593.0 | 1939.2 / 127.5 | 93.65% / 93.35% |

DiskANN 查询峰值 RSS 低 83.3%–93.4%，HNSW QPS 约为 DiskANN 的 3.84–4.35 倍。各规模整体召回率均超过 90%，70 个正式分片的最低召回率为 91.66%；两边实际召回率不同。QPS 按总查询数除以总耗时汇总，RSS 为 7 个独立进程峰值的中位数。DiskANN 的建库峰值 RSS 在本轮略高，完整建库表见报告。

本轮没有测精简前后的性能差值，以上差异是最终版本中两种索引的对比。

## 验证

- 95 项相关 C++ 测试通过，2 项 x86 AVX 测试在 ARM 平台跳过；两种索引的小规模建库与 Collection 查询均通过。
- 核对 39 个验证进程与 70 个正式进程记录；独立复算正式返回 ID、GT、QPS 和 RSS，重新检查查询集合互斥、二进制哈希及索引未变更。
- 新增独立的 native 基准程序、数据恢复和审计绘图脚本。此次基准不依赖微软适配层，也未修改应用查询循环或 SDK 依赖。

[本轮中文报告与曲线图](rust/benchmarks/diskann/REPORT_HNSW_FINAL_CN.md) · [核验记录](rust/benchmarks/diskann/results/final-native/report-audit.json) · [原始证据](rust/benchmarks/diskann/results/final-native/raw-evidence.json.gz)

[此前与微软适配层的对比](rust/benchmarks/diskann/REPORT_NODE_CN.md) 保留为历史报告，其数据来自此前版本和测试轮次。
