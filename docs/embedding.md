# 本地 Embedding 模型选型指南

选择 `local/` 模型时，zvec-grep 会完全在本机生成向量，不会把源码或查询发送给远程
Embedding 服务。模型会在第一次使用时自动下载，并缓存到
`~/.zvec-grep/models`。

## 快速选择

如果不想逐个比较，可以从下面的推荐开始：

| 使用场景             | 建议首先尝试                         | 原因                                                  |
| -------------------- | ------------------------------------ | ----------------------------------------------------- |
| 快速索引英文仓库     | `local/all-minilm-l6-v2`             | 下载体积小，并且是当前 zvec-grep 基准中最快的本地模型 |
| 英文语义搜索         | `local/bge-small-en-v1.5`            | 紧凑的通用检索模型                                    |
| 源码和技术文档       | `local/jina-embeddings-v2-base-code` | 针对代码、docstring 和技术问答训练                    |
| 轻量多语言搜索       | `local/multilingual-e5-small`        | 支持 94 种语言，比 EmbeddingGemma 和 Qwen3 更轻量     |
| 通用多语言搜索       | `local/embeddinggemma-300m`          | 语言覆盖广，适合通用场景                              |
| 英文长文档           | `local/gte-modernbert-base`          | 8,192-token 上下文和局部注意力架构                    |
| 更高容量的多语言搜索 | `local/qwen3-embedding-0.6b`         | 当前最大的本地模型，支持 8,192-token 上下文           |
| 768 维英文长文本     | `local/nomic-embed-text-v1.5`        | 8,192-token 上下文和检索任务前缀                      |

最早接入的四个模型已经在本仓库中做过性能测试。E5、Jina、GTE 和 Nomic 已完成
集成支持和测试，但在将其作为默认模型前，仍建议使用自己的仓库和真实查询进行基准测试。

## 完整对比

下载大小是 zvec-grep 固定版本所使用的模型权重近似大小。完整缓存还会包含 tokenizer
和配置文件，体积会略大一些。

| 模型                                 | 主要用途             | 语言                 | 参数量 | 模型格式         | 约下载大小 | 最大输入 tokens | 向量维度 |
| ------------------------------------ | -------------------- | -------------------- | -----: | ---------------- | ---------: | --------------: | -------: |
| `local/all-minilm-l6-v2`             | 快速英文搜索         | 英文                 |  22.7M | ONNX Q4          |      55 MB |             256 |      384 |
| `local/potion-base-8m`               | 极低延迟英文搜索     | 英文                 |     8M | Safetensors FP32 |      30 MB |             512 |      256 |
| `local/potion-code-16m-v2`           | 极低延迟代码检索     | 英文及 6 种编程语言  |  16.2M | Safetensors FP16 |      33 MB |           8,192 |      256 |
| `local/bge-small-en-v1.5`            | 轻量英文检索         | 英文                 |  33.4M | ONNX Q4          |      61 MB |             512 |      384 |
| `local/multilingual-e5-small`        | 轻量多语言搜索       | 94 种语言            |   118M | ONNX Q8          |     118 MB |             512 |      384 |
| `local/nomic-embed-text-v1.5`        | 英文长文本           | 英文                 |   137M | ONNX Q4          |     165 MB |           8,192 |      768 |
| `local/gte-modernbert-base`          | 英文长文档           | 英文                 |   149M | ONNX Q4          |     224 MB |           8,192 |      768 |
| `local/jina-embeddings-v2-base-code` | 代码搜索             | 英文及 30 种编程语言 |   161M | ONNX Q8          |     162 MB |           8,192 |      768 |
| `local/embeddinggemma-300m`          | 通用多语言搜索       | 100 多种语言         |   308M | GGUF Q8_0        |     334 MB |           2,048 |      768 |
| `local/qwen3-embedding-0.6b`         | 更高容量的多语言搜索 | 100 多种语言         |   600M | GGUF Q8_0        |     639 MB |           8,192 |    1,024 |

最大输入长度针对每个提取后的文本实体，并不意味着 zvec-grep 一定会生成这么大的
文本块。实际进入模型的上下文仍取决于文件解析和切分策略。

索引代码文件时，zvec-grep 会按更保守的 `1 token ≈ 2 chars` 把模型的最大输入
tokens 换算成 CodeExtractor 的字符上限，近似控制过大代码实体的 fragment 大小。
这不等同于精确的 token 计数；本地 embedding provider 会记录实际发生截断的
fragment 数量，并在 `zg status` 中显示。普通文本和 Markdown 仍使用各自的结构与
字符切分策略。

## 使用方法

第一次建立索引时选择模型：

```bash
zg index --embedding local/jina-embeddings-v2-base-code
```

在支持的环境中启用本地 GPU 加速：

```bash
zg index --embedding local/jina-embeddings-v2-base-code --gpu
```

### 为什么开启 GPU 后仍可能使用 CPU

`--gpu` 表示优先尝试 GPU，但最终能否加速还取决于模型和本机推理环境是否兼容。

模型内部包含类型转换、矩阵乘法和 Attention 等许多计算步骤。ONNX Runtime 需要为
每一步提供对应的 GPU 实现；如果当前 WebGPU 后端暂时不支持其中某一步，整个模型就
无法在 GPU 上正确完成推理，但通常仍然可以在 CPU 上运行。

在 Apple Silicon 上，zvec-grep 当前使用的调用路径是：

```text
Transformers.js -> ONNX Runtime WebGPU -> Metal
```

这类问题属于推理引擎与模型计算图之间的兼容性问题，不代表模型文件损坏、Apple GPU
性能不足，也不代表 Metal 不能运行 Embedding 模型。

当前 Apple Silicon 实测结果如下：

| 模型                                 | 观察到的 WebGPU 行为                                                             |
| ------------------------------------ | -------------------------------------------------------------------------------- |
| `local/multilingual-e5-small`        | 量化图产生非有限值（`NaN`）；Q4F16 图还依赖 WebGPU 不支持的 `Cast(int64)` kernel |
| `local/jina-embeddings-v2-base-code` | 量化图产生非有限值；FP16 图还依赖 WebGPU 不支持的 `Cast(int64)` kernel           |
| `local/gte-modernbert-base`          | Q4 `MatMulNBits` 算子出现维度不匹配；Q4F16 图也受到 `Cast(int64)` 限制           |
| `local/nomic-embed-text-v1.5`        | 旋转位置编码中的 `Mul` 算子发生广播维度错误；Q4F16 图也受到 `Cast(int64)` 限制   |

遇到这些情况时，zvec-grep 会自动处理：

```text
尝试使用 GPU
    -> GPU 初始化失败、推理报错或输出异常
    -> 输出警告并释放 GPU 模型
    -> 使用 CPU 重新加载同一个模型
    -> 继续索引或查询
```

在当前 Apple Silicon 环境中，MiniLM 和 BGE 可以通过 WebGPU 加速；E5、Jina、GTE
和 Nomic 会自动回退 CPU。CUDA、DirectML、不同版本的 ONNX Runtime 或不同的模型
导出方式可能会有不同结果。自动回退可以保证命令继续完成，但回退后不再是 GPU 加速。
Potion 使用 Model2Vec 静态向量查表，`--gpu` 对它不起作用。

如果索引已经存在，不传 `--embedding` 会继续使用索引中保存的模型。切换模型时需要
显式重建：

```bash
zg index --rebuild --embedding local/multilingual-e5-small --gpu
```

## 模型兼容性

不同模型生成的向量不能混用，即使向量维度相同也不兼容。例如 MiniLM 和 BGE 都输出
384 维向量，但它们使用不同的向量空间。zvec-grep 会把模型记录在索引 schema 中，
切换模型时要求重新建立索引。

## 性能建议

参数量不能单独决定索引速度。模型架构、输入长度、批量大小、量化方式以及 CPU/GPU
执行后端都会影响性能。8,192 tokens 只是容量上限，不代表长输入能和短代码实体一样快。

在生产仓库中，建议至少比较：

- 索引总耗时和每秒处理实体数；
- 模型预热后的查询延迟；
- CPU 和 GPU 峰值内存；
- 代表性查询的检索质量；
- 向量维度带来的索引大小差异。
