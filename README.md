<p align="right">
  English | <a href="./README_CN.md">中文</a>
</p>

<div align="center">
  <p>
    <a href="./docs/08-roadmap.md"><img src="https://img.shields.io/badge/status-work%20in%20progress-F59E0B?style=for-the-badge" alt="Work in progress" /></a>
  </p>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="./.github/assets/zg-logo-dark.svg">
    <img src="./.github/assets/zg-logo.svg" width="150" alt="zg logo" />
  </picture>
  <p><strong>Know the words—or don’t. Just zg.</strong></p>
  <p>The local-first search layer for humans and agents.</p>

  <p>
    <a href="https://www.npmjs.com/package/@zvec/zvec-grep"><img src="https://img.shields.io/npm/v/@zvec/zvec-grep.svg" alt="npm version" /></a>
    <a href="https://github.com/zvec-ai/zvec-grep/actions/workflows/ci.yml"><img src="https://github.com/zvec-ai/zvec-grep/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
    <a href="./LICENSE"><img src="https://img.shields.io/badge/license-Apache%202.0-blue.svg" alt="Apache 2.0 license" /></a>
    <img src="https://img.shields.io/badge/node-%3E%3D22-blue.svg" alt="Node.js 22 or newer" />
  </p>

  <p>
    <a href="#tour">🎬 <strong>Tour</strong></a> |
    <a href="#features">💫 <strong>Features</strong></a> |
    <a href="#quickstart">🚀 <strong>Quickstart</strong></a> |
    <a href="./docs/README.md">📚 <strong>Docs</strong></a> |
    <a href="#benchmarks">📊 <strong>Benchmarks</strong></a> |
    <a href="#community">🤝 <strong>Community</strong></a>
  </p>
</div>

**zg** (**z**vec-**g**rep) unifies ripgrep, BM25, and vector search behind
[one local-first interface](./docs/05-architecture.md). Use it directly from the
terminal, or let your agent use it for you.

zvec-grep is under active development. The [Roadmap](./docs/08-roadmap.md)
covers richer multimodal data, stronger retrieval, simpler setup, and expansion
from desktop to mobile.

<a id="tour"></a>

## 🎬 See it in action

<div align="center">
  <img src="./.github/assets/zvec-grep-tour.gif" width="1000" alt="Install the agent integration, index a repository, and let the agent search it with zvec-grep" />
</div>

Install the integration once, index the workspace, then search from the terminal
or ask your agent naturally.

<a id="features"></a>

## 💫 Why zg?

- **Works out of the box** — one guided install connects zg to your agent on
  macOS, Linux, or Windows; ask naturally, with no search syntax to learn.
- **All-in-one retrieval layer** — semantic discovery, BM25-ranked retrieval,
  exact text, and regex search all stay behind zg.
- **Agent- and human-friendly** — compact, file-oriented context for agents
  and readable terminal output for people, with less noise for both.
- **Local first, permission aware** — ripgrep, indexes, and local models stay
  on your machine; remote embeddings require explicit authorization.

<a id="quickstart"></a>

## 🚀 Quickstart

Requires Node.js 22 or newer on macOS, Linux, or Windows.

### 1. Install and connect your agent

```bash
npm install -g @zvec/zvec-grep
zg install
```

`zg install` detects Codex, Claude Code, Cursor, and OpenCode and configures the
local MCP integration. See [Agent integrations](./docs/01-agents.md) for managed
configuration, permissions, and uninstall instructions. You can also select one
explicitly:

```bash
zg install --target codex --yes
```

### 2. Index your repository

```bash
cd your-repository
zg index --embedding local/potion-code-16m-v2
```

This quickstart uses the lightweight
[Potion Code v2](./docs/07-embedding.md) model so you can build the first index
quickly. The first run downloads it; the index stays in `.zvec-grep/`, and later
updates only need `zg index`. See the
[retrieval pipeline](./docs/04-pipeline.md#indexing) to control scope, updates, and
rebuilds.

### 3. Ask your agent

```text
My app forgets dark mode every time I refresh. Find out why.
```

The agent stays within zg for semantic discovery, ranked keyword retrieval, and
exhaustive exact matching. You do not need to choose or invoke another search
tool. The [MCP guide](./docs/03-mcp.md) describes the two tools exposed to agents.

To search directly from the terminal, use the same local layer. See the
[CLI guide](./docs/02-cli.md) for routes, filters, and output controls.

```bash
zg query --human "theme preference persistence on startup" --limit 3
```

<a id="benchmarks"></a>

## 📊 Benchmarks

> 🚧 **Work in progress.**

<a id="community"></a>

## 🤝 Join Our Community

<div align="center">

| 💬 DingTalk | 📱 WeChat | 🎮 Discord | X (Twitter) |
| :---: | :---: | :---: | :---: |
| <img src="https://zvec.oss-cn-hongkong.aliyuncs.com/qrcode/dingding.png" width="150" alt="DingTalk QR Code"/> | <img src="https://zvec.oss-cn-hongkong.aliyuncs.com/qrcode/wechat.png?v1" width="150" alt="WeChat QR Code"/> | [![Discord](https://img.shields.io/badge/Discord-Join%20Server-5865F2?style=for-the-badge&logo=discord&logoColor=white)](https://discord.gg/rKddFBBu9z) | [![X (formerly Twitter) Follow](https://img.shields.io/twitter/follow/ZvecAI)](<https://x.com/ZvecAI>) |
| Scan to join | Scan to join | Click to join | Click to follow |

</div>

## ❤️ Contributing

Community contributions are always welcome—bug fixes, features, and
documentation improvements all help make zvec-grep better.

Check out our [Contributing Guide](./CONTRIBUTING.md) to get started!
