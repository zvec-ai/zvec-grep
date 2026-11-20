# 在个人 fork 中运行 Rust SWE-QA Benchmark

[SWE-QA 工作流](../../../.github/workflows/swe-qa-bench.yml)使用同一份选定的 **Rust zvec-grep 构建包**，比较 OpenCode 的 baseline 与 zg 方案；它不会拿随 benchmark 检出的 TypeScript 包进行测试。`candidate_ref` 可指定 Rust 分支、标签或提交，默认 `main`。工作流解析出固定的提交 SHA，并在每个任务开始前校验 npm 包哈希。工作流只能手动触发；原始触发者及重新运行 job 的用户都必须拥有 fork 的 `admin` 或 `maintain` 角色。

1. Fork `zvec-ai/zvec-grep` 并克隆自己的 fork，在 fork 中启用 GitHub Actions。先将此工作流放到 fork 的**默认分支**；GitHub 要求手动触发的工作流文件存在于默认分支。之后可以用 `--ref` 选择其他 benchmark harness 分支，再用独立的 `candidate_ref` 选择要构建的 Rust 源码。该 ref 必须存在于 fork 且包含 `rust/`。
2. 在百炼创建可调用所选 `glm-5.2` 或 `qwen3.8-max` 模型的业务空间 API Key。在 fork 的 **Settings → Secrets and variables → Actions** 中新增仓库 secret `GLM_API_KEY`。两个模型沿用这一已有名称，执行与评审共用该密钥。不要把密钥提交到仓库或写入命令行。使用 GitHub CLI 时可执行 `gh secret set GLM_API_KEY --repo OWNER/zvec-grep`，然后交互式输入密钥。
3. 在同一页面新增仓库 **variable** `SWE_QA_MODEL_BASE_URL`，例如 `https://YOUR-WORKSPACE.cn-beijing.maas.aliyuncs.com/compatible-mode/v1`。填入**你自己业务空间**的 OpenAI 兼容地址，确保区域与密钥匹配；执行与评审都会使用它。如未配置，工作流会继续使用代码中原维护者的 endpoint，你的密钥未必可用。
4. 默认的 `embedding=local` 使用 `local/potion-code-16m-v2`，无需 Embedding 密钥。若选择 `embedding=remote`，还需配置仓库 secret `QWEN_EMBEDDING_API_KEY` 和仓库 variable `QWEN_EMBEDDING_ENDPOINT`；工作流与 Retrieval-only 一样使用 `qwen/qwen3.7-text-embedding`。远端模式会将源码内容发送到该 endpoint。
5. 进入 **Actions → SWE-QA Bench → Run workflow**，选择 harness 分支并设置 `candidate_ref`、`model`、`embedding`、`scope`。建议先运行 `repro-3`：3 题 × 2 组 × 10 次，共 60 个 trial。`smoke` 是 5 题 / 100 个 trial；`all-full` 是 20 题 / 400 个 trial。失败的 trial 最多额外重试两次，可能产生更多模型调用。执行和评审使用同一个所选模型，默认 `glm-5.2`。

固定 trial 次数、失败重试上限以及每种 Embedding runtime 对应的模型由版本化的 [`ci-config.json`](../ci-config.json) 指定。要修改这些实验规则，应在 harness 分支修改此文件；workflow 输入只选择其中一个已定义 runtime，环境变量传递 endpoint 和密钥。

也可以通过 CLI 触发：

```sh
gh workflow run swe-qa-bench.yml --repo OWNER/zvec-grep \
  --ref main \
  -f candidate_ref=main \
  -f model=glm-5.2 \
  -f embedding=local \
  -f scope=repro-3
```

测试自己的 Rust 改动时，将 `candidate_ref` 改为 fork 中 Rust 分支名或完整提交 SHA。`--ref` 指向 **benchmark harness**，`candidate_ref` 指向 **Rust 候选包**。push 和 PR 不会触发这个工作流，也不会占用上游仓库 CI。只有 `write` 权限的用户可能看得到触发按钮，但工作流会在检出代码及读取模型密钥之前拒绝执行。

运行 Summary 会显示 Rust 候选提交及成功任务的 Aggregate。`swe-qa-aggregate-report-*` artifact 包含 `report.md`、`report.json` 和记录候选提交及包哈希的 `candidate-manifest.json`，任务报告保留 90 天；Harbor 配对证据（包括 `.retry-history/` 中的失败尝试）保留 30 天。若部分任务失败，它们会被列为缺失且不进入任何 Aggregate 指标；工作流结论仍保持失败。

参考：[GitHub 手动运行工作流](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manually-run-a-workflow)、[Actions Secrets](https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/use-secrets)、[百炼 API Key](https://help.aliyun.com/zh/model-studio/get-api-key)、[百炼地域与 endpoint](https://help.aliyun.com/zh/model-studio/regions/)。
