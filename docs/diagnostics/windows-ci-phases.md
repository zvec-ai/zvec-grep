# Windows CI 分阶段测量

日期：2026-09-10。仅诊断，不包含性能修复，不创建 PR。

## 实验与复现

分支：`perf/windows-ci-phases`。分支上的 `ci.yml` 专用于手动诊断，不运行通常的 CI 矩阵。

- 第一轮提交：`9807e68adba981532d9dd6fba47d9e04fc32961d`
- [第一轮工作流](https://github.com/zvec-ai/zvec-grep/actions/runs/34445244022)：两个平台均成功。
- 第二轮提交：`f02d60d8`
- [第二轮工作流](https://github.com/zvec-ai/zvec-grep/actions/runs/34445586331)：两个平台均成功。

两个平台均固定 Node 24.20.0，使用同一提交与 npm lockfile。服务实验复现 integration 测试中的索引、搜索、自动刷新、删除流程，保持假 embedding 和结果断言，每组启动 5 个独立 Node 进程，各用新临时目录。原生最小实验使用一个带倒排索引的字符串字段和一条记录，每种配置测量 5 次，交替默认/单线程的执行顺序。

`measure-ci-phases.mjs` 在导入服务前包装原生接口，记录墙钟时间、进程 CPU 时间、阶段、集合和错误；同时输出已有 pipeline timings。计时记录先缓存在内存，在清理后统一输出。未修改生产源码。

工作流 artifacts `phases-ubuntu-latest`、`phases-windows-latest` 包含 JSONL 原始结果。第二轮各包含：

- `phase-results.jsonl`：默认服务流程。
- `single-thread-results.jsonl`：仅将 optimize/optimizeSync 的 concurrency 设为 1。
- `minimal-results.jsonl`：直接调用原生集合接口。

本地入口：先 `npm ci`、`npm run build`，再运行 `node .github/scripts/measure-ci-phases.mjs` 或 `node .github/scripts/measure-zvec-minimal.mjs`。设置 `PHASE_OUTPUT` 指定 JSONL 路径，设置 `PHASE_OPTIMIZE_CONCURRENCY=1` 运行服务单线程优化对照。

## 结果

以下均为 5 次测量中位数。全流程不包含模块导入；原生调用包含在外层阶段中，不能与外层耗时相加。合计先逐次求和，再取中位数，所以分项中位数之和不一定等于合计中位数。

| 指标 | 第一轮 Linux | 第一轮 Windows | 第二轮 Linux | 第二轮 Windows |
| --- | ---: | ---: | ---: | ---: |
| 服务全流程 | 3.310 s | 11.807 s | 4.885 s | 10.303 s |
| 原生接口合计 | 2.756 s | 10.824 s | 4.346 s | 9.308 s |
| 四次优化合计 | 1.067 s | 5.952 s | 1.611 s | 4.769 s |

第一轮 Linux CPU 为 Intel Xeon Platinum 8573C，Windows 为 AMD EPYC 7763。第二轮两个 runner 均报告 AMD EPYC 9V74，均为 4 个逻辑 CPU、约 16 GiB 内存。相同 CPU 型号仍不代表相同虚拟机、磁盘或宿主机负载，不能将差异全部归因于操作系统本身。

第二轮服务阶段：

| 阶段 | Linux | Windows |
| --- | ---: | ---: |
| 首次索引 | 1.870 s | 4.194 s |
| 查询索引信息 | 0.174 s | 0.462 s |
| 首次搜索 | 0.370 s | 0.917 s |
| 修改后的自动刷新与搜索 | 1.651 s | 4.700 s |
| 删除索引 | 2.89 ms | 19.22 ms |
| 最后清理临时目录 | 1.66 ms | 3.47 ms |

第一轮首次索引的 pipeline 样本：Windows 扫描 8 ms、prepare 24 ms、embedding 1 ms、commit 13 ms；Linux 分别为 5、24、1、7 ms。主要耗时不在这些步骤。

第二轮对照：

| 指标 | Linux 默认 | Linux concurrency=1 | Windows 默认 | Windows concurrency=1 |
| --- | ---: | ---: | ---: | ---: |
| 服务全流程 | 4.885 s | 4.212 s | 10.303 s | 10.324 s |
| 服务四次优化合计 | 1.611 s | 1.779 s | 4.769 s | 4.777 s |
| 原生单记录集合 optimizeSync | 26.03 ms | 26.30 ms | 191.83 ms | 198.80 ms |

Linux 两组服务按默认、单线程顺序执行，存在预热和运行波动，不应将总耗时下降直接视为参数收益。Windows 总耗时和优化耗时均无改善；原生最小实验也不支持单线程能解决此瓶颈。

## 结论

1. 主要瓶颈已缩小到 zvec 原生接口。两轮 Windows 原生接口耗时约占服务全流程的 90%～92%，四次优化占约 46%～50%。原生最小实验在没有服务层、扫描、解析、embedding 的情况下仍复现优化差异。
2. 元数据集合有隐藏成本：`ZvecFileMetaStore.close()` 在数据变更后执行 `optimizeSync()`。首次索引和自动刷新各执行一次，此外实体集合也各优化一次。已有 `index_optimize` 只计入实体集合优化，不包括随后元数据集合关闭时的优化。
3. 建库、重复打开和关闭集合是次要但明显的累计成本。最终 service.close 很快不代表关闭存储很快：很多集合已在 index/context/info 的内部打开并关闭。
4. 本次没有记录到原生接口错误或应用层开库失败重试；清理耗时不足以解释差距。没有证据支持把本次瓶颈归因于模型推理或临时目录删除。
5. 将优化并发设为 1 无效，不应直接作为 CI 修复。

## 尚未确定与后续方向

当前定位精度到原生 API，尚未区分原生优化内部的计算、文件同步、线程等待及系统实时扫描。不能据此断言磁盘、杀毒软件或某个系统调用就是根因。进程 CPU 时间只能帮助区分 CPU 工作和等待，不能独立证明 I/O 来源。

建议下一步优先在 zvec 层测量真实 metadata schema 的 optimize 子阶段，并用 Windows 原生性能追踪区分等待与文件操作；再验证是否能降低每次关闭时全量优化的频率。任何延迟或省略优化的方案都需验证索引可见性、读写一致性和异常退出恢复。

CI 层面的分片、拆分大测试文件能缩短墙钟等待，但不会解决单次原生操作成本。先修正计时覆盖范围，再评估减少重复建库/打开/优化，以及安全复用 fixture 的收益。
