<p align="right">
  <a href="./README.md">English</a> | 中文
</p>

<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://zvec.oss-cn-hongkong.aliyuncs.com/logo/github_log_2.svg" />
    <img src="https://zvec.oss-cn-hongkong.aliyuncs.com/logo/github_logo_1.svg" width="400" alt="zvec logo" />
  </picture>
</div>

<p align="center">
  <a href="https://www.npmjs.com/package/@zvec/zvec-grep"><img src="https://img.shields.io/npm/v/@zvec/zvec-grep.svg" alt="npm 版本"/></a>
  <a href="https://github.com/zvec-ai/zvec-grep/actions/workflows/ci.yml"><img src="https://github.com/zvec-ai/zvec-grep/actions/workflows/ci.yml/badge.svg" alt="CI"/></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-Apache%202.0-blue.svg" alt="许可证"/></a>
  <img src="https://img.shields.io/badge/node-%3E%3D22-blue.svg" alt="Node.js >=22"/>
  <img src="https://img.shields.io/badge/CLI-zg-2ea44f.svg" alt="zg CLI"/>
</p>

<p align="center">
  <a href="#quickstart">🚀 <strong>快速开始</strong></a> |
  <a href="#features">💫 <strong>核心特性</strong></a> |
  <a href="#installation">📦 <strong>安装</strong></a> |
  <a href="#models">🧠 <strong>模型</strong></a> |
  <a href="#library-api">🛠️ <strong>库 API</strong></a>
</p>

**zvec-grep** 是基于 Zvec 构建的、面向 agent 的代码库混合检索工具。它为仓库维护本地语义索引，融合向量检索与全文检索，并默认输出节省 token 的结果；当人在终端里阅读时，也可以切换到更完整的展示模式。

命令非常短：

```bash
zg query "where query auto update happens"
```

> [!IMPORTANT]
> **命令接口**
>
> - **混合代码检索**：可以用自然语言、精确关键词，或两者组合来搜索代码。
> - **明确的索引生命周期**：新仓库必须显式运行 `zg index --embedding <model>`；agent 不会静默创建索引。
> - **刷新控制**：使用 `--refresh background|wait|off`。Server 默认 `background`，Direct 默认 `off`。
> - **节省 Token 的输出**：agent 默认输出 `--preview none`；`--human` 默认展示完整源码 preview。
> - **无索引文本搜索**：`zg query --rg` 提供托管的 ripgrep 搜索，不需要先建索引。

## <a id="features"></a>💫 核心特性

- **语义 + 词法检索**：融合向量检索和全文检索，适合源码、文档、测试、脚本和配置。
- **仓库本地索引**：匿名索引存放在 `<repo>/.zvec-grep/`，索引状态跟随仓库。
- **Agent 友好输出**：默认按文件分组，并尽量减少源码 preview，降低上下文成本。
- **人类阅读模式**：加 `--human` 后更适合终端阅读，并默认展示完整 preview。
- **托管 ripgrep 通道**：`zg query --rg` 支持常见 `rg` 参数，未建索引仓库也能使用。
- **显式模型选择**：第一次建索引必须指定模型，例如 `local/embeddinggemma-300m`、`local/qwen3-embedding-0.6b` 或 `qwen/text-embedding-v4`。
- **Schema 复用**：已有索引再次运行 `zg index` 会复用保存的 embedding schema，除非你显式切换模型。
- **聚焦的 MCP Server**：运行 `zg server on` 后，默认只向 Agent 暴露索引检索和托管 ripgrep；索引生命周期与诊断由 CLI 通过内部管理端点完成。
- **库 API**：Node.js 工具、agent 或 MCP server 可以直接使用 `createZvecGrep()`。

## <a id="installation"></a>📦 安装

从 npm 安装 CLI：

```bash
npm install -g @zvec/zvec-grep
zg version
```

也可以不全局安装，直接运行：

```bash
npx @zvec/zvec-grep help
```

从最新源码构建，并将 `zg` 安装为全局命令：

```bash
git clone https://github.com/zvec-ai/zvec-grep.git
cd zvec-grep
npm ci
npm run build
npm install -g .
zg version
```

为检测到的 Claude Code 和 Codex 配置集成：

```bash
zg install
```

为 Codex、Claude Code、OpenCode 或 Cursor 安装 MCP 集成：

```bash
zg install --target codex --yes
zg install --target claude --yes
zg install --target opencode --yes
zg install --target cursor --yes
```

交互式安装会检测支持的 Agent 并安装所选集成。Codex 和 Claude Code 还会写入
托管指导配置，并预批准本机 `zvec_grep` MCP 工具。MCP 信任与 Remote Embedding
授权相互独立：向远程 Provider 发送查询文本或工作区内容前，zg 仍会单独请求授权。
Codex MCP 工具调用和 OpenCode MCP 初始化默认超时为 600 秒，可通过
`--mcp-tool-timeout <秒数>` 覆盖。本地 server 默认只监听 loopback 且不启用 token。
如需 Bearer 鉴权，请在 install 前设置 `ZVEC_GREP_SERVER_TOKEN`，并传入
`--mcp-token-env ZVEC_GREP_SERVER_TOKEN`。MCP URL 为
`http://127.0.0.1:7999/mcp`，默认暴露 `zvec_grep_search` 和
`zvec_grep_rg`。索引生命周期与 daemon 诊断继续通过 `zg` 命令提供，CLI 会在内部使用
保留的 `/mcp/admin` 端点。若已有客户端需要从公开端点访问全部六个 MCP 工具，可重启
daemon 并执行 `zg server on --mcp-toolset full`。环境变量
`ZVEC_GREP_MCP_TOOLSET=full` 可作为回退，显式命令行参数优先。
`zg server status` 会显示当前 toolset。使用 `zg server off` 停止 daemon。

CLI 的索引检索和建索引命令支持 `--mode direct`、`--mode server` 和 `--mode auto`。默认模式为 `auto`：只在 daemon ready 时使用 server，否则在提交请求前回退 Direct。
Daemon 以 JSON Lines 写日志到 `~/.zvec-grep/daemon/logs/server.log`，不会记录凭证或完整查询文本。

### ✅ 运行要求

- Node.js 22 或更新版本
- macOS、Linux 或 Windows
- 使用索引检索时需要选择一个支持的 embedding 模型

`zg query --rg` 不需要 embedding 模型，也不需要索引。无论当前是 Direct、Server 还是 Auto 模式，它都始终在本地执行，不会停止 daemon，也不会访问索引 writer。

## <a id="quickstart"></a>⚡ 快速开始

为仓库建索引，并显式指定 embedding 模型：

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

查看索引状态：

```bash
zg status
```

用自然语言搜索：

```bash
zg query "where query auto update happens"
```

组合语义意图和精确锚点：

```bash
zg query "GPU fallback" --fts "usingCpuFallback" -g "src/**" -t ts --limit 5
```

不依赖索引，做穷尽文本搜索：

```bash
zg query --rg -F "ZVEC_GREP_HOME" src
```

切换到适合人看的输出：

```bash
zg query --human "root local index discovery" --limit 3
```

默认公开 MCP toolset 包含 `zvec_grep_search` 和 `zvec_grep_rg`。MCP 输入使用
JSON 友好的字段，例如 `globs: ["src/**"]`。索引生命周期与诊断请使用
`zg index`、`zg status` 和 `zg server status`。installer 会为 Codex、Claude
Code、OpenCode 和 Cursor 写入用户级 MCP 配置；Codex 和 Claude Code 还会获得托管
指导与本机工具信任配置。`cc` 和 `claude-code` 仍可作为正式 target `claude` 的兼容别名。

## <a id="models"></a>🧠 模型

本地模型通过 `node-llama-cpp`、Transformers.js 或原生 Model2Vec Safetensors adapter
运行，适合把代码检索留在本机。
可以参考[本地 Embedding 模型选型指南](docs/embedding.md)，按使用场景比较
模型大小、上下文长度和兼容性。

```bash
zg index --embedding local/embeddinggemma-300m
zg index --embedding local/qwen3-embedding-0.6b
zg index --embedding local/jina-embeddings-v2-base-code
zg index --embedding local/multilingual-e5-small
zg index --embedding local/potion-base-8m
zg index --embedding local/potion-code-16m-v2
```

在 Apple Silicon 上，本地构建默认使用更安静的 llama.cpp CMake 配置，避免无害的 OpenMP 和 ARM native 探测 warning。可以通过 `NODE_LLAMA_CPP_CMAKE_OPTION_<name>` 覆盖任意 llama.cpp CMake 选项，例如设置 `NODE_LLAMA_CPP_CMAKE_OPTION_GGML_NATIVE=ON` 重新启用 native CPU 优化。

远程 Qwen embedding 适合希望使用托管 embedding 服务，或不想在本机配置模型的场景：

```bash
zg index \
  --embedding qwen/text-embedding-v4 \
  --api-key "$DASHSCOPE_API_KEY"
```

索引成功后，通过 CLI 显式传入的全局模型和 provider 参数会保存到 `~/.zvec-grep/config.json`，并使用仅当前用户可读写的权限。已经运行的 `zg` MCP server 会在下一次需要模型时读取新的远程 API key，不需要重启 Codex。只从环境变量读取、没有作为 CLI 参数显式传入的值不会被持久化。

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

本地 embedding 会按 `models["provider/model"]` 为每个模型分别选择 GPU 和并行度；daemon 建索引和搜索都会使用这份配置，切换运行配置不需要重建已有索引。显式 CLI 或库参数优先于模型配置，模型配置优先于全局默认值和环境变量回退。远程 embedding 会忽略本地 GPU 配置。仓库 root 和 include/exclude 规则仍保存在各仓库自己的 `.zvec-grep` 元数据中，不进入全局配置。

也可以通过命令配置，无需手工编辑 JSON：

```bash
zg config model set local/embeddinggemma-300m --llama-gpu metal --embedding-parallelism 2
zg config model set local/qwen3-embedding-0.6b --no-gpu --embedding-parallelism 1
```

对于已有索引，`zg index` 不传 `--embedding` 会复用索引里保存的 schema。只有在你明确想切换模型时，才使用 `--rebuild --embedding <model>`：

```bash
zg index --rebuild --embedding local/qwen3-embedding-0.6b
```

## 🔎 查询模式

多个带引号的 query 会作为多个搜索组分别检索：

```bash
zg query "request validation" "error handling" --limit 5
```

每个 route 参数只接收一个 query，并且可以重复使用。需要把所有搜索组合并成一个排序结果时，使用 `--fuse`：

```bash
zg query \
  --hybrid "authentication flow" \
  --fts "AuthService" \
  --vector "where credentials are validated" \
  --fuse \
  --limit 10
```

尽早使用与 ripgrep 一致的路径和文件类型过滤，让结果保持聚焦。普通 glob 表示包含，以 `!` 开头的 glob 表示排除：

```bash
zg query "cache invalidation" \
  -g "src/**" \
  -g "!test/**" \
  -g "!tests/**" \
  -g "!fixtures/**" \
  -g "!dist/**" \
  -t ts
```

文件类型过滤会在 glob 过滤之后继续缩小范围。例如，`-g "docs/**" -t ts`
只会选择 `docs` 目录下的 TypeScript 文件，而不是该目录里的所有文件。

使用 `--preview` 控制索引结果的源码展示量：

```bash
zg query "plugin lifecycle" --preview none
zg query "plugin lifecycle" --preview short --limit 5
zg query "plugin lifecycle" --preview full --limit 2
```

查精确文本、符号、配置项或错误码时，使用 `--fts` 或 `--rg`：

```bash
zg query "authentication flow" --fts "AuthService" --fts "ForbiddenError"
zg query --rg -i -C 2 -g "*.ts" "needle text" src
```

托管的 rg 模式支持常用的 ripgrep 匹配、上下文、文件类型、glob、ignore、
深度、大小、符号链接、编码和正则引擎参数。结果格式由 zvec-grep 管理，
因此 `--json`、`--count`、`--files`、`-l`、`-o`、`--replace` 和
`--vimgrep` 等会改变输出形态的模式会被拒绝。`.git/**` 和
`.zvec-grep/**` 始终保持排除。

## <a id="library-api"></a>🛠️ 库 API

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

显式进行无索引文本搜索：

```ts
const result = await zvecGrep.context({
  query: "ZVEC_GREP_HOME",
  rg: true,
});
```

## 🤝 加入 Zvec 社区

<div align="center">

|                                                💬 钉钉群                                                |                                                 📱 微信群                                                 |                                                                       🎮 Discord                                                                        |                                             X (Twitter)                                              |
| :-----------------------------------------------------------------------------------------------------: | :-------------------------------------------------------------------------------------------------------: | :-----------------------------------------------------------------------------------------------------------------------------------------------------: | :--------------------------------------------------------------------------------------------------: |
| <img src="https://zvec.oss-cn-hongkong.aliyuncs.com/qrcode/dingding.png" width="150" alt="钉钉二维码"/> | <img src="https://zvec.oss-cn-hongkong.aliyuncs.com/qrcode/wechat.png?v=6" width="150" alt="微信二维码"/> | [![Discord](https://img.shields.io/badge/Discord-Join%20Server-5865F2?style=for-the-badge&logo=discord&logoColor=white)](https://discord.gg/rKddFBBu9z) | [![X (formerly Twitter) Follow](https://img.shields.io/twitter/follow/ZvecAI)](https://x.com/ZvecAI) |
|                                                扫码加入                                                 |                                                 扫码加入                                                  |                                                                        点击加入                                                                         |                                               点击关注                                               |

</div>

## ❤️ 参与贡献

欢迎提交 issue 和 pull request。请保持改动聚焦；如果改变行为，请补充测试，并运行：

```bash
npm run check
```

Pull request 标题需遵循 Conventional Commits。
