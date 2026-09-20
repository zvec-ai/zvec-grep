# File selection

Domain `GlobRule` defines an ordered path pattern using ripgrep's command-line glob semantics. `GlobMatcher` compiles a slice of these rules relative to a supplied root without filesystem access. Workspace scanning and indexed queries both resolve patterns against the workspace root.

`Workspace.scan` owns `ScanRules`, defined alongside `Workspace` in `domain/workspace.rs`. These rules contain globs and hidden, ignore, nested-repository, symlink, depth, and size settings. Indexing and watcher registration consume this value, and the manifest persists it directly. API `ScanRulesUpdate` changes only supplied fields.

API `QueryFilter` defines temporary indexed-search conditions: globs, formats, categories, modification-time bounds, and symbol types. Indexed search evaluates file attributes from the stored snapshot, with equivalent storage predicates where possible. Queries never change saved scan rules or read current ignore files or source headers. Format and category selection is available only for queries; indexing still detects formats to select extraction and processing limits.

`ScanPolicy` combines the matcher with persisted scanning settings, built-in exclusions, and hierarchical `.gitignore`, `.ignore`, and `.rgignore` rules. Native scanning and watcher registration consume this policy through `PathPolicy`; the native crate does not interpret globs or formats. Direct embedded-ripgrep search keeps its independent selection implementation.

Explicit glob matches override ordinary ignore/hidden rules. Positive file patterns do not prune nonmatching directories; excluded ancestors remain unreachable. Incremental scopes obey the same ancestor rules as full scans. `ScanRules::nested_git` defaults to true; false makes child `.git` directories and files (including submodules and worktrees) traversal boundaries, even for explicit globs and `no_ignore`. The workspace root is exempt. `.git` and `.zvec-grep` metadata are always excluded.

Repository-marker lookups use a bounded directory cache when nested traversal is disabled. Rejected repository roots retain a nonrecursive control watch so deleting `.git` can restore traversal; creating `.git` invalidates the cache and removes that subtree from the index and deeper watch registrations. This setting applies to indexing and its watcher, independently of embedded-ripgrep queries.

`no_ignore` disables built-in and discovered ignore rules; explicitly configured ignore files remain active. Missing ignore files act as empty rule files and their parent directories are watched for creation. Ignore-file symlinks track both the alias and target, independently of whether source symlinks are followed. Changes to control files invalidate the bounded rule cache, reconcile files, and update directory registrations. Recovery after missed events also invalidates cached rules. Deletions and old rename paths are retained for index cleanup even if current rules would exclude those paths.

Glob compilation allows at most 1,024 rules, 4,096 bytes per pattern, and 1 MiB total pattern text. Each ignore file is limited to 1 MiB and 16,384 lines. Ignore caches retain at most 4,096 entries and 4 MiB of source rule text. Invalid or excessive rules fail explicitly. Matchers use the Rust `ignore`/`globset` implementation, avoiding JavaScript-style backtracking regex compilation.

Query formats and categories are inferred from stored file names. Queries read the files collection's path projection, adding modification times when needed, instead of loading file payloads. Content-aware format detection remains local to indexing and extraction; file records do not persist detected formats.
