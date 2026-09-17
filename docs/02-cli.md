# CLI guide

[Documentation](./README.md) · [Agents](./01-agents.md) ·
[CLI](./02-cli.md) · [MCP](./03-mcp.md) · [Pipeline](./04-pipeline.md) ·
[Architecture](./05-architecture.md) · [Server](./06-server.md) ·
[Embedding](./07-embedding.md) · [Roadmap](./08-roadmap.md)

This guide describes the development build in this source checkout. The
[README quickstart](../README.md#try-it-yourself) keeps the published npm
release's syntax; use `zg --help` to check the version you have installed.

The `zg` command is a search interface first. Positional arguments are always
queries; maintenance operations use long action options so ordinary words such
as `index`, `install`, and `status` never collide with commands. Use the
installed CLI for version-specific help:

To expose accidental use of the old command shape without making it an alias,
a leading command-shaped word such as `query`, `index`, or `install` emits a
warning on stderr and is still parsed as search input. Use `zg -- query` (or
another literal word after `--`) to make that search intent explicit and
suppress the warning.

```bash
zg --help
zg --help search
zg --help models
zg --help file-types
zg --help environment
zg --index --help
```

## Interface overview

| Form | Purpose |
| --- | --- |
| `zg <query>` | Search an index or run managed ripgrep |
| `--index` | Build, update, rebuild, or drop a Workspace index |
| `--status` | Inspect Workspace and index state |
| `--install` / `--uninstall` | Manage agent integrations |
| `--config` | Configure provider credentials and model defaults |
| `--auth` | Manage Remote Embedding authorization |
| `--server` | Manage the shared MCP server |
| `--help` / `--version` | Show help or the installed version |

## `zg`

```text
zg <query> [options]
zg --hybrid <query> --fts <query> --vector <query> [--fuse]
zg --rg [rg-options] <pattern> [path...]
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
| `--compact` | Force compact output intended for pipes |
| `--preview none\|short\|full` | Indexed source preview size |
| `--refresh background\|wait\|off` | Index refresh policy |
| `--mode direct\|server\|auto` | Execution transport |
| `--debug` | Print diagnostics to stderr |
| `--trace` | Add per-hit indexed search trace |
| `--prefer-symbol` | Prefer an exact indexed symbol |
| `--symbol-type <type>` | Restrict results to a symbol type |
| `--modified-after <time>` | Search files modified after a time |
| `--modified-before <time>` | Search files modified before a time |

Ordinary positional words form one query: `zg connection pool timeout`,
`zg 'connection pool timeout'`, and `zg "connection pool timeout"` have the same
search intent. Shell quotes group text; they are not search-mode operators.
Use repeated `--hybrid` options for deliberate independent query groups.

Single-query CLI output is one result list. Explicit multi-query results are
separated by query group and preserve the rank assigned
inside that group. The CLI does not apply the MCP response's cross-group
coverage/global-fill presentation. A result recalled by several groups appears
under each of those groups. `--limit` continues to bound each group.

Valid symbol types are `module`, `class`, `interface`, `function`, `value`, and
`alias`.

Common scope options for indexed search are `-g/--glob`, `--iglob`, `-t/--type`,
and `-T/--type-not`. Managed rg additionally supports common ripgrep matching,
context, discovery, encoding, and regex-engine flags.

Ordinary searches first check current files. File paths such as `src/auth.ts` or
`auth.ts` find files by name, without reading or embedding their contents. A
path found only in source text still returns those text matches. Clear code
identifiers such as `verifyToken` or `CONNECTION_POOL` use literal matching;
definitions precede references. A complete lookup with no match returns
`No matches.` instead of unrelated vector neighbors, even with a warm index.
These lookups need no model, index, or daemon and respect the stored workspace
scope and query filters. Ambiguous words and questions retain hybrid retrieval.
Explicit routes (`--fts`, `--vector`, `--hybrid`), `--refresh`, model overrides
and `--mode server` retain the indexed workflow.

In default `auto` mode, indexed search starts or reuses the local daemon. If no
index is usable, it starts or reuses background local preparation. Existing text
matches return without waiting for the build, with an explicit stderr warning
that semantic results are not ready. With neither literal nor bounded keyword
matches, ordinary search waits
up to eight seconds for initial preparation and then runs semantic search in the
same invocation if ready. This budget includes preparation status and scheduling
requests, but not daemon startup or the subsequent search. Longer builds survive
the CLI exit; the output distinguishes incomplete search from an exhaustive
`No matches.` answer. Failed initialization is not treated as a usable index on
the next invocation, which can retry preparation using the persisted local model,
even if the default model has changed. Implicit retries never replace an existing
remote index or authorize remote preparation. A populated index can still be
searched during a refresh or after a per-file failure.

For an existing index using a local embedding provider, ordinary primary queries
served by the daemon have a separate internal 1,000 ms query-model preparation
budget, covering model loading and query embedding. If it expires, the same
request returns indexed FTS results with a warning that semantic retrieval was
skipped and search coverage is incomplete. If no fresh indexed hits remain
(none were recalled or all are stale), the default `auto` CLI also tries the
bounded current-source keyword fallback below, preserves semantic-budget
diagnostics, and reports any remaining empty result as incomplete rather than
`No matches.`; fresh FTS hits do not trigger that extra keyword scan.
Queries that finish preparation in time retain hybrid retrieval.
Explicit `--hybrid` or `--vector`, `--refresh wait`, and non-local providers are
not silently downgraded by this budget. It is not an end-to-end response deadline:
startup, transport, index checks, FTS retrieval, and output take additional time.
It does not change the configured model, the missing-index preparation behavior
above, or Direct mode's foreground behavior.

When initial index preparation is still unavailable, the ordinary CLI also tries a
bounded current-source keyword fallback. For example, `zg connection pool` can
find `connectionPool` without an embedding index. It recognizes code subwords
and simple plural forms, and requires multiple query words in one source window.
Verified function boundaries constrain multi-line windows; when structural
ownership is unavailable, only the original matching line is considered. It
does not combine distant windows or different files to manufacture a match. Results are marked
`matchedBy=keyword`, are approximate rather than literal/FTS matches, and retain
the original query. A code identifier embedded in a Chinese question is not
treated as several independent question words. This is not Chinese translation
or semantic retrieval.

The keyword scan is lazy: it does not run or reorder results on the normal warm
path. When no index is usable, useful keyword hits can return before the initial
readiness wait; an empty keyword scan preserves that preparation opportunity.
It keeps at most 200 match-position candidates per configured scope, considers
at most 64 matching positions per line and 5,000 raw matching lines per scope,
and has a 600 ms shared scan budget; structural enrichment is
separately limited to 25 files per scope and 1 MiB per file. These are fallback
resource limits, not an end-to-end query deadline. Hitting a scan or resource
limit emits an incomplete-coverage warning. CLI exit does not cancel the
independently owned background index job. Explicit ripgrep/routes keep their
existing meanings, including explicit positive globs overriding ignore rules.

When an ordinary query reaches indexed search, the CLI checks current files
again after retrieval and combines those literal matches with fresh indexed
results. Current matches come first; the same source location is not repeated
as a second indexed hit. Known-stale indexed items (including deleted files)
are omitted, with an incomplete-coverage warning. If nothing current remains,
the output says `No current matches; index results are incomplete.` rather than
claiming an exhaustive no-match. Newly edited code without literal query terms
still needs index refresh for semantic recall. This is a query-time snapshot,
not a filesystem transaction. Explicit routes, indexed diagnostics, `--mode
server`, and `--refresh wait` retain their index-only result lists.

If query embedding is unavailable (for example, a model load failure or HTTP
503) during an ordinary search, the CLI rescans current files and returns any
live literal or keyword matches with a semantic-unavailable warning.
Provider response details are not printed by this fallback; `--debug` includes
the failure code. With no current text matches, the original error still fails
the command instead of claiming there are no matches. Invalid requests, rejected
provider credentials, authorization failures, user cancellation, explicit routes,
and `--refresh wait` are not converted into successful text-only searches.
This error-based recovery does not impose a new timeout or
retry embedding, and does not grant permission for remote processing.

A configured local model is respected; a configured remote model is never used
implicitly. `--refresh wait` waits beyond the initial preparation budget. Explicit `--mode direct`
keeps indexed work in the foreground; `--mode server` requires a daemon.
Use `zg --index` to choose a remote model, narrow the indexed paths, rebuild,
or drop the index. An explicitly disabled index is not implicitly enabled.

For a single indexed lookup, exact symbols and paths rank ahead of references
and approximate matches; remaining candidates use rank fusion. A single hybrid
text query also uses bounded lexical support from paths, symbol names and
individual recalled text windows. This helps code-subword matches without
requiring an index rebuild or changing the query sent to the model. Mixed
questions mentioning a symbol are not treated as requests for that symbol's
definition. Scores and lexical-support values are not confidence estimates.
Scores need not decrease with final rank after exact-match prioritization.
Explicit `--fts`, `--vector`, and fused requests with different query strings
retain their route-only or multi-query ranking. `--trace` distinguishes rank
fusion from the subsequent lexical-support ranking stage.

Examples:

```bash
zg "theme preference persistence on startup"
zg --fts "loadTheme" -g "src/**" -t ts
zg --vector "where user preferences are restored" --limit 5
zg "plugin lifecycle" --preview full
zg --rg -i -C 2 -g "*.ts" "dark mode" src
```

See [Retrieval pipeline](./04-pipeline.md#3-query-through-one-search-layer) for
route selection and freshness behavior.

## `zg --index`

```text
zg --index [root] [options]
zg --index [root] --rebuild [options]
zg --index [root] --drop [--yes]
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
zg --index --embedding local/potion-code-16m-v2
zg --index
zg --index --rebuild --embedding local/jina-embeddings-v2-base-code
zg --index --drop --yes
```

## `zg --status`

```text
zg --status [root] [--mode direct|server|auto] [--check-ready]
```

Status includes the selected root, index policy, stored schema and paths, file
counts, refresh state, and a suggested next action. `--check-ready` preserves
normal output and exits non-zero unless the index is ready, which is useful in
scripts.

## `zg --install` and `zg --uninstall`

```text
zg --install [--target codex|claude|qwen|qoder|opencode|cursor|all|auto] [--mcp-transport stdio|http] [--mcp-toolset agent|full] [--yes] [--force]
zg --uninstall [--target codex|claude|qwen|qoder|opencode|cursor|all|auto] [--yes]
```

`--target` is repeatable. `qoder` is the single Qoder target and configures
Qoder CLI and Qoder IDE together. `zg --install` also accepts:

| Option | Meaning |
| --- | --- |
| `--mcp-transport <stdio\|http>` | MCP connection mode; default `stdio` |
| `--mcp-toolset <agent\|full>` | Daemon MCP surface; default `agent` |
| `--mcp-tool-timeout <seconds>` | Codex, Qwen Code, both Qoder clients, and OpenCode MCP timeout; default 600 seconds |
| `--mcp-token-env <name>` | Environment variable containing the server token |
| `--force` | Replace a conflicting unmanaged `zvec_grep` entry |

See [Agent integrations](./01-agents.md) before using `--force`.

## `zg --config`

```text
zg --config provider set <provider> --api-key <key>
zg --config model set <model> [--endpoint <url> | --device <device>] [--default]
```

Examples:

```bash
zg --config provider set qwen --api-key "$DASHSCOPE_API_KEY"
zg --config model set qwen/text-embedding-v4 --default
zg --config model set local/potion-code-16m-v2 --device metal
```

Global configuration is stored in `~/.zvec-grep/config.json`. Existing indexes
continue to use their stored model until explicitly rebuilt.

## `zg --auth`

```text
zg --auth grant [root] --capability embedding --scope workspace [--embedding <model>]
zg --auth status [root]
zg --auth revoke [root]
```

Workspace grants are stored under `.zvec-grep/authorization.json` and shared by
the CLI and MCP server. `--allow-remote` is the non-persistent alternative for
one search or `--index` operation. `--embedding` selects the Remote Embedding model
to authorize; it does not run embedding. It may be omitted when the model can be
resolved from the existing Workspace index, `ZVEC_GREP_EMBEDDING`, or the global
default, in that order.

## `zg --server`

```text
zg --server on [--listen 127.0.0.1:7999] [--token-file <path>] [--mcp-toolset agent|full]
zg --server off [--token-file <path>]
zg --server status [--check-ready]
zg --server run [--listen 127.0.0.1:7999] [--token-file <path>] [--mcp-toolset agent|full]
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
| `ZVEC_GREP_WATCHER_IDLE_TIMEOUT_SECONDS` | Seconds of workspace inactivity before the server releases its watcher; default `14400`, `0` disables idle eviction |
| `ZVEC_GREP_EMBEDDING` | Default model for new indexes |
| `ZVEC_GREP_API_KEY` | Embedding provider API key |
| `ZVEC_GREP_ENDPOINT` | Remote Embedding endpoint |
| `ZVEC_GREP_MODEL_CACHE` | Local model cache directory |
| `ZVEC_GREP_DEVICE` | Local model device |
| `DASHSCOPE_API_KEY` | Qwen API-key fallback after `ZVEC_GREP_API_KEY` |
| `QWEN_API_KEY` | Qwen API-key fallback after `DASHSCOPE_API_KEY` |
| `QWEN_HOME` | Qwen Code configuration directory used by `zg --install` |
| `QODER_CONFIG_DIR` | Qoder CLI configuration directory used by `zg --install` |
| `QODER_IDE_MCP_PATH` | Full Qoder IDE `mcp.json` path used by `zg --install` |
| `QODER_IDE_EXECUTABLE` | Qoder IDE executable used for automatic install-target detection |

Run `zg --help environment` for advanced variables, agent integration paths,
scope, and detailed precedence. A new index selects its model in this order:
explicit `--embedding`, `ZVEC_GREP_EMBEDDING`, the global default, then the
built-in local default. Existing indexes continue to use their stored model
unless `--embedding` and `--rebuild` explicitly change it.

Embedding runtime values such as endpoint and device retain this order: explicit
command option, Workspace snapshot, global configuration, then environment.
`zg --index` forwards its `ZVEC_GREP_EMBEDDING` value in server and auto modes;
direct MCP calls use the environment inherited when the daemon started.
