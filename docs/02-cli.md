# CLI guide

[Documentation](./README.md) · [Agents](./01-agents.md) ·
[CLI](./02-cli.md) · [MCP](./03-mcp.md) · [Pipeline](./04-pipeline.md) ·
[Architecture](./05-architecture.md) · [Server](./06-server.md) ·
[Embedding](./07-embedding.md) · [Roadmap](./08-roadmap.md)

The `zg` command is the human and shell interface to the same local search
layer used by agents. This page groups the main commands and options; use the
installed CLI for version-specific help:

```bash
zg help
zg help query
zg help models
zg help file-types
zg help environment
zg <command> --help
```

## Command overview

| Command | Purpose |
| --- | --- |
| `query` | Search an index or run managed ripgrep |
| `index` | Build, update, rebuild, or drop a Workspace index |
| `status` | Inspect Workspace and index state |
| `install` / `uninstall` | Manage agent integrations |
| `config` | Configure provider credentials and model defaults |
| `auth` | Manage Remote Embedding authorization |
| `server` | Manage the shared MCP server |
| `help` / `version` | Show help or the installed version |

## `zg query`

```text
zg query <query> [options]
zg query --hybrid <query> --fts <query> --vector <query> [--fuse]
zg query --rg [rg-options] <pattern> [path...]
```

Search routes:

| Option | Meaning |
| --- | --- |
| positional query | Hybrid lexical and vector retrieval |
| `--hybrid <query>` | Add an explicit hybrid query group |
| `--fts <query>` | Add a ranked lexical query group |
| `--vector <query>` | Add a semantic-only query group |
| `--fuse` | Combine all groups into one ranked plan |
| `--rg` | Run exhaustive managed ripgrep without an index |

Result controls:

| Option | Meaning |
| --- | --- |
| `--limit <n>` | Maximum returned items per group |
| `--human` | Terminal-oriented output with full previews by default |
| `--preview none\|short\|full` | Indexed source preview size |
| `--refresh background\|wait\|off` | Index refresh policy |
| `--mode direct\|server\|auto` | Execution transport |
| `--debug` | Print diagnostics to stderr |
| `--trace` | Add per-hit indexed search trace |
| `--prefer-symbol` | Prefer an exact indexed symbol |
| `--symbol-type <type>` | Restrict results to a symbol type |
| `--modified-after <time>` | Search files modified after a time |
| `--modified-before <time>` | Search files modified before a time |

Indexed CLI results are separated by query group and preserve the rank assigned
inside that group. The CLI does not apply the MCP response's cross-group
coverage/global-fill presentation. A result recalled by several groups appears
under each of those groups. `--limit` continues to bound each group.

Valid symbol types are `module`, `class`, `interface`, `function`, `value`, and
`alias`.

Common scope options for indexed search are `-g/--glob`, `--iglob`, `-t/--type`,
and `-T/--type-not`. Managed rg additionally supports common ripgrep matching,
context, discovery, encoding, and regex-engine flags.

Examples:

```bash
zg query "theme preference persistence on startup"
zg query --fts "loadTheme" -g "src/**" -t ts
zg query --vector "where user preferences are restored" --limit 5
zg query --human "plugin lifecycle" --preview full
zg query --rg -i -C 2 -g "*.ts" "dark mode" src
```

See [Retrieval pipeline](./04-pipeline.md#3-query-through-one-search-layer) for
route selection and freshness behavior.

## `zg index`

```text
zg index [root] [options]
zg index [root] --rebuild [options]
zg index [root] --drop [--yes]
```

Core options:

| Option | Meaning |
| --- | --- |
| `--embedding <model>` | Model for a new or rebuilt index |
| `--rebuild` | Recreate an existing index |
| `--drop` | Permanently remove the Workspace index |
| `--yes` | Confirm `--drop` without a prompt |
| `--reset-paths` | Replace stored file-selection settings |
| `--mode direct\|server\|auto` | Execution transport |
| `--api-key <key>` | One-command provider credential |
| `--endpoint <url>` | Remote provider endpoint |
| `--model-cache <path>` | Local model cache directory |
| `--device <device>` | `auto`, `cpu`, `metal`, `vulkan`, or `cuda` |
| `--embedding-concurrency <n>` | Concurrent Embedding tasks |
| `--allow-remote` | Authorize Remote Embedding for this command |

Local Potion embedding tasks run on worker threads. They default to two workers;
`--embedding-concurrency` can override that value for larger machines.

File discovery accepts `-g/--glob`, `--iglob`, `-t/--type`, `-T/--type-not`,
`--hidden`, `--no-ignore`, `--ignore-file`, `--max-depth`, `--max-filesize`, and
`-L/--follow`.

Examples:

```bash
zg index --embedding local/potion-code-16m-v2
zg index
zg index --rebuild --embedding local/jina-embeddings-v2-base-code
zg index --drop --yes
```

## `zg status`

```text
zg status [root] [--mode direct|server|auto] [--check-ready]
```

Status includes the selected root, index policy, stored schema and paths, file
counts, refresh state, and a suggested next action. `--check-ready` preserves
normal output and exits non-zero unless the index is ready, which is useful in
scripts.

## `zg install` and `zg uninstall`

```text
zg install [--target codex|claude|qwen|qoder|opencode|cursor|all|auto] [--mcp-transport stdio|http] [--mcp-toolset agent|full] [--yes] [--force]
zg uninstall [--target codex|claude|qwen|qoder|opencode|cursor|all|auto] [--yes]
```

`--target` is repeatable. `qodercli` and `qoder-cli` are accepted aliases for
the canonical `qoder` target. `zg install` also accepts:

| Option | Meaning |
| --- | --- |
| `--mcp-transport <stdio\|http>` | MCP connection mode; default `stdio` |
| `--mcp-toolset <agent\|full>` | Daemon MCP surface; default `agent` |
| `--mcp-tool-timeout <seconds>` | Codex, Qwen Code, Qoder, and OpenCode MCP timeout; default 600 seconds |
| `--mcp-token-env <name>` | Environment variable containing the server token |
| `--force` | Replace a conflicting unmanaged `zvec_grep` entry |

The Qoder target writes MCP configuration to `settings.json` for Qoder CLI and
`mcp.json` for Qoder IDE.

See [Agent integrations](./01-agents.md) before using `--force`.

## `zg config`

```text
zg config provider set <provider> --api-key <key>
zg config model set <model> [--endpoint <url> | --device <device>] [--default]
```

Examples:

```bash
zg config provider set qwen --api-key "$DASHSCOPE_API_KEY"
zg config model set qwen/text-embedding-v4 --default
zg config model set local/potion-code-16m-v2 --device metal
```

Global configuration is stored in `~/.zvec-grep/config.json`. Existing indexes
continue to use their stored model until explicitly rebuilt.

## `zg auth`

```text
zg auth grant [root] --capability embedding --scope workspace [--embedding <model>]
zg auth status [root]
zg auth revoke [root]
```

Workspace grants are stored under `.zvec-grep/authorization.json` and shared by
the CLI and MCP server. `--allow-remote` is the non-persistent alternative for
one `query` or `index` command. `--embedding` selects the Remote Embedding model
to authorize; it does not run embedding. It may be omitted when the model can be
resolved from the existing Workspace index, `ZVEC_GREP_EMBEDDING`, or the global
default, in that order.

## `zg server`

```text
zg server on [--listen 127.0.0.1:7999] [--token-file <path>] [--mcp-toolset agent|full]
zg server off [--token-file <path>]
zg server status [--check-ready]
zg server run [--listen 127.0.0.1:7999] [--token-file <path>] [--mcp-toolset agent|full]
```

`on` starts the background daemon; `run` keeps it in the foreground. The server
only accepts loopback listen addresses. The default public endpoint is
`http://127.0.0.1:7999/mcp` with the `agent` toolset.

See [Server and execution modes](./06-server.md) for mode selection, lifecycle,
refresh, authentication, and logs. See [MCP](./03-mcp.md) for the tool contract.

## Environment variables

| Variable | Purpose |
| --- | --- |
| `ZVEC_GREP_HOME` | Override zvec-grep state directory |
| `ZVEC_GREP_MODE` | Default `direct`, `server`, or `auto` transport |
| `ZVEC_GREP_SERVER_URL` | Override the MCP server URL used by clients |
| `ZVEC_GREP_SERVER_TOKEN` | Server/client Bearer token |
| `ZVEC_GREP_SERVER_TOKEN_FILE` | File containing the server/client token |
| `ZVEC_GREP_MCP_TOOLSET` | Default `agent` or `full` MCP surface |
| `ZVEC_GREP_EMBEDDING` | Default model for new indexes |
| `ZVEC_GREP_API_KEY` | Embedding provider API key |
| `ZVEC_GREP_ENDPOINT` | Remote Embedding endpoint |
| `ZVEC_GREP_MODEL_CACHE` | Local model cache directory |
| `ZVEC_GREP_DEVICE` | Local model device |
| `DASHSCOPE_API_KEY` | Qwen API-key fallback after `ZVEC_GREP_API_KEY` |
| `QWEN_API_KEY` | Qwen API-key fallback after `DASHSCOPE_API_KEY` |
| `QWEN_HOME` | Qwen Code configuration directory used by `zg install` |
| `QODER_CONFIG_DIR` | Qoder configuration directory used by `zg install` |

Run `zg help environment` for advanced variables, agent integration paths,
scope, and detailed precedence. A new index selects its model in this order:
explicit `--embedding`, `ZVEC_GREP_EMBEDDING`, then the global default. Existing
indexes continue to use their stored model unless `--embedding` and `--rebuild`
explicitly change it.

Embedding runtime values such as endpoint and device retain this order: explicit
command option, Workspace snapshot, global configuration, then environment.
`zg index` forwards its `ZVEC_GREP_EMBEDDING` value in server and auto modes;
direct MCP calls use the environment inherited when the daemon started.
