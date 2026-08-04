# Embedding models

[Documentation](./README.md) · [Agents](./01-agents.md) ·
[CLI](./02-cli.md) · [MCP](./03-mcp.md) · [Pipeline](./04-pipeline.md) ·
[Architecture](./05-architecture.md) · [Server](./06-server.md) ·
[Embedding](./07-embedding.md) · [Roadmap](./08-roadmap.md)

The Embedding model determines the vector representation used by indexed
search. It affects language coverage, memory use, index size, input length, and
indexing speed. A new index needs an explicit model, an environment default, or
a configured default:

```bash
zg index --embedding local/potion-code-16m-v2
```

Local models keep workspace content and query text on the machine. Their files
are downloaded on first use and cached under `~/.zvec-grep/models` by default.
Remote models avoid local inference but send disclosed query or workspace
content to the configured provider after authorization.

## Quick selection

| Need | Start with | Why |
| --- | --- | --- |
| A fast first index for a code repository | `local/potion-code-16m-v2` | Small static Model2Vec model with an 8,192-token input limit |
| A Transformer model specialized for code | `local/jina-embeddings-v2-base-code` | Code-oriented, multilingual, and long-context |
| General multilingual code and documents | `local/embeddinggemma-300m` | Broad language coverage in a local GGUF model |
| A smaller multilingual model | `local/multilingual-e5-small` | Compact 384-dimensional Transformer model |
| A lightweight English model | `local/all-minilm-l6-v2` | Small local model for short English text |
| Long English documents | `local/gte-modernbert-base` or `local/nomic-embed-text-v1.5` | 8,192-token local context |
| No local model runtime | `qwen/qwen3.7-text-embedding` | Managed text Embedding API |
| Text and image retrieval | `qwen/qwen3-vl-embedding` | Managed multimodal Embedding API |

The best model still depends on the repository and its real queries. Start with
the smallest model that covers the required languages and input length, then
compare representative results before committing to a larger model.

## Supported models

The input limit applies to each extracted entity or fragment, not the entire
file.

| Model | Runtime | Max input tokens | Dimensions |
| --- | --- | ---: | ---: |
| `local/potion-code-16m-v2` | Model2Vec FP16 | 8,192 | 256 |
| `local/potion-base-8m` | Model2Vec FP32 | 512 | 256 |
| `local/all-minilm-l6-v2` | ONNX Q4 | 256 | 384 |
| `local/bge-small-en-v1.5` | ONNX Q4 | 512 | 384 |
| `local/multilingual-e5-small` | ONNX Q8 | 512 | 384 |
| `local/jina-embeddings-v2-base-code` | ONNX Q8 | 8,192 | 768 |
| `local/gte-modernbert-base` | ONNX Q4 | 8,192 | 768 |
| `local/nomic-embed-text-v1.5` | ONNX Q4 | 8,192 | 768 |
| `local/embeddinggemma-300m` | GGUF Q8_0 | 2,048 | 768 |
| `local/qwen3-embedding-0.6b` | GGUF Q8_0 | 8,192 | 1,024 |
| `qwen/text-embedding-v4` | Remote text | 8,192 | 1,024 |
| `qwen/qwen3.7-text-embedding` | Remote text | 128,000 | 1,024 |
| `qwen/qwen3-vl-embedding` | Remote multimodal | 32,000 | 2,560 |

All catalog entries currently use cosine similarity. Exact model revisions are
pinned by zvec-grep so the same reference resolves consistently for a given
release.

## Configure a default model

Set a default for new indexes:

```bash
zg config model set local/potion-code-16m-v2 --default
zg index
```

`ZVEC_GREP_EMBEDDING` provides an environment-level default instead:

```bash
export ZVEC_GREP_EMBEDDING=local/potion-code-16m-v2
zg index
```

An existing index always reuses its stored provider, model, dimensions, and
metric unless a rebuild explicitly changes them.

## Local runtime and device

Select a device for local Transformer and GGUF models:

```bash
zg index \
  --embedding local/jina-embeddings-v2-base-code \
  --device auto
```

Supported values are `auto`, `cpu`, `metal`, `vulkan`, and `cuda`. Save a model
preference globally with:

```bash
zg config model set local/jina-embeddings-v2-base-code --device metal
```

The equivalent environment override is `ZVEC_GREP_DEVICE`. Model2Vec models
such as Potion use static vector lookup, so selecting a GPU does not improve
their runtime.

Override the download cache with `--model-cache` or `ZVEC_GREP_MODEL_CACHE`:

```bash
zg index \
  --embedding local/potion-code-16m-v2 \
  --model-cache /path/to/model-cache
```

## Remote Embedding and authorization

Configure the Qwen provider credential and, optionally, a model endpoint:

```bash
zg config provider set qwen --api-key "$DASHSCOPE_API_KEY"
zg config model set qwen/text-embedding-v4 --default
```

One-off values can be passed directly or through `ZVEC_GREP_API_KEY` and
`ZVEC_GREP_ENDPOINT`:

```bash
zg index \
  --embedding qwen/text-embedding-v4 \
  --api-key "$DASHSCOPE_API_KEY" \
  --allow-remote
```

Credentials configure access to a provider; they do not authorize data
transfer. `--allow-remote` authorizes Remote Embedding only for the current
command. To create a signed Workspace grant shared by the CLI and MCP server:

```bash
zg auth grant \
  --capability embedding \
  --scope workspace \
  --embedding qwen/text-embedding-v4

zg auth status
zg auth revoke
```

Before granting access, confirm that the workspace content is permitted to be
sent to the selected provider and endpoint. MCP tool approval is separate from
this data authorization.

## Input length and truncation

zvec-grep uses the selected model's input limit when it creates fragments, but
characters and tokens do not map one-to-one across languages. If an extracted
input still exceeds the model limit, the provider truncates it and the index
records the count.

Inspect the index after changing models or file scope:

```bash
zg status
```

Prefer a model with a larger input limit or narrow the indexed content when
`truncated_fragments` is unexpectedly high.

## Change models

Vector spaces from different models are incompatible, even when their
dimensions match. Rebuild explicitly when changing models:

```bash
zg index --rebuild --embedding local/jina-embeddings-v2-base-code
```

Changing a remote endpoint also requires a rebuild because the endpoint is part
of the stored index schema. API key and local device changes affect runtime and
do not require rebuilding the vectors.
