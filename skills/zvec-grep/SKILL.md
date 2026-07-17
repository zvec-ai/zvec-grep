---
name: zvec-grep
description: Repository code search and indexing with zvec-grep. Use when exploring a codebase, locating symbols, references, or exact text, creating or inspecting a repository index, diagnosing the daemon, or whenever repository investigation would otherwise use grep, rg, or broad file reads.
---

# zvec-grep

## Select the transport

Use zvec-grep before raw `grep` or `rg` for repository investigation.

Use native HTTP MCP tools as the primary interface when the matching `zvec_grep_*` tool is present. Call it directly; do not run `zg`, probe the daemon through shell, or choose CLI for convenience.

Use CLI fallback only when one of these conditions is true:

- The required MCP tool is absent from the current task.
- MCP initialization, authentication, connection, or the required call has failed.
- The repository has no index, the user has not authorized indexing, and an explicit no-index lexical search is needed.

Do not retry a submitted indexing write through another transport after a connection interruption.

## Use the MCP workflow

Pass the repository's daemon-visible absolute path as `root` on every repository call.

1. Call `zvec_grep_search` first for repository investigation. Search defaults to `freshness: "eventual"`; use `freshness: "wait_for_fresh"` only when the result must include all pending changes. Use hybrid `queries` for concepts, `fts` for exact lexical anchors, and `vector` for semantic-only intent.
2. Read the search response's `freshness` and `indexing` fields. Use `possibly_stale` results immediately when they are sufficient; do not call status merely because a background update is active.
3. Call `zvec_grep_index_status` only when search reports a missing index, indexing failed or was cancelled, or explicit progress monitoring and diagnostics are required.
4. Call `zvec_grep_rg` for exhaustive local ripgrep when an index is missing and the task can be answered with literal or regex search, or when the user explicitly requests rg mode.
5. Apply focused path and file-type filters early. Exclude dependencies, generated output, caches, build artifacts, fixtures, and logs unless the task concerns them.
6. Call `zvec_grep_index` only when the user requests persistent indexing or index deletion. Never silently create, rebuild, or drop an index. For a new index, use a user-selected embedding, or omit it only when a server default is known; never guess a model. Its `wait` parameter defaults to false: submit the job in the background. Poll `zvec_grep_index_status` only when completion, progress monitoring, or diagnostics are required; set `wait: true` only when completion is required before continuing. Its `drop` parameter deletes the workspace index and must be requested by the user.
7. Call `zvec_grep_server_status` only for daemon diagnostics, not before ordinary searches.

If the index is missing, explain that indexed search requires an index and ask before creating one. For exhaustive literal or regex search without an index, use `zvec_grep_rg` when present; otherwise use the explicit no-index CLI fallback.

Use multiple queries when comparing related concepts.

## Use CLI fallback

Read [references/cli-fallback.md](references/cli-fallback.md) only after a fallback condition above is satisfied. Leave CLI mode unset for ordinary status, search, and authorized indexing commands so the default Auto mode can select Server or Direct; do not probe forced Server mode and then retry forced Direct mode. Keep the selected transport consistent for the investigation unless its availability changes.
