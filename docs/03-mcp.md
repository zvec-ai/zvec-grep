# MCP guide

[Documentation](./README.md) · [Agents](./01-agents.md) ·
[CLI](./02-cli.md) · [MCP](./03-mcp.md) · [Pipeline](./04-pipeline.md) ·
[Architecture](./05-architecture.md) · [Server](./06-server.md) ·
[Embedding](./07-embedding.md) · [Roadmap](./08-roadmap.md)

zvec-grep exposes its local search layer over Streamable HTTP MCP. The normal
endpoint is:

```text
http://127.0.0.1:7999/mcp
```

Run `zg install` to configure a supported agent automatically. Use this page
when building another MCP client or when you need the exact boundary between
the default and compatibility toolsets. See
[Server and execution modes](./06-server.md) for lifecycle, mode selection,
refresh, authentication, and logs.

## Default agent toolset

The default `agent` toolset intentionally exposes only search:

| Tool | Use it when | Index required |
| --- | --- | --- |
| `zvec_grep_search` | The intent is known but the exact word, symbol, filename, or path is not | Yes |
| `zvec_grep_rg` | An exact word, symbol, filename, path, source fragment, or regex is known | No |

Agents should remain within these two zg tools. If a result is too broad, they
should narrow the query, path, glob, or file type and try again.

Every repository tool input uses an absolute `root` visible to the daemon.

## `zvec_grep_search`

The indexed tool supports hybrid, lexical, and vector query groups. At least one
of `query`, `queries`, `fts`, or `vector` is required.

Minimal conceptual search:

```json
{
  "root": "/absolute/path/to/repository",
  "query": "theme preference persistence on startup",
  "limit": 5
}
```

Explicit query routes and scope:

```json
{
  "root": "/absolute/path/to/repository",
  "query": "authentication flow",
  "fts": ["AuthService", "ForbiddenError"],
  "globs": ["src/**", "!src/generated/**"],
  "fileTypes": ["ts"],
  "fuse": true,
  "limit": 10
}
```

Important inputs:

| Input | Meaning |
| --- | --- |
| `query` | One hybrid natural-language or exact query |
| `queries` | One or more hybrid query groups |
| `fts` | Ranked lexical query groups |
| `vector` | Semantic-only query groups |
| `fuse` | Combine every group into one ranked plan |
| `limit` | Maximum items per group, up to 50 |
| `globs` / `insensitiveGlobs` | Ordered path rules |
| `fileTypes` / `excludedFileTypes` | ripgrep file-type filters |
| `symbolTypes` / `preferSymbol` | Indexed symbol controls |
| `modifiedAfter` / `modifiedBefore` | File modification-time bounds |
| `freshness` | `eventual` or `wait_for_fresh` |
| `autoUpdate` | Allow an eventual search to schedule a background update |

The response is compact text designed for agent context. It begins with index
state and then groups ranked results by file:

```text
freshness: fresh
src/theme/use-theme.ts:12-36
matched: 16-18
source:
15  export function useTheme() {
16    const [theme, setTheme] = useState("light");
17    useEffect(() => saveTheme(theme), [theme]);
```

When `freshness` is `possibly_stale`, the response may also include current
indexing state. Agents can use sufficient results immediately rather than
running a status preflight.

Remote models may cause the tool to request explicit Remote Embedding
authorization. See
[Embedding models](./07-embedding.md#remote-embedding-and-authorization).

## `zvec_grep_rg`

Pass the ripgrep command you would otherwise run. The command is parsed into
arguments and is never executed by a shell:

```json
{
  "root": "/absolute/path/to/repository",
  "command": "rg -n -F 'loadTheme' -g '*.ts' src"
}
```

The tool is exhaustive by default. Append `| head -N` only when intentionally
requesting bounded output:

```json
{
  "root": "/absolute/path/to/repository",
  "command": "rg -n 'TODO|FIXME' src | head -50"
}
```

Scope broad searches with command paths, `-g/--glob`, or `-t/--type`. Managed rg
supports common ripgrep matching, context, type, glob, ignore, encoding, and
regex-engine options while preserving zvec-grep's compact result format.

## Full compatibility toolset

The CLI owns index lifecycle and diagnostics, so agents normally do not need
administrative MCP tools. Clients that require them can restart the server with:

```bash
zg server off
zg server on --mcp-toolset full
```

The `full` toolset exposes six tools:

| Tool | Purpose |
| --- | --- |
| `zvec_grep_search` | Indexed retrieval |
| `zvec_grep_rg` | No-index exhaustive search |
| `zvec_grep_index` | Create, update, rebuild, or explicitly drop an index |
| `zvec_grep_index_drop` | Explicitly delete an index |
| `zvec_grep_index_status` | Inspect persisted and active index state |
| `zvec_grep_server_status` | Inspect daemon, queue, runtime, and model-pool state |

`zvec_grep_index` requires an absolute root. Its `wait` input defaults to
`false`, returning a background job identifier. Poll `zvec_grep_index_status`
only when completion, progress, failure diagnosis, or explicit monitoring is
needed. An agent must never silently create, rebuild, or delete a persistent
index.

Set `ZVEC_GREP_MCP_TOOLSET=full` as an environment fallback. An explicit
`--mcp-toolset` flag takes precedence.

## Transport security

The MCP endpoint is loopback-only. Optional Bearer authentication protects the
local Server but remains independent of Embedding provider credentials and
Remote Embedding authorization. Configuration examples are in
[Server authentication](./06-server.md#bearer-authentication).
