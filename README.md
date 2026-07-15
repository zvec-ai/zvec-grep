<p align="right">
  English | <a href="./README_CN.md">中文</a>
</p>

<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://zvec.oss-cn-hongkong.aliyuncs.com/logo/github_log_2.svg" />
    <img src="https://zvec.oss-cn-hongkong.aliyuncs.com/logo/github_logo_1.svg" width="400" alt="zvec logo" />
  </picture>
</div>

<p align="center">
  <a href="https://www.npmjs.com/package/@zvec/zvec-grep"><img src="https://img.shields.io/npm/v/@zvec/zvec-grep.svg" alt="npm Release"/></a>
  <a href="https://github.com/zvec-ai/zvec-grep/actions/workflows/ci.yml"><img src="https://github.com/zvec-ai/zvec-grep/actions/workflows/ci.yml/badge.svg" alt="CI"/></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-Apache%202.0-blue.svg" alt="License"/></a>
  <img src="https://img.shields.io/badge/node-%3E%3D22-blue.svg" alt="Node.js >=22"/>
  <img src="https://img.shields.io/badge/CLI-zg-2ea44f.svg" alt="zg CLI"/>
</p>

<p align="center">
  <a href="#quickstart">🚀 <strong>Quickstart</strong></a> |
  <a href="#features">💫 <strong>Features</strong></a> |
  <a href="#installation">📦 <strong>Installation</strong></a> |
  <a href="#models">🧠 <strong>Models</strong></a> |
  <a href="#library-api">🛠️ <strong>Library API</strong></a>
</p>

**zvec-grep** is an agent-friendly hybrid code search tool built on Zvec. It gives repositories a root-local semantic index, combines vector search with full-text search, and keeps CLI output compact enough for AI agents while still offering rich terminal output for humans.

The command is intentionally short:

```bash
zg "where query auto update happens"
```

> [!IMPORTANT]
> **v0.1.4**
>
> - **Hybrid Code Search**: Query code with natural language, exact terms, or both in one command.
> - **Explicit Index Lifecycle**: New repositories require `zg --index --embedding <model>`; agents do not silently create indexes.
> - **Automatic Refresh**: Existing anonymous indexes are checked and incrementally updated before normal queries.
> - **Token-Efficient Output**: Agent output defaults to `--preview none`; `--human` defaults to full source previews.
> - **No-Index Lexical Search**: `zg --rg` provides managed ripgrep search without requiring an index.

## <a id="features"></a>💫 Features

- **Semantic + Lexical Retrieval**: Blend vector search and full-text search for code, docs, tests, scripts, and configuration.
- **Root-Local Indexes**: Anonymous indexes live under `<repo>/.zvec-grep/`, so repository state stays with the repository.
- **Agent-Ready Output**: Default output is grouped by file and keeps previews small to save tokens.
- **Human Output Mode**: Add `--human` for a terminal-friendly summary with full previews by default.
- **Managed ripgrep Route**: `zg --rg` supports common `rg` flags and works even before a repository is indexed.
- **Explicit Model Choice**: The first index build requires a model such as `local/embeddinggemma-300m`, `local/qwen3-embedding-0.6b`, or `qwen/text-embedding-v4`.
- **Schema Reuse**: Re-running `zg --index` on an existing index reuses the stored embedding schema unless you explicitly change it.
- **MCP Server**: Run `zg serve --mcp` to expose indexed and no-index lexical search tools to MCP clients. Indexing and status remain CLI-only operations.
- **Library API**: Use `createZvecGrep()` directly from Node.js tools, agents, or MCP servers.

## <a id="installation"></a>📦 Installation

Install the CLI from npm:

```bash
npm install -g @zvec/zvec-grep
zg --version
```

Or run it without a global install:

```bash
npx @zvec/zvec-grep --help
```

Install the latest source checkout as a global `zg` command:

```bash
git clone https://github.com/zvec-ai/zvec-grep.git
cd zvec-grep
npm ci
npm run build
npm install -g .
zg --version
```

Run the stdio MCP server:

```bash
zg serve --mcp
```

Install the Codex MCP integration:

```bash
zg install --target codex --yes
```

Codex MCP tool calls default to a 600-second timeout. Override it during installation with `--mcp-tool-timeout <seconds>`.

### ✅ Requirements

- Node.js 22 or newer
- macOS, Linux, or Windows
- A supported embedding model for indexed search

`zg --rg` works without any embedding model or index.

## <a id="quickstart"></a>⚡ Quickstart

Index a repository with an explicit embedding model:

```bash
zg --index \
  --embedding local/embeddinggemma-300m \
  --include "src/**" \
  --include "docs/**" \
  --include "test/**" \
  --exclude "dist/**,node_modules/**,coverage/**"
```

Check index state:

```bash
zg --status
```

Search with natural language:

```bash
zg "where query auto update happens"
```

Combine semantic intent with exact anchors:

```bash
zg "GPU fallback" --fts "usingCpuFallback" --include "src/**" --limit 5
```

Use exhaustive lexical search without an index:

```bash
zg --rg -F "ZVEC_GREP_HOME" src
```

Switch to human-readable output:

```bash
zg --human "root local index discovery" --limit 3
```

Use from an MCP client with `zvec_grep_search` and `zvec_grep_rg`. MCP inputs use JSON-friendly fields such as `include: ["src/**"]`; use the CLI for `zg --status` and `zg --index`. The Codex installer writes managed zvec-grep blocks to `${CODEX_HOME:-$HOME/.codex}/config.toml` and `${CODEX_HOME:-$HOME/.codex}/AGENTS.md`.

## <a id="models"></a>🧠 Models

Local models run through `node-llama-cpp` and keep code search private to your machine:

```bash
zg --index --embedding local/embeddinggemma-300m
zg --index --embedding local/qwen3-embedding-0.6b
```

On Apple Silicon, local builds use quiet llama.cpp CMake defaults to avoid harmless OpenMP and ARM native-detection warnings. Override any llama.cpp CMake option with `NODE_LLAMA_CPP_CMAKE_OPTION_<name>`, for example `NODE_LLAMA_CPP_CMAKE_OPTION_GGML_NATIVE=ON` to opt back into native CPU tuning.

Remote Qwen embeddings are useful when you prefer a managed embedding service or want to avoid local model setup:

```bash
zg --index \
  --embedding qwen/text-embedding-v4 \
  --api-key "$DASHSCOPE_API_KEY"
```

After a successful index, explicitly passed global model and provider options are saved to `~/.zvec-grep/config.json` with user-only permissions. This lets an already-running `zg` MCP server load a newly configured remote API key on its next model request without restarting Codex. Values read only from environment variables are not persisted.

```json
{
  "version": 1,
  "defaults": {
    "embedding": "qwen/text-embedding-v4"
  },
  "providers": {
    "qwen": {
      "apiKey": "...",
      "endpoint": "https://dashscope.aliyuncs.com/compatible-mode/v1/embeddings"
    }
  }
}
```

Resolution order is explicit CLI or library options, then environment variables, then global config, then built-in defaults. Repository roots and include/exclude filters remain in each repository's `.zvec-grep` metadata rather than global config.

For existing indexes, `zg --index` without `--embedding` reuses the stored schema. Use `--rebuild --embedding <model>` only when you intentionally want to rebuild with a different model:

```bash
zg --index --rebuild --embedding local/qwen3-embedding-0.6b
```

## 🔎 Query Patterns

Multiple quoted queries are treated as separate search groups:

```bash
zg "request validation" "error handling" --limit 5
```

Use path filters early to keep results focused:

```bash
zg "cache invalidation" \
  --include "src/**" \
  --exclude "test/**,tests/**,fixtures/**,dist/**"
```

Use `--preview` to control indexed source previews:

```bash
zg "plugin lifecycle" --preview none
zg "plugin lifecycle" --preview short --limit 5
zg "plugin lifecycle" --preview full --limit 2
```

For exact text, symbols, flags, or error codes, use `--fts` or `--rg`:

```bash
zg "authentication flow" --fts "AuthService" "ForbiddenError"
zg --rg -i -C 2 -g "*.ts" "needle text" src
```

## <a id="library-api"></a>🛠️ Library API

```ts
import { createZvecGrep } from "@zvec/zvec-grep";

const zvecGrep = await createZvecGrep({
  root: process.cwd(),
});

const result = await zvecGrep.context({
  query: "ranking implementation",
  limit: 5,
});

for (const item of result.items) {
  console.log(`${item.file.relativePath}:${item.range.startLine}`);
}

await zvecGrep.close();
```

For explicit no-index lexical search:

```ts
const result = await zvecGrep.context({
  query: "ZVEC_GREP_HOME",
  rg: true,
});
```

## 🤝 Join the Zvec Community

<div align="center">

|                                                  💬 DingTalk                                                  |                                                   📱 WeChat                                                   |                                                                       🎮 Discord                                                                        |                                             X (Twitter)                                              |
| :-----------------------------------------------------------------------------------------------------------: | :-----------------------------------------------------------------------------------------------------------: | :-----------------------------------------------------------------------------------------------------------------------------------------------------: | :--------------------------------------------------------------------------------------------------: |
| <img src="https://zvec.oss-cn-hongkong.aliyuncs.com/qrcode/dingding.png" width="150" alt="DingTalk QR Code"/> | <img src="https://zvec.oss-cn-hongkong.aliyuncs.com/qrcode/wechat.png?v=6" width="150" alt="WeChat QR Code"/> | [![Discord](https://img.shields.io/badge/Discord-Join%20Server-5865F2?style=for-the-badge&logo=discord&logoColor=white)](https://discord.gg/rKddFBBu9z) | [![X (formerly Twitter) Follow](https://img.shields.io/twitter/follow/ZvecAI)](https://x.com/ZvecAI) |
|                                                 Scan to join                                                  |                                                 Scan to join                                                  |                                                                      Click to join                                                                      |                                           Click to follow                                            |

</div>

## ❤️ Contributing

Issues and pull requests are welcome. Please keep changes focused, add tests for behavior changes, and run:

```bash
npm run check
```

Pull request titles use Conventional Commits syntax.
