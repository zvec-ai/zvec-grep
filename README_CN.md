<p align="right">
  <a href="./README.md">English</a> | 中文
</p>

<div align="center">
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
    <a href="#try-it-yourself">🚀 <strong>动手体验</strong></a> |
    <a href="./docs/README.md">📚 <strong>文档</strong></a> |
    <a href="#benchmarks">📊 <strong>性能测试</strong></a> |
    <a href="#community">🤝 <strong>社区</strong></a>
  </p>
</div>

**zg**（**z**vec-**g**rep）将 ripgrep、BM25 与向量检索统一在一个
[本地优先的检索入口](./docs/05-architecture.md)中。既可以由人在终端中搜索，
也可以让 Agent 根据问题选择合适的本地检索方式。

<a id="tour"></a>

## 🎬 功能演示

<div align="center">
  <img src="./.github/assets/zvec-grep-tour.gif" width="1000" alt="安装 Agent 集成、为工作区建索引并让 Agent 使用 zvec-grep 检索本地内容" />
</div>

<a id="features"></a>

## 💫 为什么选择 zg？

- **人与 Agent 开箱即用**：安装一次、索引一次，即可在 macOS、Linux 和
  Windows 上通过 CLI 或 Agent 复用同一个工作区。
- **不止关键词搜索**：先按语义发现内容、按相关性排序，再在需要时使用精确文本
  或正则完成验证。
- **多格式检索**：搜索源代码、文档和结构化数据，同时保留有用的内容结构与
  来源位置。
- **更少搜索，更少上下文**：经过排序并保留来源的结果，可以减少工具调用、
  Token 消耗与无关噪声，更快找到所需证据。
- **默认本地运行**：文件、索引与本地模型都留在本机；只有经过你的授权，远程
  Embedding 服务才能接收数据。

<a id="try-it-yourself"></a>

## 🚀 动手体验

### 1. 准备示例书架

```bash
# 需要 Node.js 22 或更新版本。
npm install -g @zvec/zvec-grep

mkdir zg-mystery && cd zg-mystery
curl --retry 3 --retry-all-errors --progress-bar -fL \
  -o alice-in-wonderland.txt https://raw.githubusercontent.com/GITenberg/Alice-s-Adventures-in-Wonderland_11/master/11.txt \
  -o sherlock-holmes.txt https://raw.githubusercontent.com/GITenberg/The-Memoirs-of-Sherlock-Holmes_834/master/834.txt

zg index --embedding local/potion-retrieval-32m
```

### 2. 选择检索方式

#### Agent：通过 OpenCode 提问

配置好 [OpenCode](https://opencode.ai/) 后：

```bash
zg install --target opencode --yes
opencode run --model opencode/deepseek-v4-flash-free \
  "An unseen creature left a few marks. What did the detective infer? Cite local evidence."
```

Prompt 中没有指定任何工具，OpenCode 会自主选择 zg。

<details>
<summary><strong>展开查看完整 Agent 调用与回答</strong></summary>

```text
⚙ zvec_grep_zvec_grep_search {"root":".../zg-mystery","query":"An unseen creature left a few marks. What did the detective infer?","fts":["marks","unseen creature","inferred","detective"],"fuse":true}
→ Read sherlock-holmes.txt [offset=5440, limit=120]

Answer:
The detective is Sherlock Holmes in *The Adventure of the Crooked Man*.
From the animal's foot-marks, he inferred:

- It was not a dog, cat, monkey, or other familiar creature: its print had
  five foot-pads, long nails, and was nearly as large as a dessert-spoon
  (sherlock-holmes.txt:5479-5486).
- It had a long body and very short legs: nearly two feet long, but with a
  stride of only about three inches (sherlock-holmes.txt:5497-5502).
- It was carnivorous because it climbed the curtain toward a canary's cage
  (sherlock-holmes.txt:5509-5510).
- It was probably from the weasel or stoat tribe, though unusually large
  (sherlock-holmes.txt:5514-5516).

Holmes also inferred that a man entered through the window with it and later
left with the key (sherlock-holmes.txt:5464-5470, 5527-5528).
```

</details>

#### 用户：直接检索

不通过 Agent，直接搜索同一个书架：

```bash
zg query --human "An unseen creature left a few marks. What did the detective infer?" --limit 3
```

zg 会将 `sherlock-holmes.txt` 中的相关段落排在
`alice-in-wonderland.txt` 前面。

<a id="benchmarks"></a>

## 📊 性能测试

zg 通过**缩小有效搜索空间**，帮助 Agent 更快找到相关证据 —— 在保持回答质量的同时，使用**更少的 Token、工具调用和时间**。

每项测试均采用**受控、可复现的配对 A/B 评测**：同一个 Agent 在完全相同的模型、Prompt、环境和资源限制下执行同一组预先固定的任务；**实验组仅额外提供 `zg` 工具及其使用说明**。

### 1. 三个突出代码库任务

以下是在平均 Judge 不下降的任务中，输入 Token 降幅最大的三个
SWE-QA-Bench 任务。数值按 **Baseline → zg（变化）** 展示，每个 Profile
均为三次运行的平均值。

| 任务 | 仓库 | 类型 | 任务主题 | Judge&nbsp;/100&nbsp;↑ | 输入&nbsp;Token&nbsp;↓ | 工具调用&nbsp;↓ | 时间&nbsp;↓ | 成本&nbsp;↓ |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| `pylint:10` | `pylint-dev/pylint` | What · 架构探索 | 通过 AST 节点类型区分带类型标注和不带类型标注的实例属性初始化 | 61.33 → 77.00<br>**+15.67 pp** | 1,379,349 → 239,010<br>**−82.7%** | 54.67 → 9.00<br>**−83.5%** | 286.3s → 69.5s<br>**−75.7%** | $1.894 → $0.544<br>**−71.3%** |
| `matplotlib:37` | `matplotlib/matplotlib` | Where · 数据 / 控制流 | 追踪 `FontInfo` 在数学文本渲染中的流向，以及 `postscript_name` / `FT2Font` 分支 | 83.33 → 86.00<br>**+2.67 pp** | 787,247 → 367,421<br>**−53.3%** | 30.33 → 13.00<br>**−57.1%** | 218.8s → 104.1s<br>**−52.4%** | $1.272 → $0.727<br>**−42.9%** |
| `django:32` | `django/django` | Why · 设计原理 | 解释用户名唯一约束、ORM 事务，以及移除约束对 formset 批量操作的级联影响 | 85.00 → 87.33<br>**+2.33 pp** | 758,941 → 416,109<br>**−45.2%** | 42.00 → 12.67<br>**−69.8%** | 195.8s → 118.6s<br>**−39.4%** | $1.577 → $0.689<br>**−56.3%** |

在这三个任务上，zg 将平均 Judge 提高 **6.89 分**，同时将输入 Token、
工具调用、时间和成本分别降低 **65.0%**、**72.7%**、**58.3%** 和
**58.7%**。三个任务覆盖架构、跨文件数据流和设计原理，都是“定位正确证据”
占主要成本的检索密集场景。该列表是事后挑选的突出样例，不代表无偏总体估计；
其中 `pylint:10` 的 Baseline Judge 波动也明显较大。

### 2. SWE-QA-Bench — 20 个代码库任务

已发布的 [SWE-QA-Bench](./benchmarks/swe-qa-bench/README_CN.md) 测试覆盖
What、Where、How、Why 四个顶层类别、8 种意图和 11 个仓库中的 20 个
检索密集任务。测试使用 Claude Code 与 Claude Opus 5，zg Profile 使用
Qwen3.7 Text Embedding，每个任务和 Profile 各运行三次。

| Profile | Judge&nbsp;/100&nbsp;↑ | 平均输入&nbsp;Token&nbsp;↓ | 平均工具调用&nbsp;↓ | 平均时间&nbsp;↓ | 平均成本&nbsp;↓ |
| --- | ---: | ---: | ---: | ---: | ---: |
| Baseline | 80.42 | 558,651 | 23.42 | 127.5s | $0.905 |
| Baseline + zg | 81.92 | 294,262 | 9.70 | 79.7s | $0.558 |
| **变化** | **+1.50 pp** | **−47.3%** | **−58.6%** | **−37.5%** | **−38.3%** |

在 20 个任务 × 3 次运行中，zg 在提高平均评审质量的同时，显著降低了所有
测量到的资源消耗。这组任务针对检索密集场景进行了筛选，不应被理解为对全部
720 道 SWE-QA-Bench 题目的均匀抽样。

### 3. BrowseComp-Plus — 80 个深度研究样例

[BrowseComp-Plus](./benchmarks/browse-comp-plus/README_CN.md) 在固定的
100,195 文档语料库上评估多文档证据检索。80 个样例的测试使用 Codex
`gpt-5.6-sol`、medium 推理强度，zg Profile 使用 Qwen3.7 Text Embedding，
每个样例运行两次。

| Profile | 准确率&nbsp;↑ | 平均输入&nbsp;Token&nbsp;↓ | 平均工具调用&nbsp;↓ | 平均时间&nbsp;↓ |
| --- | ---: | ---: | ---: | ---: |
| Baseline | 90.00% | 2.04M | 22.70 | 284.8s |
| Baseline + zg | 90.00% | 1.19M | 14.24 | 199.3s |
| **变化** | **0.00 pp** | **−41.7%** | **−37.3%** | **−30.0%** |

zg 在保持 **90.00% 准确率**的同时，减少了跨文档组装证据所需的上下文和
搜索工作量。

完整结果和复现细节参见[性能测试文档](./benchmarks/README_CN.md)。

## 📚 文档

| 指南 | 你可以完成什么 |
| :--- | :--- |
| [Agent 集成](./docs/01-agents.md) | 将 zg 接入 Codex、Claude Code、Cursor 或 OpenCode，并验证是否正常工作。 |
| [CLI 指南](./docs/02-cli.md) | 在终端中搜索、索引和管理本地工作区。 |
| [MCP 指南](./docs/03-mcp.md) | 了解 Agent 可以使用哪些 zg 工具，以及访问权限如何受到保护。 |
| [检索 Pipeline](./docs/04-pipeline.md) | 选择索引范围、保持内容新鲜，并获得更好的检索结果。 |
| [架构](./docs/05-architecture.md) | 了解 zg 如何处理查询，以及数据会保留在哪里。 |
| [服务端与执行模式](./docs/06-server.md) | 在一次性命令和长期运行的本地服务之间选择。 |
| [Embedding 模型](./docs/07-embedding.md) | 根据速度、检索质量、隐私和本机硬件选择合适的模型。 |
| [Roadmap](./docs/08-roadmap.md) | 了解接下来的产品方向，并参与影响 zg 的优先级。 |

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
