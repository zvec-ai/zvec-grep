# Retrieval pipeline

[Documentation](./README.md) · [Agents](./01-agents.md) ·
[CLI](./02-cli.md) · [MCP](./03-mcp.md) · [Pipeline](./04-pipeline.md) ·
[Architecture](./05-architecture.md) · [Server](./06-server.md) ·
[Embedding](./07-embedding.md) · [Roadmap](./08-roadmap.md)

zg provides one search layer across semantic discovery, ranked lexical search,
and exhaustive ripgrep. The normal flow is:

```text
workspace → file discovery → local index → query routes → compact results
```

The index combines lexical and vector retrieval. Managed ripgrep follows the
same workspace and output conventions but can run without an index.

## 1. Choose the workspace scope

Run indexing from the repository root, or pass it explicitly:

```bash
cd your-repository
zg index --embedding local/potion-code-16m-v2

# Equivalent with an explicit root
zg index /absolute/path/to/your-repository \
  --embedding local/potion-code-16m-v2
```

The workspace index is stored under `<root>/.zvec-grep/`. `.git` and
`.zvec-grep` are always excluded. Common dependency, build, generated, cache,
and log directories are excluded by default, as are files ignored by the
repository's ignore rules.

The main workspace files are `manifest.json`, `files.zvec`, and `index.zvec`.
The manifest stores index metadata and the workspace Embedding runtime settings,
including an API key when one was explicitly persisted for that workspace.

Scope large repositories early:

```bash
zg index \
  --embedding local/potion-code-16m-v2 \
  -g "src/**" \
  -g "docs/**" \
  -g "!dist/**" \
  -t ts
```

Useful discovery controls include:

| Option | Effect |
| --- | --- |
| `-g, --glob <glob>` | Add an ordered include or `!` exclude rule |
| `--iglob <glob>` | Add a case-insensitive glob rule |
| `-t, --type <type>` | Include a ripgrep file type |
| `-T, --type-not <type>` | Exclude a ripgrep file type |
| `--hidden` | Include hidden paths except `.git` and `.zvec-grep` |
| `--no-ignore` | Stop applying ignore files |
| `--ignore-file <path>` | Add an ignore file |
| `--max-depth <n>` | Limit recursive depth |
| `--max-filesize <size>` | Limit file size, for example `500K` or `2M` |
| `-L, --follow` | Follow symbolic links safely |

File-type filters narrow the result after glob rules. For example,
`-g "docs/**" -t ts` selects TypeScript files inside `docs`, not every file in
that directory.

<a id="indexing"></a>

## 2. Build and maintain the index

A new index needs an explicit model, `ZVEC_GREP_EMBEDDING`, or a configured
default. Existing indexes reuse their stored model and file-selection settings:

```bash
# First build
zg index --embedding local/potion-code-16m-v2

# Incremental update with the stored schema
zg index
```

Use `zg status` to see the root, selected model, file counts, failures,
truncation, and the suggested next action:

```bash
zg status
zg status --check-ready
```

Changing the Embedding model or an incompatible endpoint requires an explicit
rebuild:

```bash
zg index --rebuild --embedding local/jina-embeddings-v2-base-code
```

Use `--reset-paths` when the existing file-selection settings should be
replaced rather than reused. Deleting an index is explicit and destructive:

```bash
zg index --drop --yes
```

See [Embedding models](./07-embedding.md) before choosing or changing a model.

<a id="querying"></a>

## 3. Query through one search layer

The shortest query uses hybrid ranked retrieval:

```bash
zg query "where theme preferences are restored"
```

Choose an explicit route only when you need more control:

| Route | Use it for | Coverage |
| --- | --- | --- |
| Positional query or `--hybrid` | Intent plus useful lexical anchors | Ranked sample |
| `--fts` | Exact terms ranked through the index | Ranked sample |
| `--vector` | Conceptual similarity without lexical ranking | Ranked sample |
| `--rg` | Exhaustive literal or regex matching | Exhaustive unless explicitly bounded |

Examples:

```bash
# Ranked lexical search
zg query --fts "AuthService"

# Explicit semantic search
zg query --vector "where credentials are validated"

# Combine and fuse several query groups
zg query \
  --hybrid "authentication flow" \
  --fts "ForbiddenError" \
  --fuse \
  --limit 10

# No index required
zg query --rg -n -F "AuthService" -g "*.ts" src
```

Multiple positional queries remain separate groups unless `--fuse` is set.
Use `-g`, `--iglob`, `-t`, and `-T` on indexed queries to narrow results. Managed
rg also accepts common ripgrep matching, context, engine, encoding, discovery,
glob, and type options.

zg owns the managed-rg result format, so output-changing options such as
`--json`, `--count`, `--files`, `-l`, `-o`, `--replace`, and `--vimgrep` are
rejected. Use `-A`, `-B`, or `-C` to add context.

## Freshness

Indexed results report `fresh` or `possibly_stale`. A possibly stale result can
remain immediately useful while an update is pending. See
[Server and execution modes](./06-server.md#refresh-behavior) for the interaction
between `auto`, `server`, `direct`, and `--refresh`.

## Output for agents and people

Default CLI output is compact and grouped by file. Indexed source previews are
omitted unless requested, reducing context passed to an agent:

```bash
zg query "plugin lifecycle" --preview short --limit 5
```

For terminal reading, `--human` enables richer presentation and defaults to a
full preview:

```bash
zg query --human "plugin lifecycle" --limit 5
```

Use `--debug` for query diagnostics and `--trace` for per-hit indexed search
trace information.
