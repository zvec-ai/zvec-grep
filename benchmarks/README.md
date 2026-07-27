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
- **CI:** a fixed, representative subset used to detect regressions over time,
  when that suite defines one.
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
[Docker Engine or Docker Desktop](https://docs.docker.com/), Docker Compose v2,
and the credentials required by your chosen Harbor agent and model. Verify that
Compose is available as a Docker CLI plugin, not the legacy `docker-compose`
command:

```sh
docker compose version
```

On Ubuntu, an `Unable to locate package docker-compose-plugin` error normally
means the Docker apt repository has not been configured. Follow Docker's Ubuntu
installation instructions to add its official repository, then install the
Compose plugin.

From this directory, install the pinned Python environment and check the base
setup:

```sh
uv sync
uv run zg-bench doctor
```

Using a local zvec-grep checkout additionally requires
[Node.js 22 or newer and npm](https://nodejs.org/) and its installed dependencies:

```sh
npm --prefix .. ci
```

If doctor reports `registry.anpm.alibaba-inc.com`, either update the user-level
registry or override it for setup and benchmark commands:

```sh
npm config set registry https://registry.npmjs.org/ --location=user
# Or, for one command:
npm_config_registry=https://registry.npmjs.org/ npm ci
```

The repository intentionally does not commit an `.npmrc` that rewrites registry
hosts; public download URLs are kept correct in `package-lock.json` instead.

Run a profile-aware check before starting Docker. It verifies credentials,
package compatibility, Node/npm, local build dependencies, and registry state:

```sh
uv run zg-bench doctor \
  --agent opencode \
  --model aliyun-glm-5.2 \
  --profile zvec-grep \
  --zvec-grep-package ..
```

`zg-bench run` performs the same preflight automatically. The explicit doctor
command is useful while preparing a machine because it stops before packaging
or creating any Docker resources.

SWE-bench task images install `uv 0.7.13` while Docker builds them. If GitHub is
unavailable from the Docker build network, download the Linux x86-64 release
archive once on a machine with access:

```sh
curl -L \
  https://github.com/astral-sh/uv/releases/download/0.7.13/uv-x86_64-unknown-linux-gnu.tar.gz \
  -o .cache/uv-x86_64-unknown-linux-gnu.tar.gz
```

Then pass it to the benchmark run:

```sh
uv run zg-bench run swebench-verified \
  --tier smoke \
  --agent opencode \
  --model aliyun-glm-5.2 \
  --profile all \
  --uv-archive .cache/uv-x86_64-unknown-linux-gnu.tar.gz
```

The runner resolves the selected Harbor tasks, creates an isolated task copy
under `benchmarks/.cache/agent-setup/uv-tasks`, copies the archive into each
Docker build context, and replaces the online installer layer with a local
`COPY` and install step. Harbor's global content-addressed task cache is left
unchanged. The archive must contain both `uv` and `uvx`.

Benchmark-controlled GitHub downloads use `gh-proxy.com` by default:

```sh
uv run zg-bench run swebench-verified \
  --tier smoke \
  --agent opencode \
  --model aliyun-glm-5.2 \
  --profile all
```

This applies to GitHub Release assets, Raw content, archives, git clone URLs,
the internal GitHub download performed by the uv installer, and the internal
nvm repository clone. It covers supported agent setup commands and setup files
inside the selected benchmark tasks. The runner uses an isolated task copy, so
Harbor's global task cache remains unchanged. To use direct GitHub downloads
for a run, pass `--no-github-proxy`.

The proxy is a third-party trust dependency. Existing SHA-256 checks remain
enforced, including the pinned OpenCode binary checksum. Downloads without an
upstream checksum retain their existing risk and add the proxy operator to the
trust path. Do not send GitHub credentials or private-repository URLs through
this public proxy.

### Ubuntu 24.04 OpenCode + GLM-5.2 quickstart

From the repository root:

```sh
# Node.js 22 or newer must already be active.
npm ci

cd benchmarks
uv sync
export DASHSCOPE_API_KEY="your-api-key"

uv run zg-bench doctor \
  --agent opencode \
  --model aliyun-glm-5.2 \
  --profile all \
  --zvec-grep-package ..

uv run zg-bench run swebench-verified \
  --tier smoke \
  --agent opencode \
  --model aliyun-glm-5.2 \
  --profile all \
  --zvec-grep-package ..
```

Omit `--job-name` unless a stable name is required. Timestamped defaults avoid
collisions with existing Harbor output directories.

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

To run Aliyun GLM-5.2 or Qwen 3.7 Max through OpenCode, export a DashScope API
key:

```sh
export DASHSCOPE_API_KEY="your-api-key"
```

Use either supported combination in the test commands below:

```sh
--agent opencode --model aliyun-glm-5.2
--agent opencode --model qwen3.7-max
```

The runner pins the OpenCode version, maps the selected model to the custom
DashScope provider, configures the public Beijing endpoint, and selects
OpenCode's Chat Completions-compatible AI SDK package. OpenCode runs through
its official ACP server so Harbor owns the session lifecycle through a
structured protocol. Its ACP release manifest and SHA-256 checksum are pinned
with the benchmark instead of being resolved from the mutable registry at run
time. The credential is passed through Harbor's host-environment template and
is neither included in the generated command nor persisted in benchmark
output. Thinking is disabled so a single long response cannot consume the
task's 50-minute agent timeout.

### zvec-grep authentication

The zvec-grep profile uses Qwen's `text-embedding-v4` model. Export a DashScope
API key on the host before running that profile:

```sh
export DASHSCOPE_API_KEY="your-api-key"
```

The adapter uploads the embedding credential to a protected zvec-grep config
file in the isolated agent environment and does not add its value to the
generated Harbor command. Before indexing, it creates a Workspace-scoped Remote
Embedding grant with `zg auth grant`; the index command and later MCP queries
reuse that grant without `--allow-remote`. For Codex and OpenCode, it configures
the agent's MCP integration and starts the zvec-grep server inside the task
container. If the same credential also authenticates the agent's model, that
agent's normal authentication path still applies.

## Test run instructions

Add `--dry-run` to inspect the generated Harbor command without starting a
container. Harbor writes trajectories and evaluator output to `runs/`.

Inspect the supported agent/model combinations, available suites, and exact
tasks selected by a tier:

```sh
uv run zg-bench list agent-models
uv run zg-bench list suites
uv run zg-bench list tasks swebench-verified --tier smoke
uv run zg-bench list tasks django-focused --tier full
```

`run` and run-specific `doctor` commands reject unknown agents and unsupported
models while parsing their arguments. Qwen Code and OpenCode accept only the
combinations shown by `list agent-models`; Codex model identifiers are passed
through to Codex's native model catalog.

The first run may take several minutes while Docker downloads and builds the
task image and prepares the agent environment. This is expected. By default,
the generated Compose overlay sets `PIP_INDEX_URL` to the USTC PyPI mirror for
pip commands executed inside the task container. The zvec-grep profile installs
the pinned npm package. Pass
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

The zvec-grep profile also caches the complete `.zvec-grep` workspace directory
after a successful index build. The default cache root is
`benchmarks/.cache/zvec-grep-indexes`, organized by dataset, case, and embedding
model. A later run copies the backup into its own workspace before validating
the index, so benchmark trials remain isolated from both the backup and each
other. Concurrent first runs for the same key share a host lock. An invalid
backup is quarantined and rebuilt once; cache mount or copy failures fall back
to the normal index build without failing the benchmark.

Use `--zvec-index-cache-dir <path>` to move the persistent cache, or
`--no-zvec-index-cache` to disable it. Dataset revisions such as `@2` are not
part of the cache key because benchmark cases are treated as stable.

Each command runs both profiles by default. Use `--profile baseline` or
`--profile zvec-grep` to run one profile. The zvec-grep profile supports Codex,
Qwen Code, and OpenCode and builds its index before agent execution. Codex and
OpenCode use MCP; Qwen Code keeps the benchmark skill integration.

### Run the smoke test

#### SWE-bench Verified

```sh
uv run zg-bench run swebench-verified \
  --tier smoke --agent <agent> --model <provider/model>
```

Use a published version or npm spec:

```sh
uv run zg-bench run swebench-verified \
  --agent <agent> --model <provider/model> \
  --profile zvec-grep --zvec-grep-package <compatible-version>
```

Published zvec-grep `0.1.5` does not support Workspace Remote Embedding
authorization and also lacks the OpenCode installer. Use the benchmark default
(`0.1.6-alpha.3` or newer) or the current checkout.

Use the current repository checkout when running from `benchmarks/`:

```sh
uv run zg-bench run swebench-verified \
  --agent <agent> --model <provider/model> \
  --profile zvec-grep --zvec-grep-package ..
```

#### Terminal-Bench 2.1

```sh
uv run zg-bench run terminal-bench-2.1 \
  --tier smoke --agent <agent> --model <provider/model>
```

Override a tier with one or more exact Harbor task names when debugging:

```sh
uv run zg-bench run swebench-verified \
  --tier smoke --task swe-bench/pallets__flask-5014 \
  --agent <agent> --model <provider/model>
```

### Run the CI test

The CLI supports CI tiers, but a suite must define its curated task list first.
The current built-in suites do not yet define one; attempting to run it reports
the configured tiers instead of silently substituting smoke tasks.

### Run the focused Django suite

The `django-focused` suite contains five curated SWE-bench Verified tasks
covering query compilation, fast deletion, form data flow, constraint
validation, and the middleware response chain. Run the complete group with:

```sh
uv run zg-bench run django-focused \
  --tier full --agent <agent> --model <provider/model>
```

Add `--profile baseline` or `--profile zvec-grep` to run only one tool profile.
Use `list tasks` as shown above to inspect the exact task names before a run.

### Run the full benchmark

Run every task in the pinned dataset revision explicitly with:

```sh
uv run zg-bench run swebench-verified \
  --tier full --agent <agent> --model <provider/model>
```

This can be expensive. Use `--dry-run` first and keep the agent, model, package,
and platform fixed across compared profiles.

### Diagnose a failed run

When a trial records an exception, even if Harbor itself exits successfully,
`zg-bench` prints the structured exception and zvec-grep setup error
automatically and exits non-zero. The same report can be requested later:

```sh
uv run zg-bench diagnose --latest
uv run zg-bench diagnose <job-name>
```
