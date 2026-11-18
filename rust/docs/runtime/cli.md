# CLI behavior

Use `zg --help [topic]` for the option reference. These notes describe current
search, presentation and setup behavior shared by Direct and Server modes.
See the [documentation index](../README.md) for related runtime contracts.

## Commands and query groups

Search directly with `zg "where authentication is validated"`, `zg --fts
"AuthService"`, or `zg --rg -F "AuthService" src`. Management uses flags:
`zg --index`, `zg --status`, `zg --server`, `zg --config`, `zg --auth`,
`zg --install`, and `zg --uninstall`. Bare words such as `query`, `index`, and
`help` are literal search text, not commands. Use `zg --help [topic]` (for example,
`zg --help search`) and `zg --version` for help and version information.

The Rust CLI joins positional words into one hybrid query:
`zg connection pool timeout` is equivalent to `zg "connection pool timeout"`.
Separately quoted positional arguments also merge: both
`zg "connection pool" "timeout"` and `zg "connection pool" "socket timeout"`
produce one query. When at least two nonempty positional arguments include a
phrase with internal whitespace, a warning on stderr explains the merge and
recommends explicit queries, such as
`zg --hybrid "connection pool" --hybrid "socket timeout"`.
Shell quote characters are not available to the CLI, so separately quoted single
words behave like unquoted words without a warning. Each positional argument is
trimmed, empty arguments are ignored, and the remaining parts are joined with one
space while preserving internal whitespace. Explicit
`--hybrid`, `--fts`, and `--vector` queries stay separate. Direct and Server modes
use the same parsing; managed `--rg` retains its pattern/path syntax.

Multi-query indexed output labels each nonempty group with its original query
and position, for example `-- query 2/3: socket timeout --`. Colored output uses
a dim rule, a magenta label, and a bold query in the terminal's default foreground
color, preserving contrast on light and dark backgrounds. Control characters become
spaces so each heading occupies one logical line; long queries remain complete
and may wrap in narrow terminals. Empty groups stay hidden. Single-query,
fused, managed-rg, and MCP output do not add these group headings.

## Indexed previews

Searches default to `--preview short` in terminals, pipes, and redirected files.
Use `none` for a one-line source summary, `short` for compact bounded source,
`full` for each complete indexed entity, or `detail` for full previews with metadata,
outlines, and file/hit grouping. Short and full use the compact
per-hit layout, including indented `line: source` rows. Every preview preserves each
query group's results, ranks and matched content; only `--fuse` combines groups.
Query echoes, hit counts and `matchedBy` labels are omitted from the CLI result
display; multiple nonempty query groups have the headings described above.
Full previews use the complete stored entity (such as a Markdown section or
code symbol), even when only one fragment matched. They do
not expand to the entire file. Bounded previews remain anchored to the matched fragment.
Short previews prioritize fragment lines within the ten-line limit, using any
remaining space for surrounding entity context. In server mode, the daemon
returns complete entity content and the CLI applies the selected preview.
Short preview limits displayed text, not the internal response payload.
Metadata labels use purple, file locations use cyan, and query matches use yellow.
Automatic colors are disabled for redirected output, `TERM=dumb`, or `NO_COLOR`;
`--color always` explicitly enables colors, including through pipes, and
`--color never` disables them. `--color auto` is the default.

## First index and progress

When an interactive search finds no index, it asks you to choose an embedding
model before building. The picker has **Local models** and **Remote models** tabs:
downloaded local models and globally configured remote models appear first.
Use ↑/↓ to select, ←/→ or Tab to switch, Enter to confirm and Esc to exit.
Each tab keeps its selection when switching. A valid model preference selects the
initial tab; without one the picker opens Local models. Remote models show whether
credentials are available. Selecting one with missing credentials opens a masked
API Key prompt, with paste, Backspace, Ctrl+U to clear and Esc to exit. Enter saves
a non-empty key to the provider's global configuration and continues to remote
authorization. The new key is used for both the build and following CLI query.

Local selection starts the build. Remote selection first discloses the workspace,
model, endpoint, data sent and possible API charges, then offers Allow once, Allow
for this workspace, or Back to model selection. Existing matching workspace consent
skips this step. Allow once covers the CLI build and the following query without
persisting a grant. Esc exits either screen before any workspace content is sent.

CLI searches display model preparation/download and file indexing progress and wait
for the full index to be published before running the original search. This also
applies to `--mode server`, where the CLI follows the daemon's background job.
Non-interactive searches never create a missing index; initialize it explicitly
with `zg --index --embedding <model>` first. Empty workspaces do not download models.

Interactive indexing retains a compact record of each major stage. File counts
and model downloads update at most eight times per second; optimization, saving,
and cleanup share a stable "Finalizing index" heading. Fast stages remain in
scrollback without delaying indexing. Only the final CLI result reports readiness.
Index progress and status coverage use a green gradient; progress uses diamond
markers.
`NO_COLOR` and `--color never` disable color; redirected or dumb-terminal progress
remains line-oriented.

Incompatible indexes show a rebuild command and their directory. Use `--debug`
for internal diagnostics. See [model selection](models.md#model-selection) and
[remote authorization](authorization.md) for setup policy.

## Status

`zg --status` shows only **Coverage**, **Embedding**, **FTS**, and **Storage**.
Coverage compares the current scan with unchanged indexed files and includes
any non-ready state. Missing statistics show `Unavailable`; missing embedding
or FTS configuration shows `Not configured`. `--check-ready` keeps its existing
exit-code behavior. The daemon and MCP status APIs retain their full metadata.
`--debug` also writes the complete structured status snapshot to stderr.

## Path lookup

A single file name or path, such as `zg auth.ts` or `zg src/auth.ts`, first
looks up current file paths. Matches use `#1 src/auth.ts`, with
forward slashes on every platform and exact workspace-relative paths before suffix
matches. Ties use deterministic native path ordering; `--limit` bounds the list
(default: 10). Lookup uses the nearest workspace's saved scan rules and query
file filters, and includes empty files. It runs locally in direct, server, and
auto modes without loading a model, creating an index, or requiring a daemon.
File discovery does not apply index-only default exclusions for build directories,
lock files, or images; saved scanning rules and ignore files still apply.
File matches do not read or embed contents. If no file matches, lookup searches
the selected current source text literally using the same scan rules and filters.
A path mentioned only in source text returns those text matches. No match prints
`No matches.` instead of unrelated vector neighbors, even with an existing index.
Text results deduplicate complete structural entities before applying `--limit`;
multiple retrieval fragments of one function or Markdown section count once.
Literal matching and highlighting use the same smart-case rules: queries with
uppercase letters are case-sensitive; lowercase queries ignore case.
Results show line numbers, match/context markers, and structural headings. Terminal output also
shows coverage and per-file/per-hit metadata. Truncated file or text results warn
on stderr. Truncated text remains local; direct, server, and auto modes never require
model loading or indexing for path lookup. Native glob ordering, format aliases, ignore
handling, and path normalization retain the Rust CLI's existing semantics.
Explicit `--hybrid`, `--fts`, `--vector`, `--fuse`, symbol options, `--trace`, and
`--refresh` retain their indexed behavior. Text questions, URLs, and glob patterns
do not trigger automatic path lookup.

Automatic path lookup emits
`warning: automatically performing path lookup; use --hybrid to force hybrid search.`
on stderr only when the default query actually enters that branch. `--hybrid`
overrides the inference, including ambiguous inputs such as `config.timeout` and
`v1.2`.

When no file path matches and lookup falls back to literal source search, it also
prints `warning: no matching file paths; searching current file contents literally (rg).`
on stderr. This appears for both text matches and `No matches.`; stdout contains
only the search results.

## Managed lexical search

Rg requests return plain text matches and requested context without parsing code
or Markdown structure. CLI and MCP rg searches share this behavior; current-file
path lookup keeps its structural entity grouping.\nManaged `zg --rg` preserves literal pattern whitespace and supports empty
patterns with `-e ''`. Matching options include fixed strings (`-F`), case
selection (`-i`, `-s`, `-S`, with the last option winning), word/whole-line
matching (`-w`, `-x`), inversion (`-v`), multiline search (`-U`),
`--multiline-dotall`, `--crlf`, and text search through NUL bytes (`-a`).
Case-sensitive and insensitive glob rules (`-g`, `--iglob`) retain command-line
order. `-m` bounds matches per file and reports truncated coverage when additional
matches are omitted; `--limit` bounds the final result list. `-j 0` selects the
automatic thread count. Ignore controls include `-u`/`-uu`/`-uuu`,
`--no-ignore-dot`, `--no-ignore-files`, `--no-ignore-global`,
`--no-ignore-parent`, and `--no-ignore-vcs`. Managed search still excludes its
internal directories. The full MCP toolset uses the same argument parser and
engine. PCRE2, compressed-file search, explicit encoding selection, and native
ripgrep output-format switches other than file listing are rejected; Unicode
BOM decoding is automatic.

## File discovery

Use `zg --rg --files` for explicit file discovery:

```sh
zg --rg --files
zg --rg --files -g 'auth.ts'
zg --rg --files src -g '*.ts' -g '!*.test.ts'
zg --rg --files -t rust -0
```

Every positional argument is an input file or directory, so `auth.ts` by itself
selects that input path; `-g 'auth.ts'` finds that name throughout the inputs.
Discovery reuses managed rg's glob, type, ignore, hidden, symlink, depth, size,
and modification-time filters. It reads filesystem metadata and ignore rules,
without opening source contents, loading models, or opening an index. All client
modes run locally. Output contains all selected paths in sorted order, one per
line or NUL-delimited with `-0`/`--null`, without headings or match metadata.
Absolute inputs retain absolute output paths; relative inputs retain relative
paths. `--limit` is rejected to avoid silently truncating the enumeration.
Exit status is 0 when files are found, 1 for an empty list, and 2 for a discovery
error; traversal errors go to stderr while accessible paths remain on stdout.
Like managed content search, recursive discovery skips `.git` and `.zvec-grep`;
explicit file inputs are still listed.
This is a CLI entry point; the MCP rg tool retains its content-search contract.

## Call graph queries

`zg --callers <symbol> [root]` and `zg --callees <symbol> [root]` query an existing
workspace graph. CLI `-g`/`--glob` and `--iglob` use the same ordered,
workspace-relative glob rules as ordinary search (for example, `-g 'src/**'`).
They filter definitions and suggestions, while call-site files remain unrestricted.
Use `--limit N` to cap call sites per matching definition and
`--mode direct|server|auto` to select execution. Scope-qualified names such as
`Class::method` are supported. Same-name definitions remain separate, and repeated
call sites retain their locations; `truncated` indicates additional visible call
sites beyond the per-symbol limit. Endpoint visibility is checked before the limit;
queries do not compute an exact total. Each definition and its call sites share a
read snapshot; indexing may advance between definitions. Definitions changed since
initial selection are skipped until the next query.
Each edge has a caller `filePath` independently of its optional `symbol` metadata.
Both MCP toolsets expose `zvec_grep_callers` and `zvec_grep_callees` with an absolute `root`, `symbol`,
and optional `globs`, `insensitiveGlobs`, and `limit`, using the same glob inputs as
search. These queries do not load models or refresh the index.
Graph extraction and resolution must have populated the index for call edges to
be available; unresolved references are excluded from results.
