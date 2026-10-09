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

Run `zg --install` to configure a supported agent automatically. Use this page
when building another MCP client or when you need the exact boundary between
the default and compatibility toolsets. See
[Server and execution modes](./06-server.md) for lifecycle, mode selection,
refresh, authentication, and logs.

To relocate a workspace or transfer an existing index, see
[Move and reuse an index](./09-portable-indexes.md). The current MCP tools
can search and update a portable index. The full toolset also exposes migration, export and import. Paths always refer to files visible to the server; copying an
artifact between hosts is a separate operation.

## Default agent toolset

The default `agent` toolset intentionally exposes only search:

Agents first decide whether the requested answer should be grounded in the
current indexed workspace, then choose exact or semantic retrieval. The same
rules apply to source code and non-code material such as documentation, books,
research material, meeting notes, knowledge-base exports, manuals,
configuration, and data.

Workspace relevance requires a request to inspect, search, or ground the answer
in local material, prior context that established local material as the intended
source, or a question about whether relevant local material exists. Negative,
incidental, or comparative workspace mentions do not establish relevance.

| Tool | Use it when | Index required |
| --- | --- | --- |
| `zvec_grep_search` | The answer is workspace-grounded and wording or location is unknown, or semantic, fuzzy, relationship, chronology, causality, comparison, or cross-file synthesis is required | Yes |

Agents use native grep or rg when locating an exact word, quotation, name, date,
key, filename, path, source fragment, or regex is sufficient. For mixed tasks,
start with `zvec_grep_search`, then use native grep or rg for focused follow-up.
When semantic discovery is selected because no sufficient exact anchor is
available and the user asks whether conceptually related material exists
locally, agents make at most one focused search probe and stop when its results
are not relevant. The probe does not apply to exact quotations, configuration
keys, filenames, regexes, or exhaustive occurrence requests. Unrelated
open-world knowledge, current external facts, and web content that does not
depend on local evidence use the appropriate external source instead.

Every workspace tool input uses an absolute `root` visible to the daemon.

## `zvec_grep_search`

The indexed tool supports hybrid, lexical, and vector query groups. At least one
of `query`, `queries`, `fts`, or `vector` is required.

Minimal conceptual search:

```json
{
  "root": "/absolute/path/to/workspace",
  "query": "decision history behind the launch date",
  "limit": 5
}
```

Explicit query routes and scope:

```json
{
  "root": "/absolute/path/to/workspace",
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
| `fts` | Supplemental lexical retrieval groups, not hard constraints or exhaustive occurrence lookup |
| `vector` | Semantic-only query groups |
| `fuse` | Combine every group into one ranked plan |
| `limit` | Maximum items per group, up to 50 |
| `preview` | `short` (default) for bounded snippets, or `full` for all available retrieved-item content; affects display only |
| `globs` / `insensitiveGlobs` | A string or list of ordered path rules; insensitive rules follow globs |
| `fileTypes` / `excludedFileTypes` | ripgrep file-type filters |
| `symbolTypes` / `preferSymbol` | Indexed symbol controls |
| `modifiedAfter` / `modifiedBefore` | File modification-time bounds |
| `freshness` | `eventual` or `wait_for_fresh` |
| `autoUpdate` | Allow an eventual search to schedule a background update |

`preview: "full"` preserves all available source lines and line lengths of each
retrieved item, plus its available outline. It does not read the entire file,
change retrieval or ranking, or recover content omitted during extraction.
Both the default `agent` and compatibility `full` toolsets accept this parameter;
the preview value is independent of the toolset name.

The response is text designed for agent context. It begins with index
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

This tool is retained in the optional `full` MCP toolset and is not registered
in the default `agent` toolset. The CLI equivalent, `zg --rg`, remains
available without changing the MCP toolset.

Pass the ripgrep command you would otherwise run. The command is parsed into
arguments and is never executed by a shell:

```json
{
  "root": "/absolute/path/to/workspace",
  "command": "rg -n -F 'loadTheme' -g '*.ts' src"
}
```

The tool is exhaustive by default. Append `| head -N` only when intentionally
requesting bounded output:

```json
{
  "root": "/absolute/path/to/workspace",
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
zg --server off
zg --server on --mcp-toolset full
```

The `full` toolset exposes nine tools:

| Tool | Purpose |
| --- | --- |
| `zvec_grep_search` | Indexed retrieval |
| `zvec_grep_rg` | No-index exhaustive search |
| `zvec_grep_index` | Create, update, rebuild, or explicitly drop an index |
| `zvec_grep_index_drop` | Explicitly delete an index |
| `zvec_grep_index_migrate` | Migrate a legacy index into a portable workspace index |
| `zvec_grep_index_export` | Write a logical index artifact with stored vectors |
| `zvec_grep_index_import` | Create native storage from a received artifact |
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

## Rust compatibility notes

Rust uses the same search routing, preview and freshness rules in `agent` and
`full`. Search credentials and device selection come from the configured runtime;
public search does not accept `apiKey` or `device`. Index accepts runtime overrides,
`follow`, ordered globs and native ripgrep type filters, and defaults to `wait: false`.
Completed debug indexing returns `scan_diagnostics` alongside extended diagnostics.

The Rust indexed-search API rejects scanning options (`hidden`, `noIgnore`,
`ignoreFiles`, `maxDepth`, `maxFileSizeBytes`, `follow`); change these through index.
The Node indexed-search implementation currently ignores them. Rust also rejects
unknown search/index fields rather than silently discarding them. Retained extensions
and review conditions are listed in
[the compatibility registry](../rust/compat/allowed-differences.toml).

With protocol `2026-07-28`, remote permission returns `input_required`; resend the
same tool arguments and `requestState`, with the accepted decision under
`inputResponses.remote_embedding_authorization.content.decision`. Decisions are
`allow_once`, `allow_workspace`, `use_local_search` (search only), or `cancel`.
States expire after ten minutes and cannot be replayed. A daemon restart requires
a new prompt. Older clients use reverse elicitation. No protected data is sent
before consent, and FTS-only disables both vectors and refresh.

HTTP discovery and tool lists advertise a one-hour private cache. Older HTTP
sessions have a 256-session limit and 30-minute idle expiry; active requests remain
protected. Closing stdio leaves the shared daemon running. Both transports are
covered by the normal Rust workspace tests and the three-platform Rust CI matrix.

## Portable index operations (full toolset)

The default `agent` toolset remains search-only. Start the server with
`--mcp-toolset full` to expose these operations on its existing MCP endpoint.
Use them only after an explicit user request. Each call requires `confirm: true`.
This field records the caller's confirmation; it does not prove user identity
or replace server authentication.

| Tool                      | Required paths, all absolute and visible to the server                                             |
| ------------------------- | -------------------------------------------------------------------------------------------------- |
| `zvec_grep_index_migrate` | `sourceHome`: legacy index directory; `destinationRoot`: workspace receiving the v2 index          |
| `zvec_grep_index_export`  | `sourceHome`: index directory; `artifactPath`: new artifact directory                              |
| `zvec_grep_index_import`  | `artifactPath`: received artifact directory; `destinationRoot`: workspace receiving native storage |

Example export call:

```json
{
  "name": "zvec_grep_index_export",
  "arguments": {
    "sourceHome": "/work/project/.zvec-grep",
    "artifactPath": "/transfer/project-index",
    "confirm": true
  }
}
```

Copy the complete artifact and documents to the other host as a separate
operation. Import with that host's paths:

```json
{
  "name": "zvec_grep_index_import",
  "arguments": {
    "artifactPath": "/received/project-index",
    "destinationRoot": "/work/project",
    "confirm": true
  }
}
```

Results include `operation`, `state` and `result`. Migration and import return
index identity, counts, missing files and verification results. The default
missing-file sample has at most 20 paths and 4096 path characters.
`missingFilesCount` gives the total. `missingFilesTruncated` identifies a sample.
Set `includeAllMissingFiles: true` to request the full list. MCP compares
all vectors. `vectorsExact` and `vectorsPreserved` are separate results.
Cosine storage permits at most two float32 steps per component. Other metrics
require exact equality. Export returns identity and counts. Failures set `isError: true`
and return `state: "failed"` with an error code, message and available context.
Input validation failures use the standard MCP validation error.

Clients can request MCP progress notifications. Native storage operations run
in a separate process so that the server can receive cancellation. A fatal
native failure stops that process and returns a tool error. Before publication,
cancellation uses the engine's reservation cleanup. Publication is the commit
boundary: a late cancellation does not remove a completed index or artifact.
A forced process kill is different from a cancellation request; incomplete
markers and locks can require recovery after all writers have stopped. Do not
remove them while another process owns them.

These operations use existing engine locks. A busy source or occupied
destination can fail; they do not stop another user's indexing job or replace
an existing index. Retain the source, artifact and results until verification
is complete. No transfer operation computes document embeddings. See the
[move guide](./09-portable-indexes.md) for reconciliation and model requirements.
