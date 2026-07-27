---
name: zvec-grep
description: Repository code search and indexing with zvec-grep. Use when exploring a codebase, locating symbols, references, or exact text, creating or inspecting a repository index, diagnosing the daemon, or whenever repository investigation would otherwise use grep, rg, or broad file reads.
---

# zvec-grep

## Select the transport

Route repository investigation through zvec-grep instead of raw `grep` or
`rg`. Choose the MCP tool by search intent; this does not mean indexed search
must run before an exact search.

Use the public native HTTP MCP search tools as the primary interface when the matching `zvec_grep_*` tool is present. Call them directly; do not run `zg`, probe the daemon through shell, or choose CLI for convenience.

Use CLI fallback only when one of these conditions is true:

- The task requires index lifecycle or daemon diagnostics, which are intentionally
  kept out of the default agent MCP toolset.
- An available MCP search tool cannot perform the required operation.
- MCP initialization, authentication, connection, or the required search call has
  failed.
- The repository has no index, the user has not authorized indexing, and an explicit no-index lexical search is needed.

Do not retry a submitted indexing write through another transport after a connection interruption.

## Use the MCP workflow

Pass the repository's daemon-visible absolute path as `root` on every repository call.

1. Call `zvec_grep_rg` first when an exact keyword, text, symbol, filename,
   path, configuration key, error message, source fragment, literal, or regex
   anchor is known. A named class, function, or symbol remains an exact anchor
   even when its file or definition location is unknown. Scope broad matches
   with paths or globs.
2. Call `zvec_grep_search` when the exact anchor is unknown and conceptual
   discovery is needed. Search defaults to `freshness: "eventual"`; use
   `freshness: "wait_for_fresh"` only when the result must include all pending
   changes. Use hybrid `queries` for concepts plus known lexical constraints,
   `fts` for indexed lexical-only intent, and `vector` for semantic-only intent.
3. Read the indexed search response's `freshness` and `indexing` fields. Use
   `possibly_stale` results immediately when they are sufficient; do not call
   status merely because a background update is active.
4. Apply focused path and file-type filters early. Exclude dependencies,
   generated output, caches, build artifacts, fixtures, and logs unless the
   task concerns them.

The default public MCP endpoint intentionally exposes only search and managed
ripgrep. If the index is missing, explain that indexed search requires an index
and ask before creating one. After authorization, use the CLI lifecycle workflow;
never silently create, rebuild, or drop an index. For exhaustive literal or regex
search without an index, use `zvec_grep_rg` when present; otherwise use the
explicit no-index CLI fallback.

Use multiple queries when comparing related concepts.

## Use CLI fallback

Read [references/cli-fallback.md](references/cli-fallback.md) only after a fallback condition above is satisfied. Leave CLI mode unset for status, fallback search, and authorized indexing commands so the default Auto mode can select Server or Direct; do not probe forced Server mode and then retry forced Direct mode. Keep the selected transport consistent for the investigation unless its availability changes.
