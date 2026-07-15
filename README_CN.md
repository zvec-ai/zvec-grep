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
zg "where query auto update happens"
```

> [!IMPORTANT]
> **v0.1.4**
>
> - **混合代码检索**：可以用自然语言、精确关键词，或两者组合来搜索代码。
> - **明确的索引生命周期**：新仓库必须显式运行 `zg --index --embedding <model>`；agent 不会静默创建索引。
> - **自动刷新**：已有匿名索引会在普通查询前自动检查并增量更新。
> - **节省 Token 的输出**：agent 默认输出 `--preview none`；`--human` 默认展示完整源码 preview。
> - **无索引文本搜索**：`zg --rg` 提供托管的 ripgrep 搜索，不需要先建索引。

## <a id="features"></a>💫 核心特性

- **语义 + 词法检索**：融合向量检索和全文检索，适合源码、文档、测试、脚本和配置。
- **仓库本地索引**：匿名索引存放在 `<repo>/.zvec-grep/`，索引状态跟随仓库。
- **Agent 友好输出**：默认按文件分组，并尽量减少源码 preview，降低上下文成本。
- **人类阅读模式**：加 `--human` 后更适合终端阅读，并默认展示完整 preview。
- **托管 ripgrep 通道**：`zg --rg` 支持常见 `rg` 参数，未建索引仓库也能使用。
- **显式模型选择**：第一次建索引必须指定模型，例如 `local/embeddinggemma-300m`、`local/qwen3-embedding-0.6b` 或 `qwen/text-embedding-v4`。
- **Schema 复用**：已有索引再次运行 `zg --index` 会复用保存的 embedding schema，除非你显式切换模型。
- **MCP Server**：运行 `zg serve --mcp`，向 MCP 客户端暴露索引搜索和无索引文本搜索工具；建索引和状态查看仍由 CLI 提供。
- **库 API**：Node.js 工具、agent 或 MCP server 可以直接使用 `createZvecGrep()`。

## <a id="installation"></a>📦 安装

从 npm 安装 CLI：

```bash
npm install -g @zvec/zvec-grep
zg --version
```

也可以不全局安装，直接运行：

```bash
npx @zvec/zvec-grep --help
```

从最新源码构建，并将 `zg` 安装为全局命令：

```bash
git clone https://github.com/zvec-ai/zvec-grep.git
cd zvec-grep
npm ci
npm run build
npm install -g .
zg --version
```

运行 stdio MCP server：

```bash
zg serve --mcp
```

安装 Codex MCP 集成：

```bash
zg install --target codex --yes
```

Codex MCP 工具调用默认超时为 600 秒，可在安装时通过 `--mcp-tool-timeout <秒数>` 覆盖。

### ✅ 运行要求

- Node.js 22 或更新版本
- macOS、Linux 或 Windows
- 使用索引检索时需要选择一个支持的 embedding 模型

`zg --rg` 不需要 embedding 模型，也不需要索引。

## <a id="quickstart"></a>⚡ 快速开始

为仓库建索引，并显式指定 embedding 模型：

```bash
zg --index \
  --embedding local/embeddinggemma-300m \
  --include "src/**" \
  --include "docs/**" \
  --include "test/**" \
  --exclude "dist/**,node_modules/**,coverage/**"
```

查看索引状态：

```bash
zg --status
```

用自然语言搜索：

```bash
zg "where query auto update happens"
```

组合语义意图和精确锚点：

```bash
zg "GPU fallback" --fts "usingCpuFallback" --include "src/**" --limit 5
```

不依赖索引，做穷尽文本搜索：

```bash
zg --rg -F "ZVEC_GREP_HOME" src
```

切换到适合人看的输出：

```bash
zg --human "root local index discovery" --limit 3
```

在 MCP 客户端中可使用 `zvec_grep_search` 和 `zvec_grep_rg`。MCP 输入使用 JSON 友好的字段，例如 `include: ["src/**"]`；状态查看和建索引请使用 CLI 的 `zg --status` 与 `zg --index`。Codex installer 会向 `${CODEX_HOME:-$HOME/.codex}/config.toml` 和 `${CODEX_HOME:-$HOME/.codex}/AGENTS.md` 写入由 zvec-grep 管理的配置块。

## <a id="models"></a>🧠 模型

本地模型通过 `node-llama-cpp` 运行，适合把代码检索留在本机：

```bash
zg --index --embedding local/embeddinggemma-300m
zg --index --embedding local/qwen3-embedding-0.6b
```

在 Apple Silicon 上，本地构建默认使用更安静的 llama.cpp CMake 配置，避免无害的 OpenMP 和 ARM native 探测 warning。可以通过 `NODE_LLAMA_CPP_CMAKE_OPTION_<name>` 覆盖任意 llama.cpp CMake 选项，例如设置 `NODE_LLAMA_CPP_CMAKE_OPTION_GGML_NATIVE=ON` 重新启用 native CPU 优化。

远程 Qwen embedding 适合希望使用托管 embedding 服务，或不想在本机配置模型的场景：

```bash
zg --index \
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
  }
}
```

配置优先级为：显式 CLI 或库参数、环境变量、全局配置、内置默认值。仓库 root 和 include/exclude 规则仍保存在各仓库自己的 `.zvec-grep` 元数据中，不进入全局配置。

对于已有索引，`zg --index` 不传 `--embedding` 会复用索引里保存的 schema。只有在你明确想切换模型时，才使用 `--rebuild --embedding <model>`：

```bash
zg --index --rebuild --embedding local/qwen3-embedding-0.6b
```

## 🔎 查询模式

多个带引号的 query 会作为多个搜索组分别检索：

```bash
zg "request validation" "error handling" --limit 5
```

尽早使用路径过滤，让结果保持聚焦：

```bash
zg "cache invalidation" \
  --include "src/**" \
  --exclude "test/**,tests/**,fixtures/**,dist/**"
```

使用 `--preview` 控制索引结果的源码展示量：

```bash
zg "plugin lifecycle" --preview none
zg "plugin lifecycle" --preview short --limit 5
zg "plugin lifecycle" --preview full --limit 2
```

查精确文本、符号、配置项或错误码时，使用 `--fts` 或 `--rg`：

```bash
zg "authentication flow" --fts "AuthService" "ForbiddenError"
zg --rg -i -C 2 -g "*.ts" "needle text" src
```

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
  console.log(`${item.file.relativePath}:${item.range.startLine}`);
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

Pull request 标题需遵循 Conventional Commits。Release Please 会维护发布 PR；
合并审核通过的发布 PR 后会创建版本 tag 和 GitHub Release，npm production
环境还需要维护者进行最终审批。
