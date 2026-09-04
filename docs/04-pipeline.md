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

For the common case, run a search from the repository root. If no index exists,
zg creates one there with a local embedding model and then completes the search:

```bash
cd your-repository
zg "where authentication is validated"
```

Use `--index` when you need to select a model or constrain the workspace before
the first search:

```bash
cd your-repository
zg --index --embedding local/potion-code-16m-v2

# Equivalent with an explicit root
zg --index /absolute/path/to/your-repository \
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
zg --index \
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

Without an explicit `--max-filesize`, indexing uses type-aware safety limits:
1 MiB for code, 256 MiB for text and Markdown, 16 MiB for structured data, and
10 MiB for images. An explicit value replaces the type-aware defaults for every
selected file. Files excluded by these limits remain silent during normal
indexing; use `zg --index --debug` to print skipped-file counts and samples.

### Supported formats and extraction

The scanner assigns each admitted file to one extraction path. Structure-aware
extractors preserve useful code symbols or Markdown sections. When structure is
not available, zg falls back to plain-text chunks so the file can still
participate in indexed search.

| Files | Formats | Extractor | Indexed representation |
| --- | --- | --- | --- |
| Structure-aware code | C/C++ (`.c`, `.cc`, `.cpp`, `.cxx`, `.h`, `.hpp`), Go, Java, JavaScript/JSX, TypeScript/TSX, Python, Rust | `CodeExtractor` | Symbols, signatures, breadcrumbs, and surrounding source |
| Component scripts | `.vue`, `.svelte` | `CodeExtractor` | JavaScript or TypeScript `<script>` blocks; plain-text fallback when no structure is found |
| Other recognized code | Ruby, PHP, Swift, Kotlin, C#, Scala, shell, SQL, CSS/SCSS/Less, `Dockerfile`, `Makefile` | `CodeExtractor` | Plain-text chunks until a structural grammar is available |
| Markdown | `.md`, `.mdx` | `MarkdownExtractor` | Heading sections and breadcrumbs; plain-text fallback for documents without headings |
| Text documents | `.txt`, `.rst`, `.html`, `.htm`, `.xml` | `TextExtractor` | Plain-text chunks |
| Text data | `.csv`, `.json`, `.jsonc`, `.toml`, `.yaml`, `.yml` | `TextExtractor` | Plain-text chunks |
| Other non-binary files | Unrecognized extensions that pass binary detection | `TextExtractor` | Plain-text chunks |
| Raster images | `.gif`, `.jpeg`, `.jpg`, `.png`, `.webp` | `ImageExtractor` | Image content when explicitly included and the selected Embedding model accepts images |

Raster images are excluded by the default discovery rules and must be selected
explicitly. A text-only Embedding model cannot add image fragments to its
vector index.

The following binary formats are currently skipped before extraction:

- documents: `.pdf`, `.doc`, `.docx`, `.ppt`, `.pptx`, `.xls`, `.xlsx`;
- archives: `.zip`, `.tar`, `.gz`, `.bz2`, `.xz`, `.7z`, `.rar`;
- executables and compiled artifacts: `.exe`, `.dll`, `.dylib`, `.so`, `.a`,
  `.o`, `.obj`, `.wasm`, `.class`, `.jar`;
- media and databases: `.mp3`, `.mp4`, `.mov`, `.avi`, `.mkv`, `.db`,
  `.sqlite`.

Empty files, files above the configured size limit, and files detected as
binary are also skipped. Skipped files do not become extraction failures and
are not included in the current `filesScanned` count, so the index summary does
not list each skipped path or reason.

<a id="indexing"></a>

## 2. Build and maintain the index

A manually created index resolves its model from explicit `--embedding`,
`ZVEC_GREP_EMBEDDING`, the configured default, then the built-in local default.
An implicit first-search index always uses a local model. Existing indexes reuse
their stored model and file-selection settings:

```bash
# First build
zg --index --embedding local/potion-code-16m-v2

# Incremental update with the stored schema
zg --index
```

Use `zg --status` to see the root, selected model, file counts, failures,
truncation, and the suggested next action:

```bash
zg --status
zg --status --check-ready
```

Changing the Embedding model or an incompatible endpoint requires an explicit
rebuild:

```bash
zg --index --rebuild --embedding local/jina-embeddings-v2-base-code
```

Use `--reset-paths` when the existing file-selection settings should be
replaced rather than reused. Deleting an index is explicit and destructive:

```bash
zg --index --drop --yes
```

See [Embedding models](./07-embedding.md) before choosing or changing a model.

<a id="querying"></a>

## 3. Query through one search layer

The shortest query uses hybrid ranked retrieval:

```bash
zg "where theme preferences are restored"
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
zg --fts "AuthService"

# Explicit semantic search
zg --vector "where credentials are validated"

# Combine and fuse several query groups
zg \
  --hybrid "authentication flow" \
  --fts "ForbiddenError" \
  --fuse \
  --limit 10

# No index required
zg --rg -n -F "AuthService" -g "*.ts" src
```

Multiple positional queries remain separate groups unless `--fuse` is set.
Use `-g`, `--iglob`, `-t`, and `-T` on indexed queries to narrow results. Managed
rg also accepts common ripgrep matching, context, engine, encoding, discovery,
glob, and type options.

zg owns the managed-rg result format, so output-changing options such as
`--json`, `--count`, `--files`, `-l`, `-o`, `--replace`, and `--vimgrep` are
rejected. Use `-A`, `-B`, or `-C` to add context.

## Freshness

Indexed results report `fresh` or `possibly_stale`. Routine reconciliation stays
`fresh` until there is evidence of index drift. See
[Server and execution modes](./06-server.md#refresh-behavior) for the interaction
between `auto`, `server`, `direct`, and `--refresh`.

## Output for agents and people

When stdout is a terminal, indexed CLI output is human-readable and includes a
full source preview by default. When stdout is redirected, output is compact,
grouped by query group, and omits previews unless requested:

```bash
zg "plugin lifecycle" --preview short --limit 5
```

Use `--compact` to request the pipe-oriented form even in a terminal:

```bash
zg --compact "plugin lifecycle" --limit 5
```

Use `--debug` for query diagnostics and `--trace` for per-hit indexed search
trace information.
