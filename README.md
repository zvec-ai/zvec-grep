<p align="right">
  English | <a href="./README_CN.md">中文</a>
</p>

<div align="center">
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
    <a href="#try-it-yourself">🚀 <strong>Try it yourself</strong></a> |
    <a href="./docs/README.md">📚 <strong>Docs</strong></a> |
    <a href="#benchmarks">📊 <strong>Benchmarks</strong></a> |
    <a href="#community">🤝 <strong>Community</strong></a>
  </p>
</div>

**zg** (**z**vec-**g**rep) unifies ripgrep, BM25, and vector search behind
[one local-first interface](./docs/05-architecture.md). Use it directly from the
terminal, or let your agent use it for you.

<a id="tour"></a>

## 🎬 See it in action

<div align="center">
  <img src="./.github/assets/zvec-grep-tour.gif" width="1000" alt="Install the agent integration, index a workspace, and let the agent search it with zvec-grep" />
</div>

<a id="features"></a>

## 💫 Why zg?

- **Ready for humans and agents** — install once, index once, then use the same
  workspace from the CLI or your agent on macOS, Linux, and Windows.
- **Search beyond keywords** — discover by meaning, rank by relevance, then
  verify with exact text or regex when needed.
- **Multi-format search** — search source code, documents, and structured data
  while preserving useful structure and source locations.
- **Less searching, less context** — ranked, source-linked results surface the
  right evidence with fewer tool calls, fewer tokens, and less noise.
- **Local by default** — files, indexes, and local models stay on your machine;
  remote embeddings receive data only with your permission.

<a id="try-it-yourself"></a>

## 🚀 Try it yourself

### 1. Set up a sample bookshelf

```bash
# Requires Node.js 22 or newer.
npm install -g @zvec/zvec-grep

mkdir zg-mystery && cd zg-mystery
curl --retry 3 --retry-all-errors --progress-bar -fL \
  -o alice-in-wonderland.txt https://raw.githubusercontent.com/GITenberg/Alice-s-Adventures-in-Wonderland_11/master/11.txt \
  -o sherlock-holmes.txt https://raw.githubusercontent.com/GITenberg/The-Memoirs-of-Sherlock-Holmes_834/master/834.txt

zg index --embedding local/potion-retrieval-32m
```

### 2. Choose how to search

#### For agents: ask with OpenCode

With [OpenCode](https://opencode.ai/) configured:

```bash
zg install --target opencode --yes
opencode run --model opencode/deepseek-v4-flash-free \
  "An unseen creature left a few marks. What did the detective infer? Cite local evidence."
```

OpenCode chooses zg on its own—the prompt does not name a tool.

<details>
<summary><strong>See the full agent run and answer</strong></summary>

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

#### For humans: search directly

Search the same bookshelf directly, without an agent:

```bash
zg query --human "An unseen creature left a few marks. What did the detective infer?" --limit 3
```

zg returns the relevant passages from `sherlock-holmes.txt`, ranked ahead of
`alice-in-wonderland.txt`.

<a id="benchmarks"></a>

## 📊 Benchmarks

zg helps agents find relevant evidence faster by **narrowing the effective
search space** — using **fewer tokens, tool calls, and less time** while
preserving answer quality.

Each benchmark follows a **controlled, reproducible paired A/B protocol**: the
same agent runs the same pinned tasks with identical model, prompt, environment,
and limits; **only `zg` access and usage guidance differ**.

### 1. Overall retrieval benchmarks

zg is evaluated in two complementary retrieval settings:

- **Coding:** [SWE-QA-Bench](./benchmarks/swe-qa-bench/README.md) covers 20
  retrieval-intensive tasks across What, Where, How, and Why, 8 intentions, and
  11 repositories. It uses Claude Code with Claude Opus 5 and three runs per
  task and profile.
- **General text retrieval:**
  [BrowseComp-Plus](./benchmarks/browse-comp-plus/README.md) covers 80
  deep-research cases over a fixed 100,195-document corpus. It uses Codex
  `gpt-5.6-sol` at medium reasoning effort and two trials per case and profile.

Both zg profiles use Qwen3.7 Text Embedding.

<p align="center">
  <img src="./.github/assets/benchmark-overall-retrieval-indexed-v2.png" alt="Overall zg benchmark results for Coding and general text retrieval, comparing answer quality, input tokens, tool calls, and time against Baseline" width="1200" />
</p>

Across both studies, answer quality was preserved or improved while retrieval
work fell substantially. In Coding, Judge increased from 80.42 to 81.92 while
input tokens, tool calls, and time fell 47.3%, 58.6%, and 37.5%. In general
text retrieval, accuracy held at 90.00% while the same metrics fell 41.7%,
37.3%, and 30.0%.

The SWE-QA-Bench task set was curated for retrieval-intensive scenarios and
should not be interpreted as a uniform sample of all 720 questions. See the
[benchmark documentation](./benchmarks/README.md) for full results and
reproduction details.

### 2. Representative repository cases

These are the three SWE-QA-Bench tasks with the largest input-token reductions
among tasks whose mean Judge score did not decline.

<p align="center">
  <img src="./.github/assets/benchmark-repository-top3-v2.png" alt="Baseline to zg comparison across three repository-comprehension tasks: Judge score, input tokens, tool calls, and wall time" width="1200" />
</p>

The highlighted cases span architecture, cross-file data flow, and design
rationale—the retrieval-heavy situations where locating the right evidence is
often the dominant cost. This is a post-hoc highlight, not an unbiased estimate
of overall performance; the pylint case also had unusually high Baseline Judge
variance.

<details>
<summary><strong>Repository questions</strong></summary>

| Repository | Question type | Question |
| --- | --- | --- |
| **`pylint-dev/pylint`** | What<br>Architecture exploration | What is the architectural pattern that distinguishes type-annotated from non-annotated instance attribute initialization using AST node type separation? |
| **`matplotlib/matplotlib`** | Where<br>Data / Control-flow | Where does the `FontInfo` NamedTuple propagate font metrics and glyph data through the mathematical text rendering pipeline, and what control flow determines whether the `postscript_name` or the `FT2Font` object is used at different stages of character rendering? |
| **`django/django`** | Why<br>Design rationale | Why does the User model's unique constraint on the username field interact with Django's ORM transaction handling, and what cascading effects would occur if this constraint were removed on an existing database with formset-based bulk operations? |

</details>

## 📚 Documentation

| Guide | What you can do |
| :--- | :--- |
| [Agent integrations](./docs/01-agents.md) | Connect zg to Codex, Claude Code, Cursor, or OpenCode and verify that it works. |
| [CLI guide](./docs/02-cli.md) | Search, index, and manage your local workspaces from the terminal. |
| [MCP guide](./docs/03-mcp.md) | Understand which zg tools your agent can use and how access is secured. |
| [Retrieval pipeline](./docs/04-pipeline.md) | Choose what to index, keep it fresh, and get better search results. |
| [Architecture](./docs/05-architecture.md) | See how zg handles your query and where your data stays. |
| [Server and execution modes](./docs/06-server.md) | Choose between one-off commands and a long-running local server. |
| [Embedding models](./docs/07-embedding.md) | Pick the right model for speed, search quality, privacy, and your hardware. |
| [Roadmap](./docs/08-roadmap.md) | See what is coming next and help shape zg's priorities. |

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
