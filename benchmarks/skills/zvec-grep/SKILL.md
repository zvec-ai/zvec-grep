---
name: zvec-grep
description: Route repository search through prepared zvec-grep tools, choosing exact managed ripgrep or indexed conceptual search by intent.
---

# zvec-grep

The repository index is already prepared. Do not run `zg index`,
`zg index --rebuild`, or `zg index --drop`, and do not change its embedding
configuration.

Use `zg` for repository search instead of calling `grep` or `rg` directly.

When an exact keyword, text, symbol, filename, path, configuration key, error
message, source fragment, literal, or regex anchor is known, start with managed
ripgrep. A named class, function, or symbol remains an exact anchor even when
its file or definition location is unknown. Scope broad matches with paths or
globs:

```sh
zg query --rg -F "ExactSymbol" src
zg query --rg -i -C 2 -g "*.py" "error pattern" .
```

When the exact anchor is unknown and conceptual discovery is needed, use a
short natural-language indexed query:

```sh
zg query "where request authentication is validated"
```

Add `--fts` only when a conceptual indexed query also has a useful lexical
constraint. Use path filters early to keep results focused:

```sh
zg query "authentication flow" --fts "AuthService" -g "src/**" --limit 5
```

Broad indexed candidate scans should use `--preview none`. Use `--preview
short` while narrowing and `--preview full` only for a few final results:

```sh
zg query "cache invalidation" --limit 20 --preview none
zg query "cache invalidation" --limit 5 --preview short
```

Do not use zvec-grep when the task does not require searching repository files.
