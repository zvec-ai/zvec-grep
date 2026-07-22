# Benchmarks

This directory contains benchmarks for measuring how `zvec-grep` affects an
agent's ability to complete real-world tasks.

The comparison keeps the model, agent loop, task prompt, environment, and limits
the same. The only difference is the tool profile:

- **Baseline:** the agent's standard tools, including shell utilities such as
  `rg` and `find` when available.
- **zvec-grep:** the same agent with a prepared `zvec-grep` index. Codex and
  OpenCode access it through MCP; Qwen Code retains its query-only skill.

## Benchmark suites

- **[SWE-bench Verified](https://www.swebench.com/SWE-bench/guides/datasets/):**
  evaluates an agent's ability to resolve real-world software issues by
  modifying an existing repository. Solutions are graded by running repository
  tests.
- **[Terminal-Bench 2.1](https://www.tbench.ai/news/terminal-bench-2-1):** a
  collection of complex tasks completed in isolated terminal environments. It
  covers areas such as software engineering, system administration, data
  processing, and machine learning, with programmatic evaluation of results.

## Run tiers

Each benchmark suite can be run at different tiers:

- **Smoke:** one task that quickly verifies the complete benchmark workflow.
- **CI:** a fixed, representative subset used to detect regressions over time.
- **Full:** the complete suite, used for release results and reports.

Smoke and CI results help us develop and maintain the benchmark. Only full runs
are intended to support general performance claims.

## Metrics

We track two outcomes and one diagnostic:

1. **Outcome quality:** the score or reward from the benchmark's official
   evaluator, including correctness and any benchmark-provided quality signals.
2. **Efficiency:** tokens, wall-clock time, cost, and tool calls used to
   complete the task.
3. **Tool behavior:** whether and how the agent used `zvec-grep`, and how
   quickly it reached relevant information.

Index construction time is recorded separately from agent execution.

## Setup instructions

### Platform support

Benchmarks can run through Docker on Linux or macOS. On Apple silicon, some
images require emulation and individual tasks may not be compatible. Full
benchmark reports should use a consistent Linux x86-64 environment.

### Install dependencies

You need [uv](https://docs.astral.sh/uv/),
[Docker Engine or Docker Desktop](https://docs.docker.com/), and the credentials
required by your chosen Harbor agent and model. From this directory, install the
pinned environment and check the local setup:

```sh
uv sync
uv run zg-bench doctor
```

Using a local zvec-grep checkout additionally requires
[Node.js and npm](https://nodejs.org/) and its installed dependencies:

```sh
npm --prefix .. ci
```

### Codex authentication

When using `--agent codex`, choose one authentication method.

**API key:** Harbor uses `OPENAI_API_KEY` by default.

```sh
export OPENAI_API_KEY="your-api-key"
```

**ChatGPT subscription:** install the Codex CLI on the host, sign in, then tell
Harbor to use the resulting credentials:

```sh
codex login
codex login status
export CODEX_FORCE_AUTH_JSON=1
```

Subscription authentication requires `~/.codex/auth.json`. If the Codex CLI
uses the operating-system credential store instead, configure file storage in
`~/.codex/config.toml` and sign in again:

```toml
cli_auth_credentials_store = "file"
```

Treat `auth.json` like a password: never commit or share it.

### Qwen Code authentication

Harbor names the [Qwen Code](https://github.com/QwenLM/qwen-code) agent
`qwen-coder`. To run Qwen 3.7 Max through DashScope, export an API key:

```sh
export DASHSCOPE_API_KEY="your-api-key"
```

Use `--agent qwen-coder --model qwen3.7-max` in the test commands below. The
runner pins the Qwen Code version and configures the DashScope endpoint. The
same key can also authenticate the zvec-grep embedding model when both profiles
are selected.

### OpenCode authentication

To run Aliyun GLM-5.2 through OpenCode, export a DashScope API key:

```sh
export DASHSCOPE_API_KEY="your-api-key"
```

Use `--agent opencode --model aliyun-glm-5.2` in the test commands below. The
runner pins the OpenCode version, maps the alias to DashScope's `glm-5.2` model,
configures the public Beijing endpoint, and selects OpenCode's Chat
Completions-compatible AI SDK package. OpenCode runs through its official ACP
server so Harbor owns the session lifecycle through a structured protocol. Its
ACP release manifest and SHA-256 checksum are pinned with the benchmark instead
of being resolved from the mutable registry at run time. The credential is
passed through Harbor's host-environment template and is neither included in the
generated command nor persisted in benchmark output. This GLM configuration
disables model thinking so a single long response cannot consume the task's
50-minute agent timeout.

### zvec-grep authentication

The zvec-grep profile uses Qwen's `text-embedding-v4` model. Export a DashScope
API key on the host before running that profile:

```sh
export DASHSCOPE_API_KEY="your-api-key"
```

The adapter uploads the embedding credential to a protected zvec-grep config
file in the isolated agent environment and does not add its value to the
generated Harbor command. For Codex and OpenCode, it configures the agent's MCP
integration and starts the zvec-grep server inside the task container. If the
same credential also authenticates the agent's model, that agent's normal
authentication path still applies.

## Test run instructions

Add `--dry-run` to inspect the generated Harbor command without starting a
container. Harbor writes trajectories and evaluator output to `runs/`.

The first run may take several minutes while Docker downloads and builds the
task image and prepares the agent environment. This is expected. By default,
the zvec-grep profile installs the pinned npm package. Pass
`--zvec-grep-package <version-or-npm-spec>` to select another published package,
or `--zvec-grep-package <directory-or-tgz>` to test local code. Local directories
are packaged with `npm pack`, and only the resulting tarball is mounted into the
task container. Its SHA-256 is recorded in setup metadata and included in the
setup-cache identity. Later runs of the same package reuse both the large task
image and a Docker volume containing the installed agent runtime. Setup caches
are isolated by agent and profile, so baseline containers never receive
zvec-grep. They contain neither credentials, the repository index, nor the MCP
configuration. The zvec-grep profile also skips the unused local embedding
runtime when Qwen embeddings are selected.

Each command runs both profiles by default. Use `--profile baseline` or
`--profile zvec-grep` to run one profile. The zvec-grep profile supports Codex,
Qwen Code, and OpenCode and builds its index before agent execution. Codex and
OpenCode use MCP; Qwen Code keeps the benchmark skill integration.

### Run the smoke test

#### SWE-bench Verified

```sh
uv run zg-bench run swebench-verified \
  --agent <agent> --model <provider/model>
```

Use a published version or npm spec:

```sh
uv run zg-bench run swebench-verified \
  --agent <agent> --model <provider/model> \
  --profile zvec-grep --zvec-grep-package 0.1.5
```

Use the current repository checkout when running from `benchmarks/`:

```sh
uv run zg-bench run swebench-verified \
  --agent <agent> --model <provider/model> \
  --profile zvec-grep --zvec-grep-package ..
```

#### Terminal-Bench 2.1

```sh
uv run zg-bench run terminal-bench-2.1 \
  --agent <agent> --model <provider/model>
```

### Run the CI test

The CI tier is not implemented yet. It will run a fixed, representative task
set for each benchmark suite.

### Run the full benchmark

The Full tier is not implemented yet. It will run the complete task set for a
benchmark suite and produce the results used in external reports.
