# Rust CLI 与 Node.js 展示对齐

Node.js 的 `src/cli/format/status.ts` 是工作区状态、索引完成摘要和远程授权状态的展示基准。本次对齐 Rust 的这组管理命令；不改变索引行为、授权校验或命令退出条件。

## 命令梳理

| 命令 | Node.js 展示 | Rust 对齐范围 |
| --- | --- | --- |
| `zg --status`，direct/server/auto | 状态图标和标题；工作区路径；分组字段；覆盖率进度条 | 统一标题、缩进、留白、数字分组、路径和颜色；两种传输复用同一渲染器 |
| `zg --index`、`--rebuild` | `Workspace index` 标题；`files`、`entities`、`duration`、`roots` 摘要 | 统一完成摘要及按字段着色；失败信息继续可见 |
| `zg --index --drop` | `Dropped index for …` 或 `No index found for …` | 统一结果文案 |
| `zg --auth status`、`grant` | 授权状态标题；`Authorization`、`Storage` 分组 | 统一分组、目标、端点主机、授权文件路径和颜色 |
| `zg --auth revoke` | 已撤销授权数量，或没有授权 | 统一结果文案，保留重复撤销的幂等行为 |
| `zg --server on/off/status` | `Server:`、`PID:`、`URL:`、`MCP toolset:` | 已符合 Node.js 风格，保留原有展示 |
| `zg --config provider/model set` | 配置类型及引用；`Global config:` 路径 | 已符合 Node.js 风格，保留原有展示 |
| 索引搜索 | `--preview none/short/full/detail`，终端和管道统一默认 `short` | 四档布局、详细字段和结构摘要对齐；direct/server 共用渲染器 |
| `--rg` | 固定简洁布局，使用 `-A/-B/-C` 控制上下文 | 不接受索引搜索的 `--preview`，不再根据终端切换布局 |
| `zg --help`、`--version` | 纯文本帮助及版本号 | 保留纯文本；帮助内容继续反映各实现实际支持的选项 |

## 状态布局约定

```text
✓ Workspace index is ready
  ~/workspace/example

  Coverage    ████████████████████ 100%  1,132 / 1,132 files
  Entities    22,037
  Source size 4,177,103 bytes
  Queue       0 pending · 0 failed

  Embedding   qwen/text-embedding-v4
              1,024 dimensions · cosine
  FTS         tokenizer=jieba filters=lowercase

  Storage     .zvec-grep/generations/…/storage
  Version     2
  Nested Git  included
```

- 标题表达当前状态，路径单独一行；字段缩进 2 个空格，标签占 12 列，续行与值对齐。
- 标签和零值诊断使用暗色；路径用青色；就绪用绿色；待更新和待处理用黄色；失败用红色。标题和普通字段不再统一涂成青色。
- 覆盖率为 `files_unchanged / files_scanned`。已修改但尚未重新索引的文件不计入已完成数量，避免旧索引显示为 100%。
- 未完成时，百分比最多为 99%，20 格进度条最多填满 19 格；只有全部扫描文件完成时才显示 100% 和满格。空扫描保持 0%。
- 索引完成摘要读取保存后的工作区元数据，`roots` 包含实际生效的 glob、文件类型、深度、大小及其他扫描规则；省略参数时保留的规则和 `--reset-paths` 的结果都会反映在摘要中。
- 工作区内的存储路径显示相对路径，用户主目录前缀显示 `~`，计数使用英文千位分隔符。
- `--color always` 显式开启颜色，`never` / `--no-color` 关闭颜色；`auto` 仅在终端且未设置 `NO_COLOR` 时开启。重定向保留分组信息但默认不输出 ANSI 控制码。
- `--status --check-ready` 仍依据引擎健康状态决定成功或失败，不根据展示文案判断。

## 保留的实现差异

Rust `IndexStats` 没有 Node.js 的截断片段计数，因此显示已有的 `Source size`，不伪造 `Truncated: 0`。Rust 特有的索引版本兼容性、需要重建的原因和操作建议必须保留，使用同一分组字段风格展示。

Rust 的工作区状态回复没有 Node.js 服务端回复中的实时任务状态（queued/running/cancelled）和运行中完成率；本次不通过修改文案假装具有这些信息。未扫描的未知状态、未配置、索引缺失、禁用、待更新、失败和需要重建仍保持各自语义。

授权展示只消费授权校验结果，不改变签名验证、作用域匹配或异常的退出行为。端点仅在展示时简写为主机及端口；保存的目标和授权范围不变。
授权模块作为 `ZvecGrep` 操作边界的明确例外，其范围和约束见 `rust/CONTRIBUTING.md`。授权提示和状态展示共用端点主机格式化逻辑。

索引搜索使用 `--preview` 统一控制展示，移除 `--compact` 和内部 `human` 开关。`detail` 展示查询组、文件分组、元数据、Outline、Matched 和完整 Source；其余三档采用简洁布局。TTY 仅影响自动颜色选择。

Rust 的 Outline 从索引保存的源码生成，包含有界的调用列表或直接成员摘要，不读取当前磁盘源码，也不改变检索和排序。可选元数据缺失时省略；Rust 当前没有 Node.js 的 `modifiers` 列表，不伪造该字段。公开 MCP 的 `short/full` 参数保持不变。

## 回归检查位置

- Node.js 基准：`test/unit/cli-format.test.mjs` 中工作区状态、覆盖率、索引摘要和授权状态测试。
- 搜索共享基准：`rust/compat/cli-search-presentation.json`；Node.js 格式测试与 Rust `search_presentation.rs` 分别验证四档输出和 TTY 一致性。
- Rust 状态格式：`rust/crates/zg-cli/src/status.rs` 中的单元测试。
- Rust 命令行为：`rust/crates/zg/tests/implicit_index.rs` 覆盖无索引、就绪、待更新、颜色参数、路径简写和 `--check-ready`；`server_lifecycle.rs` 覆盖服务端状态入口。
- Rust 索引与授权命令：`rust/crates/zg/tests/index_progress.rs`、`auth.rs`。

开发 CI 按项目约定仅在 `Cuiyus/zvec-grep` fork 触发。
