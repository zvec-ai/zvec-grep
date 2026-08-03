<p align="right">
  <a href="./README.md">English</a> | 中文
</p>

<div align="center">
  <p>
    <img src="https://img.shields.io/badge/status-work%20in%20progress-F59E0B?style=for-the-badge" alt="项目开发中" />
  </p>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="./.github/assets/zg-logo-dark.svg">
    <img src="./.github/assets/zg-logo.svg" width="150" alt="zg logo" />
  </picture>
  <p><strong>Know the words—or don’t. Just zg.</strong></p>
  <p>面向人与 Agent 的本地优先统一检索层。</p>

  <p>
    <a href="https://www.npmjs.com/package/@zvec/zvec-grep"><img src="https://img.shields.io/npm/v/@zvec/zvec-grep.svg" alt="npm 版本" /></a>
    <a href="https://github.com/zvec-ai/zvec-grep/actions/workflows/ci.yml"><img src="https://github.com/zvec-ai/zvec-grep/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
    <a href="./LICENSE"><img src="https://img.shields.io/badge/license-Apache%202.0-blue.svg" alt="Apache 2.0 许可证" /></a>
    <img src="https://img.shields.io/badge/node-%3E%3D22-blue.svg" alt="Node.js 22 或更新版本" />
  </p>

  <p>
    <a href="#tour">🎬 <strong>功能演示</strong></a> |
    <a href="#features">💫 <strong>核心特性</strong></a> |
    <a href="#quickstart">🚀 <strong>快速开始</strong></a> |
    <a href="#benchmarks">📊 <strong>性能测试</strong></a> |
    <a href="#community">🤝 <strong>社区</strong></a>
  </p>
</div>

**zg**（**z**vec-**g**rep）将 ripgrep、BM25 与向量检索统一在一个本地优先
的入口中。无论是人在终端中搜索，还是让 Agent 自主检索，只用 zg 即可。

<a id="tour"></a>

## 🎬 功能演示

<div align="center">
  <img src="./.github/assets/zvec-grep-tour.gif" width="1000" alt="安装 Agent 集成、为仓库建索引并让 Agent 使用 zvec-grep 搜索代码" />
</div>

只需安装一次集成并为工作区建索引，之后既可以在终端中搜索，也可以直接向
Agent 提问。

<a id="features"></a>

## 💫 为什么选择 zg？

- **开箱即用，零学习成本**：一套引导式安装即可在 macOS、Linux 和 Windows
  上接入 Agent；直接用自然语言提问，无需学习搜索语法。
- **一站式检索层**：语义发现、BM25 相关性检索、精确文本与正则搜索
  都由 zg 统一提供。
- **对 Agent 和开发者都友好**：为 Agent 提供紧凑、围绕文件组织的上下文，
  为开发者提供易读的终端输出，双方都能减少无关噪声。
- **本地优先、权限明确**：ripgrep、索引与本地模型均留在本机；远程 Embedding
  需要显式授权。

<a id="quickstart"></a>

## 🚀 快速开始

需要 Node.js 22 或更新版本，支持 macOS、Linux 和 Windows。

### 1. 安装并连接 Agent

```bash
npm install -g @zvec/zvec-grep
zg install
```

`zg install` 会检测 Codex、Claude Code、Cursor 和 OpenCode，并配置本地 MCP 集成。
也可以明确指定目标：

```bash
zg install --target codex --yes
```

### 2. 为仓库建索引

```bash
cd your-repository
zg index --embedding local/potion-code-16m-v2
```

快速开始使用轻量的 Potion Code v2，以便快速完成首次建索引。第一次运行会下载
模型，索引保存在 `.zvec-grep/` 下；后续更新只需运行 `zg index`。

### 3. 直接向 Agent 提问

```text
每次刷新页面，应用都会忘记深色模式。帮我找出原因。
```

Agent 会始终通过 zg 完成语义发现、关键词相关性检索与穷尽精确匹配。你无需
选择或调用其他搜索工具。

也可以直接在终端中使用同一个本地检索层：

```bash
zg query --human "theme preference persistence on startup" --limit 3
```

<a id="benchmarks"></a>

## 📊 性能测试

> 🚧 **正在完善中。**

<a id="community"></a>

## 🤝 加入社区

<div align="center">

| 💬 钉钉群 | 📱 微信群 | 🎮 Discord | X (Twitter) |
| :---: | :---: | :---: | :---: |
| <img src="https://zvec.oss-cn-hongkong.aliyuncs.com/qrcode/dingding.png" width="150" alt="钉钉二维码"/> | <img src="https://zvec.oss-cn-hongkong.aliyuncs.com/qrcode/wechat.png?v1" width="150" alt="微信二维码"/> | [![Discord](https://img.shields.io/badge/Discord-Join%20Server-5865F2?style=for-the-badge&logo=discord&logoColor=white)](https://discord.gg/rKddFBBu9z) | [![X (formerly Twitter) Follow](https://img.shields.io/twitter/follow/ZvecAI)](<https://x.com/ZvecAI>) |
| 扫码加入 | 扫码加入 | 点击加入 | 点击关注 |

</div>

## ❤️ 参与贡献

始终欢迎社区贡献——缺陷修复、新功能和文档改进都会让 zvec-grep 变得更好。

请查阅我们的[贡献指南](./CONTRIBUTING.md)开始参与！
