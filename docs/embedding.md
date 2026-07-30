# Embedding 模型选型指南

Embedding 模型决定语义检索质量、建索引速度、内存占用和索引大小。第一次建立索引时
需要明确选择模型：

```bash
zg index --embedding local/jina-embeddings-v2-base-code
```

本地模型不会把源码或查询发送给远程 Embedding 服务。权重会在第一次使用时自动下载
并缓存到 `~/.zvec-grep/models`。远程模型不占用本地推理资源，但会把需要生成向量的
文本发送给对应服务商。

## 快速选择

| 使用场景             | 建议首先尝试                                                 | 选择理由                                         |
| -------------------- | ------------------------------------------------------------ | ------------------------------------------------ |
| 本地代码检索质量优先 | `local/jina-embeddings-v2-base-code`                         | CoIR-ZG 中综合质量最高，Recall@100 为 0.982      |
| 本地多语言通用搜索   | `local/embeddinggemma-300m`                                  | 质量与 Jina 接近，适合代码、文档和多语言混合仓库 |
| 轻量英文仓库         | `local/all-minilm-l6-v2`                                     | 资源占用低，质量和索引速度比 BGE 更均衡          |
| 极快建立代码索引     | `local/potion-code-16m-v2`                                   | 静态向量查表，实测约 40 秒完成 20,604 个文件     |
| 避免本地模型运行     | `qwen/qwen3.7-text-embedding`                                | 使用托管 API，不需要下载和加载本地模型           |
| 英文长文档           | `local/gte-modernbert-base` 或 `local/nomic-embed-text-v1.5` | 8,192-token 输入容量，适合较长的文档实体         |
| 轻量多语言仓库       | `local/multilingual-e5-small`                                | 比 Gemma 和 Qwen3 轻量，覆盖多种语言             |

如果仓库主要是代码，推荐从 Jina Code 开始；机器资源有限时选择 MiniLM；需要频繁为
大量临时仓库建立索引时选择 Potion Code。

## zvec-grep 实测结果

下面是 CoIR-ZG CosQA 的完整系统测试摘要：20,604 个 Python 文件、500 条查询，
vector-only 检索，每条查询先取 500 个 fragment，再按文档去重到 Top 100。指标使用
`ir-measures` 独立复核。

| 模型                                 |    nDCG@10 | Recall@10 | Recall@100 |  索引时长 |    平均查询 | 查询峰值内存 |
| ------------------------------------ | ---------: | --------: | ---------: | --------: | ----------: | -----------: |
| `local/jina-embeddings-v2-base-code` | **0.3947** |     0.686 |  **0.982** |    432.2s |     355.5ms |      3.46GiB |
| `local/embeddinggemma-300m`          |     0.3892 | **0.690** |      0.974 |    794.3s |     368.5ms |      3.47GiB |
| `local/qwen3-embedding-0.6b`         |     0.3680 |     0.652 |      0.952 |   1695.5s |     358.6ms |      6.94GiB |
| `qwen/qwen3.7-text-embedding`        |     0.3381 |     0.612 |      0.938 |    169.5s |     576.3ms |      1.58GiB |
| `qwen/text-embedding-v4`             |     0.3304 |     0.578 |      0.930 |    142.6s |     542.3ms |      1.58GiB |
| `local/all-minilm-l6-v2`             |     0.2850 |     0.496 |      0.874 |    197.1s |     348.0ms |      1.79GiB |
| `local/bge-small-en-v1.5`            |     0.2798 |     0.508 |      0.842 |    305.9s | **331.1ms** |      1.99GiB |
| `local/potion-code-16m-v2`           |     0.2486 |     0.452 |      0.844 | **40.0s** |     349.5ms |      1.59GiB |
| `local/potion-base-8m`               |     0.1464 |     0.268 |      0.578 |     41.6s |     339.4ms |      1.72GiB |

测试环境是 Apple M4 Pro、48GiB RAM。索引时长排除了本地模型的首次下载，但仍会受
CPU、GPU、磁盘和运行时版本影响；平均查询时间包含 zvec-grep 服务和向量检索开销，
不是纯模型推理延迟。

CoIR-ZG 测的是“模型 + 文件提取 + fragment 切分 + zvec-grep 检索”的最终效果。
不同模型的输入上限会影响 fragment 大小，因此它适合指导 zvec-grep 选型，但不能
替代模型厂商发布的纯 Embedding 榜单。完整指标、环境和复现方法见
[CoIR-ZG benchmark](../benchmarks/coir-zg/README.md)。

## 本地模型规格

下载大小是 zvec-grep 固定模型版本的权重近似大小。完整缓存还会包含 tokenizer 和
配置文件。

| 模型                                 | 主要用途         | 语言               | 模型格式         | 约下载大小 | 最大输入 tokens | 向量维度 |
| ------------------------------------ | ---------------- | ------------------ | ---------------- | ---------: | --------------: | -------: |
| `local/all-minilm-l6-v2`             | 轻量英文搜索     | 英文               | ONNX Q4          |       55MB |             256 |      384 |
| `local/potion-base-8m`               | 极低延迟通用搜索 | 英文               | Safetensors FP32 |       30MB |             512 |      256 |
| `local/potion-code-16m-v2`           | 极低延迟代码检索 | 英文及多种编程语言 | Safetensors FP16 |       33MB |           8,192 |      256 |
| `local/bge-small-en-v1.5`            | 轻量英文检索     | 英文               | ONNX Q4          |       61MB |             512 |      384 |
| `local/multilingual-e5-small`        | 轻量多语言搜索   | 94 种语言          | ONNX Q8          |      118MB |             512 |      384 |
| `local/nomic-embed-text-v1.5`        | 英文长文本       | 英文               | ONNX Q4          |      165MB |           8,192 |      768 |
| `local/gte-modernbert-base`          | 英文长文档       | 英文               | ONNX Q4          |      224MB |           8,192 |      768 |
| `local/jina-embeddings-v2-base-code` | 代码搜索         | 英文及多种编程语言 | ONNX Q8          |      162MB |           8,192 |      768 |
| `local/embeddinggemma-300m`          | 通用多语言搜索   | 100 多种语言       | GGUF Q8_0        |      334MB |           2,048 |      768 |
| `local/qwen3-embedding-0.6b`         | 高容量多语言搜索 | 100 多种语言       | GGUF Q8_0        |      639MB |           8,192 |    1,024 |

E5、GTE 和 Nomic 已受 zvec-grep 支持，但尚未纳入上面的 CoIR-ZG 对比。选择这些
模型时，建议用自己的仓库和真实查询补充验证。

## 远程模型

| 模型                          | 适用场景                             | CoIR-ZG 状态 |
| ----------------------------- | ------------------------------------ | ------------ |
| `qwen/qwen3.7-text-embedding` | 托管文本 Embedding，代码检索质量优先 | 已测试       |
| `qwen/text-embedding-v4`      | 托管文本 Embedding，索引吞吐优先     | 已测试       |
| `qwen/qwen3-vl-embedding`     | 同时检索文本和图片                   | 未测试       |

远程模型的实际延迟、价格和服务端实现可能变化。上表的“已测试”只表示本页对应的
CoIR-ZG 运行已完成，不代表服务商之间的价格或多语言能力排名。

## 如何理解取舍

### Jina Code 与 Gemma

Jina Code 在当前代码检索测试中质量最高，索引也明显快于 Gemma，是代码仓库的优先
选择。Gemma 的优势是多语言通用能力，适合源码、中文说明和其他语言文档混合的仓库。

### MiniLM 与 BGE Small

两者都是 384 维轻量英文模型。BGE 的 Recall@10 略高，但 MiniLM 的 nDCG@10、
Recall@100 和索引速度更好。没有特定偏好时先选择 MiniLM。

### Potion Code

Potion 使用 Model2Vec 静态 token 向量查表，因此建立索引非常快、模型也很小。它适合
临时仓库、低资源设备和强调吞吐的场景；代价是无法像 Transformer 模型一样充分建模
上下文，检索质量低于 Jina、Gemma 和 MiniLM。

### Qwen3 0.6B 本地模型

Qwen3 0.6B 的代码检索质量较高，但模型最大、建索引最慢，峰值内存也最高。除非需要
它的多语言能力并且能够接受资源成本，否则不建议作为默认本地模型。

## 本地与远程

本地模型适合源码不能离开机器、离线使用或希望成本固定的场景：

```bash
zg index --embedding local/jina-embeddings-v2-base-code
```

远程模型适合不想下载本地模型、机器资源有限或已有托管服务额度的场景：

```bash
zg index \
  --embedding qwen/text-embedding-v4 \
  --api-key "$DASHSCOPE_API_KEY"
```

使用远程模型前，请确认仓库内容允许发送给服务商。zvec-grep 会在远程 Embedding
发生前展示授权信息；MCP 工具本身的信任设置不会替代这次数据发送授权。

## 执行设备

本地 Transformer 和 GGUF 模型可以用 `--device` 选择执行设备：

```bash
zg index --embedding local/jina-embeddings-v2-base-code --device auto
```

可选值是 `auto`、`cpu`、`metal`、`vulkan` 和 `cuda`。也可以通过
`ZVEC_GREP_DEVICE` 设置默认设备，或用 `zg config model set <local/model>
--device <device>` 为单个模型保存配置。Potion 属于静态向量查表模型，选择 GPU
不会带来收益。

建议在自己的机器上分别测试 CPU 和加速设备；参数量、量化格式、批量大小和执行后端都会
影响实际速度。

## 输入长度与截断

“最大输入 tokens”针对单个提取后的文本实体。zvec-grep 会参考模型输入上限控制代码
fragment 大小，但字符数与 token 数不存在对所有语言都精确成立的换算关系。

如果最终输入仍然超过模型上限，Embedding provider 会截断输入，并把发生截断的
fragment 数量记录到索引状态中：

```bash
zg status
```

选型或调整仓库过滤规则后，建议确认 `fragmentsTruncated` 是否保持为 0。

## 切换模型

不同模型的向量空间互不兼容，即使维度相同也不能混用。已有索引不传
`--embedding` 时会继续使用原模型：

```bash
zg index
```

切换模型需要明确重建：

```bash
zg index --rebuild --embedding local/jina-embeddings-v2-base-code
```

评估自己的仓库时，至少比较代表性查询的结果、完整索引耗时、峰值内存和索引体积，
不要只根据参数量或公开榜单做决定。
