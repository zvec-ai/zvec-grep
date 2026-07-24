![WIP](https://img.shields.io/badge/status-WIP-orange)

**zvec-grep** is an agent-friendly hybrid code search tool built on Zvec. It gives repositories a root-local semantic index, combines vector search with full-text search, and keeps CLI output compact enough for AI agents while still offering rich terminal output for humans.

The command is intentionally short:

```bash
zg query "where query auto update happens"
```

> [!IMPORTANT]
> **Command interface**
>
> - **Hybrid Code Search**: Query code with natural language, exact terms, or both in one command.
> - **Explicit Index Lifecycle**: New repositories require `zg index --embedding <model>`; agents do not silently create indexes.
> - **Refresh Control**: Use `--refresh background|wait|off`. Server defaults to `background`; Direct defaults to `off`.
> - **Token-Efficient Output**: Agent output defaults to `--preview none`; `--human` defaults to full source previews.
> - **No-Index Lexical Search**: `zg query --rg` provides managed ripgrep search without requiring an index.

## <a id="features"></a>💫 Features

- **Semantic + Lexical Retrieval**: Blend vector search and full-text search for code, docs, tests, scripts, and configuration.
- **Root-Local Indexes**: Anonymous indexes live under `<repo>/.zvec-grep/`, so repository state stays with the repository.
- **Agent-Ready Output**: Default output is grouped by file and keeps previews small to save tokens.
- **Human Output Mode**: Add `--human` for a terminal-friendly summary with full previews by default.
- **Managed ripgrep Route**: `zg query --rg` supports common `rg` flags and works even before a repository is indexed.
- **Explicit Model Choice**: The first index build requires a model such as `local/embeddinggemma-300m`, `local/qwen3-embedding-0.6b`, or `qwen/text-embedding-v4`.
- **Schema Reuse**: Re-running `zg index` on an existing index reuses the stored embedding schema unless you explicitly change it.
- **Shared MCP Server**: Run `zg server on` to expose indexed search, managed ripgrep, indexing, index-status, and server-status tools over loopback Streamable HTTP.
- **Library API**: Use `createZvecGrep()` directly from Node.js tools, agents, or MCP servers.

## <a id="installation"></a>📦 Installation

Install the CLI from npm:

```bash
npm install -g @zvec/zvec-grep
zg version
```

Or run it without a global install:

```bash
npx @zvec/zvec-grep help
```

Install the latest source checkout as a global `zg` command:

```bash
git clone https://github.com/zvec-ai/zvec-grep.git
cd zvec-grep
npm ci
npm run build
npm install -g .
zg version
```

Configure the detected Claude Code and Codex integrations:

```bash
zg install
```

Install the MCP integration for Codex, Claude Code, OpenCode, or Cursor:

```bash
zg install --target codex --yes
zg install --target claude --yes
zg install --target opencode --yes
zg install --target cursor --yes
```

Interactive setup detects supported agents and installs the selected integration.
Codex and Claude Code also receive managed guidance and pre-approval for the local
`zvec_grep` MCP tools. MCP trust and Remote Embedding authorization are separate:
zg still asks before query text or workspace content is sent to a remote provider.
Codex MCP tool calls and OpenCode MCP initialization default to a 600-second
timeout; override it with `--mcp-tool-timeout <seconds>`. The local server only
listens on loopback and has no token by default. To require Bearer authentication,
set `ZVEC_GREP_SERVER_TOKEN` before install and pass
`--mcp-token-env ZVEC_GREP_SERVER_TOKEN`. The MCP URL is
`http://127.0.0.1:7999/mcp`; stop the daemon with `zg server off`.

CLI indexed queries and index commands can use `--mode direct`, `--mode server`, or `--mode auto`. The default is `auto`: it uses the daemon only when it is ready and otherwise falls back before submitting a request.
Daemon logs are written as JSON lines to `~/.zvec-grep/daemon/logs/server.log`; credentials and complete query text are not recorded.

### ✅ Requirements

- Node.js 22 or newer
- macOS, Linux, or Windows
- A supported embedding model for indexed search

`zg query --rg` works without any embedding model or index. It always runs locally, regardless of Direct, Server, or Auto mode, and does not stop the daemon or access the index writer.

## <a id="quickstart"></a>⚡ Quickstart

Index a repository with an explicit embedding model:

```bash
zg index \
  --embedding local/embeddinggemma-300m \
  -g "src/**" \
  -g "docs/**" \
  -g "test/**" \
  -g "!dist/**" \
  -g "!node_modules/**" \
  -g "!coverage/**"
```

Check index state:

```bash
zg status
```

Search with natural language:

```bash
zg query "where query auto update happens"
```

Combine semantic intent with exact anchors:

```bash
zg query "GPU fallback" --fts "usingCpuFallback" -g "src/**" -t ts --limit 5
```

Use exhaustive lexical search without an index:

```bash
zg query --rg -F "ZVEC_GREP_HOME" src
```

Switch to human-readable output:

```bash
zg query --human "root local index discovery" --limit 3
```

Use the MCP tools `zvec_grep_search`, `zvec_grep_rg`, `zvec_grep_index`, `zvec_grep_index_drop`, `zvec_grep_index_status`, and `zvec_grep_server_status`. MCP inputs use JSON-friendly fields such as `globs: ["src/**"]`. The installer writes user-level MCP configuration for Codex, Claude Code, OpenCode, and Cursor. Codex and Claude Code also receive managed guidance and local tool trust configuration. `cc` and `claude-code` remain accepted aliases for the canonical `claude` target.

## <a id="models"></a>🧠 Models

Local models run through `node-llama-cpp`, Transformers.js, or the native
Model2Vec Safetensors adapter and keep code search private to your machine. See the
[local embedding model guide](docs/embedding.md) for a scenario-based
selection table, model sizes, context limits, and compatibility notes.

```bash
zg index --embedding local/embeddinggemma-300m
zg index --embedding local/qwen3-embedding-0.6b
zg index --embedding local/jina-embeddings-v2-base-code
zg index --embedding local/multilingual-e5-small
zg index --embedding local/potion-base-8m
zg index --embedding local/potion-code-16m-v2
```

On Apple Silicon, local builds use quiet llama.cpp CMake defaults to avoid harmless OpenMP and ARM native-detection warnings. Override any llama.cpp CMake option with `NODE_LLAMA_CPP_CMAKE_OPTION_<name>`, for example `NODE_LLAMA_CPP_CMAKE_OPTION_GGML_NATIVE=ON` to opt back into native CPU tuning.

Remote Qwen embeddings are useful when you prefer a managed embedding service or want to avoid local model setup:

```bash
zg index \
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
  },
  "models": {
    "local/embeddinggemma-300m": {
      "llamaGpu": "metal",
      "embeddingParallelism": 2
    },
    "local/qwen3-embedding-0.6b": {
      "llamaGpu": false,
      "embeddingParallelism": 1
    }
  }
}
```

For local embeddings, `models["provider/model"]` selects GPU and parallelism independently for each model; these settings apply to both daemon indexing and searching, and do not require rebuilding an existing index. Explicit CLI or library options override model settings, which override global defaults and environment fallback. Remote embeddings ignore the local GPU settings. Repository roots and include/exclude filters remain in each repository's `.zvec-grep` metadata rather than global config.

Configure the same settings without editing JSON:

```bash
zg config model set local/embeddinggemma-300m --llama-gpu metal --embedding-parallelism 2
zg config model set local/qwen3-embedding-0.6b --no-gpu --embedding-parallelism 1
```

For existing indexes, `zg index` without `--embedding` reuses the stored schema. Use `--rebuild --embedding <model>` only when you intentionally want to rebuild with a different model:

```bash
zg index --rebuild --embedding local/qwen3-embedding-0.6b
```

## 🔎 Query Patterns

Multiple quoted queries are treated as separate search groups:

```bash
zg query "request validation" "error handling" --limit 5
```

Route options each consume one query and can be repeated. Add `--fuse` when
you want all groups combined into one ranked result list:

```bash
zg query \
  --hybrid "authentication flow" \
  --fts "AuthService" \
  --vector "where credentials are validated" \
  --fuse \
  --limit 10
```

Use ripgrep-style path and file-type filters early to keep results focused.
Positive globs include paths and globs beginning with `!` exclude them:

```bash
zg query "cache invalidation" \
  -g "src/**" \
  -g "!test/**" \
  -g "!tests/**" \
  -g "!fixtures/**" \
  -g "!dist/**" \
  -t ts
```

File-type filters are applied in addition to glob filters. For example,
`-g "docs/**" -t ts` selects TypeScript files under `docs`, not every file in
that directory.

Use `--preview` to control indexed source previews:

```bash
zg query "plugin lifecycle" --preview none
zg query "plugin lifecycle" --preview short --limit 5
zg query "plugin lifecycle" --preview full --limit 2
```

For exact text, symbols, flags, or error codes, use `--fts` or `--rg`:

```bash
zg query "authentication flow" --fts "AuthService" --fts "ForbiddenError"
zg query --rg -i -C 2 -g "*.ts" "needle text" src
```

Managed rg mode accepts common ripgrep matching, context, type, glob, ignore,
depth, size, symlink, encoding, and regex-engine options. Zvec-grep owns the
result format, so output-changing modes such as `--json`, `--count`, `--files`,
`-l`, `-o`, `--replace`, and `--vimgrep` are rejected. `.git/**` and
`.zvec-grep/**` remain excluded.

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
  const location =
    item.range.kind === "text"
      ? `${item.file.relativePath}:${item.range.startLine}`
      : item.file.relativePath;
  console.log(location);
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
