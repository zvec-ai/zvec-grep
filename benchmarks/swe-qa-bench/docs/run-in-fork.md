# Run the Rust SWE-QA benchmark in your fork

The [SWE-QA workflow](../../../.github/workflows/swe-qa-bench.yml) compares OpenCode without and with the **same selected Rust zvec-grep build**. It never benchmarks the TypeScript package checked out with the harness. `candidate_ref` may be a Rust branch, tag, or commit; it defaults to `main`. The workflow records the resolved commit and verifies the npm tarball hash before each task. The benchmark is manual-only and runs only for a fork's `admin` or `maintain` members, including job reruns.

1. Fork `zvec-ai/zvec-grep` and clone your fork. Enable GitHub Actions in the fork. Put this workflow on the fork's **default branch** first: GitHub's manual dispatch control requires the workflow file to exist there. You may then use `--ref` to choose a different harness branch. Set `candidate_ref` independently to choose the Rust source to build. The chosen ref must exist in the fork and contain `rust/`.
2. Obtain a Bailian API key for a workspace that can invoke the selected `glm-5.2` or `qwen3.8-max` model. In your fork's **Settings → Secrets and variables → Actions**, add the repository secret `GLM_API_KEY`. Both model choices use this legacy secret name for execution and self-judging. Do not commit the key or pass it on a command line. With [GitHub CLI](https://cli.github.com/manual/gh_secret_set), run `gh secret set GLM_API_KEY --repo OWNER/zvec-grep` and enter the value interactively.
3. In the same Actions settings, add the repository **variable** `SWE_QA_MODEL_BASE_URL`, for example `https://YOUR-WORKSPACE.cn-beijing.maas.aliyuncs.com/compatible-mode/v1`. Use the OpenAI-compatible URL for **your** Bailian workspace and the region of your API key. The variable is applied to both execution and judging. If omitted, the workflow retains the project maintainer's checked-in endpoint, which may not accept your key.
4. The default `embedding=local` uses `local/potion-code-16m-v2` and needs no embedding credential. For `embedding=remote`, add the repository secret `QWEN_EMBEDDING_API_KEY` and repository variable `QWEN_EMBEDDING_ENDPOINT`; the workflow uses `qwen/qwen3.7-text-embedding`, as Retrieval-only does. Source content is sent to that endpoint in remote mode.
5. Open **Actions → SWE-QA Bench → Run workflow**, choose the harness branch, then set `candidate_ref`, `model`, `embedding`, and `scope`. Start with `repro-3` (3 tasks × 2 profiles × 10 trials = 60 trials). `smoke` has 5 tasks / 100 trials; `all-full` has 20 tasks / 400 trials. Failed trials can be retried twice, causing additional model calls. Both execution and judging use the selected model. The default model is `glm-5.2`.

Equivalent CLI dispatch:

```sh
gh workflow run swe-qa-bench.yml --repo OWNER/zvec-grep \
  --ref main \
  -f candidate_ref=main \
  -f model=glm-5.2 \
  -f embedding=local \
  -f scope=repro-3
```

Use your fork's Rust branch name or full commit SHA for `candidate_ref` when evaluating a change. `--ref` selects the **benchmark harness**; `candidate_ref` selects the **Rust package**. The workflow does not run from a pull request or push, and it does not use upstream CI resources. A GitHub user with only `write` access may see the dispatch button, but the workflow rejects the run before checkout or model-secret use.

The versioned [`ci-config.json`](../ci-config.json) sets the trial count, failure retry limit, and model behind each embedding runtime. Change that file on the harness branch to change the fixed protocol; the workflow input selects one of those versioned runtimes, while environment variables carry endpoints and credentials.

The run Summary shows the selected Rust commit, raw and score-IQR-filtered task-weighted means, retained/excluded sample counts, and exclusion details for completed tasks. Download the `swe-qa-aggregate-report-*` artifact for `report.md`, `report.json`, and `candidate-manifest.json` (commit and tarball hash); individual task reports are retained for 90 days. Paired Harbor evidence, including failed attempts under `.retry-history/`, is retained for 30 days. If tasks fail, they are listed as missing and excluded from every Aggregate metric; the workflow still has a failed conclusion.

References: [GitHub manual workflows](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manually-run-a-workflow), [Actions secrets](https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/use-secrets), [Bailian API keys](https://help.aliyun.com/zh/model-studio/get-api-key), [Bailian regions and endpoints](https://help.aliyun.com/zh/model-studio/regions/).
