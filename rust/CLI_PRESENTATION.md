# CLI presentation

Human-facing terminal output uses one visual language: a short status heading,
two-space detail indentation, neutral body text, and dim paths and timing.
Green marks success and progress; yellow marks attention; red marks failure.
Search content and machine-readable streams retain their existing contracts.

## Command coverage

The public syntax comes from `zg --help` and its topics. Bare words such as
`index` and `help` remain literal search text, not management commands.

| Entry point | Terminal presentation |
| --- | --- |
| `zg <query>`, `--hybrid`, `--fts`, `--vector`, `--fuse` | Preserve query groups, ranks, source and the four preview modes; neutral headings and shared match highlighting. |
| Automatic path-shaped query lookup | Preserve current-file results; use a warning heading and actionable detail for literal fallback or incomplete coverage. |
| `zg --rg …` | Preserve managed-ripgrep matching and result layout. |
| `zg --rg --files …`, `-0` / `--null` | Paths only, with existing newline/NUL separators and native path bytes. |
| `zg --index [root]`, `--rebuild` | One live line, then a compact result/count/path summary; failures remain visible. |
| `zg --index [root] --drop --yes` | Explicit confirmation flag, then a compact removal or no-index result; no interactive deletion prompt. |
| `zg --status [root] [--check-ready]` | Short state heading, matching block-style coverage bar, grouped metadata, nonzero pending/change counts and recovery guidance. |
| `zg --config provider set …`, `model set …` | Configuration acknowledgement, model/provider reference and dim configuration path; never print credentials. |
| `zg --auth grant …`, `status`, `revoke` | Authorization state, model and destination, workspace scope, and the next command when authorization is missing. |
| `zg --server on`, `off`, `status [--check-ready]` | Shared status heading and relevant process/connection details. |
| `zg --server run`, `--stdio` | Preserve daemon logging and MCP transport; never add presentation text to protocol stdout. |
| `zg --install [targets/options]` | Shared target picker and compact per-target installation results, connection status and next steps. |
| `zg --install --upgrade-installed [--dry-run]` | Clearly distinguish planned changes from completed upgrades; retain file, backup and failure details. |
| `zg --uninstall [targets/options]` | Shared target selection and compact per-target removal results. |
| `zg` with no arguments, `zg -h`, `--help [topic]` | Static usage text; no progress or success decoration. |
| `zg -v`, `--version` | Version only. |

Help topics: `search`, `index`, `status`, `config`, `auth`, `server`, `install`,
`uninstall`, `help`, `version`, `models`, `file-types`, and `environment` / `env`.
Install/uninstall targets remain `codex`, `claude`, `qwen`, `qoder`, `opencode`,
`cursor`, `copilot`, `vscode`, `grok`, `pi`, `auto`, and `all`.

## Interactive and automatic entry points

| Entry point | Behavior to preserve |
| --- | --- |
| Missing-index search setup | Local/remote model tabs, keyboard navigation, selection retention, Enter confirmation, and Esc/Ctrl-C cancellation. |
| Missing provider credential | Masked input, validation and explicit submission; cancellation neither saves credentials nor starts indexing. |
| Remote indexing authorization | Disclose the workspace, model and destination; require an explicit allow-once or workspace grant. |
| Remote query authorization | Preserve allow-once, workspace grant, FTS-only fallback and cancellation choices. |
| Install/uninstall agent selection | Preserve arrow/Enter/Esc controls and line-input fallback; dim secondary detection information. |
| Index deletion | Require the existing `--yes` flag before creating a deletion plan; omitting it is an error, not an interactive prompt. |
| Automatic indexing and refresh | Reuse the indexing display; show current-index freshness and real refresh counts on the diagnostic stream. |
| Parse errors, compatibility warnings and runtime errors | One concise heading plus the cause and available next step. Detailed error codes, locations and retryability remain available in debug/log output. |

## Progress and compatibility rules

- Use solid green blocks over a dotted track for measured progress. Optimization
  uses a moving segment and elapsed time; it does not invent a percentage.
- Keep finalization visible until storage, publication and CLI cleanup finish.
  File processing at 100% does not itself imply `Index ready`.
- Coalesce terminal updates before formatting, cap refreshes at eight per
  second, and write each complete frame together. Static summaries and errors
  render once. Restore cursor visibility on completion, cancellation and drop.
- Respect color policy and `NO_COLOR`; use plain fallbacks when terminal
  capabilities are unavailable. Non-TTY progress is throttled line-oriented
  logging, without animation or cursor movement.
- Preserve redirected index/status/config/auth/server fields, search content, ranks, file paths,
  NUL output, MCP/JSON contracts and command exit statuses. Cosmetic changes
  must not change readiness, retrieval, authorization or indexing semantics.
- Terminal diagnostics sanitize control characters. Keep warnings durable even
  when rapid progress events are coalesced.

## Verification targets

These are test references, not a claim that a particular build has passed.

| Area | Tests |
| --- | --- |
| Progress frames, throttling, heartbeat, cursor restoration and warnings | [progress.rs](crates/zg-cli/src/progress.rs), especially `terminal_coalesces_bursts_and_restores_cursor_on_drop`, `heartbeat_animates_optimization_without_producer_events`, `a_blocked_terminal_write_does_not_block_progress_producers` and `warnings_are_durable_complete_and_survive_coalescing`. |
| Status and summaries | [status.rs](crates/zg-cli/src/status.rs), `terminal_status_has_compact_heading_and_retains_nonzero_attention_counts`; [management.rs](crates/zg-cli/src/management.rs), existing scan-rule, retry/failure, duration and color-policy tests. |
| Search and path compatibility | [render.rs](crates/zg-cli/src/render.rs), preview/highlighting, native path bytes and `terminal_freshness_keeps_refresh_counts_and_redirected_contract`; [path_lookup.rs](crates/zg-cli/src/path_lookup.rs), `terminal_warning_changes_only_diagnostics_not_search_data`. |
| Errors | [messages.rs](crates/zg-cli/src/messages.rs), manifest recovery, debug diagnostics, control-character sanitization and unrelated storage failures. |
| Pickers and consent | [setup.rs](crates/zg-cli/src/setup.rs), [setup/credential.rs](crates/zg-cli/src/setup/credential.rs), [authorization.rs](crates/zg-cli/src/authorization.rs), and [install.rs](crates/zg-cli/src/install.rs): navigation, cancellation, consent, screen clipping and one-write redraw. |
| Direct/server and process behavior | [index_progress.rs](crates/zg/tests/index_progress.rs), [implicit_index.rs](crates/zg/tests/implicit_index.rs) and [server_lifecycle.rs](crates/zg/tests/server_lifecycle.rs): diagnostic streams, saved scope, readiness exit codes, closed pipes, setup cancellation and daemon/MCP behavior. |

## Review images

Review screenshots and their command/failure coverage are kept in the
[PR description](https://github.com/zvec-ai/zvec-grep/pull/337), outside the source
tree. Each card identifies its source: a real CLI execution, a production
formatter with sample data, or a cancelled interactive prompt with sample data.
Fixture output does not imply that the corresponding business operation ran.

Only OpenCode installation/removal outcomes are executed by the capture script;
other integration targets retain their implementation tests. MCP stdout and
redirected byte-stream contracts require tests, not screenshots. Still images
do not verify animation performance or readiness.

Generate fresh local captures from `rust/` (the PTY scripts require Unix):

```sh
cargo build --locked -p zg
cargo build --locked -p zg-cli --examples
TERM=xterm-256color COLUMNS=100 cargo run --locked -p zg-cli --example presentation > /tmp/zg-presentation.json
python3 scripts/capture-cli-catalog.py --include-daemon --output /tmp/zg-commands.json
python3 scripts/capture-cli-interactions.py --output /tmp/zg-prompts.json
node scripts/render-cli-style.mjs /tmp/zg-presentation.json /tmp/zg-review /tmp/zg-commands.json /tmp/zg-prompts.json
```

These commands generate current formatter and PTY captures; the historical model
and install picker screenshots remain in the PR description. Open the generated
HTML sheets at a viewport width of 1120 pixels and take full-page screenshots.
Keep generated images and capture data outside the repository.

Surrounding cards and evidence labels are review annotations, not terminal
borders. The converter preserves captured text and SGR colors, removing cursor
controls for static display. NUL separators are visualized as `␀` with a label;
the input JSON retains the original bytes. Home and temporary path prefixes may
be normalized for publication only when the card explicitly says so.
