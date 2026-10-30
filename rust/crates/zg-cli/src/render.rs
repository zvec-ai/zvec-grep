mod compact;
mod context;

#[cfg(test)]
use crate::highlight::QueryHighlighter;
#[cfg(test)]
use compact::write_item_preview;
#[cfg(test)]
use zg_engine::api::context::result::{
    CodeMetadata, ContextItemStatus, EntityMetadata, MarkdownMetadata,
};

use std::{
    io::{self, Write},
    path::Path,
};

use thiserror::Error;
use zg_engine::api::context::{
    ContextResult,
    result::{ContentRange, ContextItem},
};

use crate::{highlight::highlight_ranges, output_path, theme::StatusTheme};

/// Writes only native file paths, separated by newlines or NUL bytes.
///
/// # Errors
/// Returns the underlying writer error.
pub fn write_file_paths(
    mut writer: impl Write,
    paths: &[std::path::PathBuf],
    null: bool,
) -> io::Result<()> {
    for path in paths {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            writer.write_all(path.as_os_str().as_bytes())?;
        }
        #[cfg(not(unix))]
        writer.write_all(path.to_string_lossy().as_bytes())?;
        writer.write_all(if null { b"\0" } else { b"\n" })?;
    }
    writer.flush()
}

/// Writes a context reply in the stable CLI text layout.
///
/// # Errors
///
/// Returns the underlying writer error.
pub fn write_context_result(writer: impl Write, result: &ContextResult) -> io::Result<()> {
    write_rg_context_result(writer, result, false, |value| value)
}

fn write_rg_context_result(
    mut writer: impl Write,
    result: &ContextResult,
    color: bool,
    heading: impl Fn(String) -> String,
) -> io::Result<()> {
    if result.items.is_empty() {
        writeln!(writer, "{}", heading("No matches.".into()))?;
        return Ok(());
    }
    let mut previous_path: Option<&Path> = None;
    for item in &result.items {
        if previous_path != Some(item.relative_path.as_path()) {
            if previous_path.is_some() {
                writeln!(writer)?;
            }
            writeln!(
                writer,
                "{}",
                heading(output_path(&item.relative_path).display().to_string())
            )?;
            previous_path = Some(item.relative_path.as_path());
        }
        let content = item.content.trim_end();
        let content = if color {
            highlight_ranges(content, lexical_content_ranges(item))
        } else {
            content.into()
        };
        if let Some(first) = item.content_range.start_line() {
            writeln!(writer, "  {first}: {content}")?;
        } else {
            writeln!(writer, "  {content}")?;
        }
    }
    Ok(())
}

fn lexical_content_ranges(item: &ContextItem) -> impl Iterator<Item = (usize, usize)> + '_ {
    item.match_ranges.iter().filter_map(|range| {
        let ContentRange::Text {
            start_byte_offset: content_start,
            ..
        } = item.content_range
        else {
            return None;
        };
        let ContentRange::Text {
            start_byte_offset,
            end_byte_offset,
            ..
        } = range
        else {
            return None;
        };
        Some((
            start_byte_offset.checked_sub(content_start)?,
            end_byte_offset.checked_sub(content_start)?,
        ))
    })
}

/// Writes Node-compatible indexed-search status to the caller's diagnostic stream.
/// # Errors
/// Returns the underlying writer error.
pub fn write_context_status(mut writer: impl Write, result: &ContextResult) -> io::Result<()> {
    use zg_engine::api::context::result::ContextSource;
    if result.source != ContextSource::Index {
        return Ok(());
    }
    let has_drift =
        result.freshness.as_deref() == Some("possibly_stale") || result.has_stale_items();
    if !has_drift && result.indexing.is_none() {
        return Ok(());
    }
    writeln!(writer, "status: possibly_stale")?;
    let Some(state) = result
        .indexing
        .as_ref()
        .map(|indexing| indexing.state.as_str())
        .or(result.background_refresh.as_deref())
    else {
        return Ok(());
    };
    writeln!(writer, "results: served_from_current_index")?;
    write!(
        writer,
        "background_refresh: {}",
        if state == "off" { "idle" } else { state }
    )?;
    if let Some((completed, total)) = result
        .indexing
        .as_ref()
        .and_then(|indexing| indexing.completed.zip(indexing.total))
    {
        write!(writer, " ({completed}/{total})")?;
    }
    writeln!(writer)?;
    Ok(())
}

/// Writes concise terminal freshness diagnostics, preserving redirected output.
///
/// # Errors
/// Returns the underlying writer error.
pub fn write_context_status_with_options(
    mut writer: impl Write,
    result: &ContextResult,
    options: crate::OutputOptions,
    terminal: bool,
) -> io::Result<()> {
    use zg_engine::api::context::result::ContextSource;
    if !terminal {
        return write_context_status(writer, result);
    }
    if result.source != ContextSource::Index {
        return Ok(());
    }
    let has_drift =
        result.freshness.as_deref() == Some("possibly_stale") || result.has_stale_items();
    if !has_drift && result.indexing.is_none() {
        return Ok(());
    }
    let theme = StatusTheme::new(options.color, terminal);
    writeln!(writer, "{} Index may be out of date", theme.warning("!"))?;
    writeln!(writer, "  Results use the current index")?;
    if let Some(state) = result
        .indexing
        .as_ref()
        .map(|indexing| indexing.state.as_str())
        .or(result.background_refresh.as_deref())
    {
        let state = if state == "off" { "idle" } else { state };
        write!(writer, "  Refresh: {state}")?;
        if let Some((completed, total)) = result
            .indexing
            .as_ref()
            .and_then(|indexing| indexing.completed.zip(indexing.total))
        {
            write!(writer, " · {completed}/{total} files")?;
        }
        writeln!(writer)?;
    }
    Ok(())
}

/// Writes context using the selected preview presentation.
/// # Errors
/// Returns the underlying writer error.
pub fn write_context_with_options(
    mut writer: impl Write,
    result: &ContextResult,
    options: crate::OutputOptions,
    terminal: bool,
) -> io::Result<()> {
    use zg_engine::api::context::result::ContextSource;
    let theme = StatusTheme::new(options.color, terminal);
    if result.source == ContextSource::Rg {
        return write_rg_context_result(writer, result, theme.color, |value| value);
    }
    context::write(&mut writer, result, options, terminal)
}

#[derive(Debug, Error)]
#[error("Unknown help topic: {0}")]
pub struct HelpTopicError(String);

/// Returns the stable CLI help text for a command or topic.
///
/// # Errors
///
/// Returns an error when `topic` is not part of the public help surface.
pub fn help_text(topic: Option<&str>) -> Result<String, HelpTopicError> {
    let text = match topic {
        None => return Ok(main_help()),
        Some("search") => SEARCH_HELP,
        Some("index") => INDEX_HELP,
        Some("status") => STATUS_HELP,
        Some("config") => CONFIG_HELP,
        Some("auth") => AUTH_HELP,
        Some("server") => SERVER_HELP,
        Some("install") => INSTALL_HELP,
        Some("uninstall") => UNINSTALL_HELP,
        Some("help") => HELP_HELP,
        Some("models") => MODELS_HELP,
        Some("file-types") => FILE_TYPES_HELP,
        Some("environment" | "env") => ENVIRONMENT_HELP,
        Some("version") => VERSION_HELP,
        Some(topic) => return Err(HelpTopicError(topic.to_owned())),
    };
    Ok(text.to_owned())
}

/// Prints the stable CLI help text.
///
/// # Errors
///
/// Returns an error when `topic` is not part of the public help surface or
/// stdout fails for a reason other than a closed pipe.
pub fn print_help(topic: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    writeln!(crate::stdout(), "{}", help_text(topic)?)?;
    Ok(())
}

fn main_help() -> String {
    format!(
        "zvec-grep {}\n\n{MAIN_HELP_BODY}",
        env!("CARGO_PKG_VERSION")
    )
}

const MAIN_HELP_BODY: &str = r#"Usage:
  zg <query> [options]
  zg --<management-command> [options]

Search:
  <query>        Search indexed context (no command prefix)
  --rg           Run managed ripgrep

Management:
  --index        Build, rebuild, or drop the workspace index
  --status       Show workspace and index status
  --config       Configure provider credentials and embedding model defaults
  --auth         Manage Workspace Remote Embedding authorization
  --server       Start, stop, inspect, or run the shared MCP server
  --install      Install agent integrations
  --uninstall    Remove agent integrations
  --help [topic] Show help for search, a management command, or a topic
  --version      Print the installed version

Bare words such as query, index, and help are literal search text.
Positional words are joined into one query; repeat --hybrid for separate queries.
Terminal, redirected, and piped output all default to --preview short.
Use --preview detail for detailed metadata, file grouping, and full previews.

Examples:
  zg "where authentication is validated"
  zg --fts "AuthService"
  zg --rg -F "AuthService" src
  zg --rg --files src -g '*.ts'
  zg --index --embedding local/potion-code-16m-v2
  zg --status
  zg --auth status
  zg --server on
  zg --config model set local/potion-code-16m-v2 --device metal
  zg --install

Environment:
  ZVEC_GREP_HOME        Runtime and daemon state directory; Workspace indexes stay under <root>/.zvec-grep
  ZVEC_GREP_MODE        Default client mode: direct, server, or auto
  ZVEC_GREP_EMBEDDING   Default model for new indexes and auth grant
  ZVEC_GREP_API_KEY     Embedding provider credential fallback
  ZVEC_GREP_SERVER_URL  MCP server URL used by CLI clients

Run zg --help models or zg --help file-types for supported indexing capabilities.
Run zg --help environment for all variables, scopes, aliases, and precedence.
Run zg --help search or zg --help <topic> for specific help.
Use zg -h/--help for this page and zg -v/--version for the version."#;

const SEARCH_HELP: &str = r#"Usage:
  zg <query> [options]
  zg --hybrid <query> --fts <query> --vector <query> [--fuse]
  zg --rg [rg-options] <pattern> [path...]
  zg --rg --files [discovery-options] [path...]

Search routes:
  positional words                  File paths use current files and literal text; other words form one hybrid query
  --hybrid <query>                  Add an explicit hybrid query
  --fts <query>                     Add an exact/lexical query
  --vector <query>                  Add a semantic/vector query
  --fuse                            Fuse all query groups into one ranked list
  --rg                              Run exhaustive managed ripgrep

Positional arguments are joined with spaces, even if separately quoted:
  zg connection pool timeout                 One query: connection pool timeout
  zg "connection pool" "timeout"             The same query, with a warning
  zg "connection pool" "socket timeout"      One query, with a warning
  zg --hybrid "connection pool" --hybrid "socket timeout"
                                            Two explicit queries
The warning is written to stderr when multiple nonempty positional arguments
include one with internal whitespace. Shell quote characters are not available.
--hybrid, --fts, and --vector remain separate, repeatable queries.
Managed --rg keeps its pattern and path arguments separate.
An ordinary single path-shaped query such as auth.ts first matches current file
names without reading contents. If no name matches, current source is searched
literally. A complete miss returns No matches., including with a warm index.
Automatic path lookup prints a warning on stderr; use --hybrid to force
hybrid search. Explicit query routes, trace, refresh and symbol options bypass it.
With --files, every positional argument is an input file or directory; no pattern
is required. Use -g 'auth.ts' to select files by name.

Result options:
  --limit <n>                       Maximum results per group (default: 7)
  --preview <none|short|full|detail> Indexed output detail (default: short)
  --debug                           Print diagnostics to stderr
  --trace                           Include per-hit indexed search trace
  --refresh <background|wait|off>   Refresh policy (defaults: server=background, direct=off)
                                    In direct mode, background warns and falls back to off
  --mode <direct|server|auto>       Select indexed query transport (default: auto)

Preview modes: none keeps one source line; short uses compact bounded source;
full uses compact full source; detail adds metadata, outlines, and file grouping.
Full previews do not retrieve an entire function or file. TTY affects only color.

All preview modes preserve query groups, their ranks, and matched content.
A result appears in each matching group; only --fuse combines groups.

Embedding runtime:
  --api-key <key>                   Embedding provider API key
  --model-cache <path>              Local model cache directory
  --device <device>                 auto, cpu, metal, vulkan, cuda
  --allow-remote                    Allow Remote Embedding for this command only

Device and model-cache overrides apply to this query and its refresh only;
they do not change saved workspace settings.

File filters:
  -g, --glob <glob>                 Include paths; prefix with ! to exclude; repeatable
  --iglob <glob>                    Case-insensitive path glob; repeatable
  -t, --type <format>               Include an engine format, e.g. rust or markdown
  -T, --type-not <format>           Exclude an engine format; repeatable
  --category <category>             Include an engine category, e.g. code or document
  --category-not <category>         Exclude a file category; repeatable
  --modified-after <time>           Only files modified after a date or epoch milliseconds
  --modified-before <time>          Only files modified before a date or epoch milliseconds
  --symbol-type <type>              alias, class, enum, function, interface, module, value
  --prefer-symbol                   Prefer exact indexed symbols

Managed --rg uses the embedded Rust regex engine (--engine default).
Matching: -F, -i/-s/-S, -w/-x, -v, -U, --multiline-dotall, --crlf, -a.
Bounds: -A/-B/-C, -m/--max-count (per file), -j/--threads (0 = automatic).
Discovery: -u/-uu/-uuu, --no-ignore-*, --one-file-system, globs and types.
File listing: --files prints all selected paths without reading their contents;
-0/--null uses NUL separators. No index or daemon is needed in any client mode.
The list is sorted and has no headings or result limit. Recursive discovery
skips .git and .zvec-grep; explicit file inputs are still listed.
--limit is rejected with --files.
Exit status for --files: 0 = files found, 1 = no files, 2 = discovery error.
Patterns preserve whitespace; -e accepts empty patterns and leading "-".
Unicode BOM decoding is automatic. PCRE2, compressed search, explicit encoding,
and options that replace rg's output format are not supported.

Environment:
  ZVEC_GREP_MODE         Default client mode: direct, server, or auto
  ZVEC_GREP_API_KEY      Embedding provider credential fallback
  ZVEC_GREP_ENDPOINT     Remote Embedding endpoint fallback
  ZVEC_GREP_MODEL_CACHE  Local embedding model cache directory
  ZVEC_GREP_DEVICE       Local embedding device: auto, cpu, metal, vulkan, or cuda

See zg --help environment for precedence and Server-mode scope."#;

const INDEX_HELP: &str = r"Usage:
  zg --index [root] [options]
  zg --index [root] --rebuild [options]
  zg --index [root] --drop [--yes]

Index options:
  --rebuild                         Rebuild the existing index
  --drop                            Permanently remove the workspace index
  --yes                             Confirm --drop without prompting
  --debug                           Print skipped-file diagnostics to stderr
  --mode <direct|server|auto>       Select indexing transport

Embedding options:
  --embedding <model>               Single text embedding model for this workspace
  --api-key <key>                   Embedding provider API key
  --endpoint <url>                  Embedding provider endpoint
  --model-cache <path>              Local model cache directory
  --device <device>                 auto, cpu, metal, vulkan, cuda
  --embedding-concurrency <n>        Positive embedding concurrency (index only)
  --allow-remote                    Allow Remote Embedding for this command only

Explicit --device and --model-cache values are saved after successful indexing;
omitted options keep their saved values. Changes do not require --rebuild.
Precedence: CLI > Environment > Workspace > Global config > Backend default.

Index embedding concurrency:
  Applies only to index construction and updates, including automatic indexing
  and refresh; query-vector inference keeps its own defaults. The CLI option
  is accepted only with --index and is never saved in workspace settings.
  llama.cpp: context limit capped at 8, with serialized batches; defaults to 1.
  Transformers/ORT: in-flight batch limit capped at 8; defaults to 1. Native
    sessions honor that budget; CoreML uses one physical session.
  Potion/model2vec: concurrent batch tasks; defaults to 2, with an independent
    CPU worker-pool capacity. No cap of 8 is applied.
  Remote: concurrent requests; adaptive scheduling may reduce the limit after
    failures or rate limits. No cap of 8 is applied.
  Precedence: CLI/API override, ZVEC_GREP_EMBEDDING_CONCURRENCY, legacy
    ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM for llama.cpp indexing, then defaults.
  Explicit CLI overrides work without restarting the Server. Environment
    changes require restarting the Server so it inherits the new value.

Scan rules:
  -g, --glob <glob>                 Include paths; prefix with ! to exclude; repeatable
  --iglob <glob>                    Case-insensitive path glob; repeatable
  -t, --type <format>               Include an engine format, e.g. rust or markdown; repeatable
  -T, --type-not <format>           Exclude an engine format; repeatable
  --hidden[=true|false]              Include hidden paths except .git and .zvec-grep
  --no-ignore[=true|false]           Do not apply default or .gitignore rules
  --nested-git[=true|false]          Scan nested Git repositories and submodules
  --ignore-file <path>              Add an explicit ignore file; repeatable
  --max-depth <n>                   Maximum directory depth
  --max-filesize <size>             Maximum bytes or K/M/G/T size
  -L, --follow[=true|false]          Follow symbolic links safely
  --reset-paths                     Clear inherited scanning settings

Type filters use the same file-name rules as indexed queries and intersect
with path globs. Scan rules are saved; omitted options keep their saved values.

Interactive remote indexing asks to allow once, allow for this workspace, or
cancel. Non-interactive indexing requires --allow-remote or a workspace grant.

Workspaces are identified by their root paths; different roots may share a directory name.

This version indexes text only with one embedding model per workspace.
For a new index, zg --index uses --embedding, then ZVEC_GREP_EMBEDDING, then
the configured default, falling back to local/potion-code-16m-v2.
Interactive searches prompt to select an embedding model before creating a
missing index. Local and remote models have separate tabs; downloaded local
models and configured remote models appear first. Use arrows or Tab to switch,
Enter to confirm, and Esc to exit. Remote models require data authorization.
CLI searches wait for the complete index, including in --mode server.
Non-interactive searches never create an index; run zg --index first.
Model changes require --rebuild. Failed files are recorded; successful files
remain searchable after a rebuild. Compatible indexes reuse their stored model.
Rebuilding an incompatible index uses --embedding or the configured default;
provide desired scan rules again.

Environment:
  ZVEC_GREP_MODE         Default client mode: direct, server, or auto
  ZVEC_GREP_EMBEDDING    Default model for new indexes and auth grant
  ZVEC_GREP_API_KEY      Embedding provider credential fallback
  ZVEC_GREP_ENDPOINT     Remote Embedding endpoint fallback
  ZVEC_GREP_MODEL_CACHE  Local embedding model cache directory
  ZVEC_GREP_DEVICE       Local embedding device: auto, cpu, metal, vulkan, or cuda

Index-only embedding concurrency:
  ZVEC_GREP_EMBEDDING_CONCURRENCY  Positive index/update concurrency override
  Applies to automatic indexing and refresh, never query-vector inference.
  Explicit CLI/API index settings take precedence. Invalid values warn and use
  automatic selection. Server mode reads the Server process environment;
  restart the Server after changing it. Overrides are not persisted.

See zg --help environment for precedence and Server-mode scope.";

const STATUS_HELP: &str = r"Usage:
  zg --status [root] [--mode <direct|server|auto>] [--check-ready]

Shows the nearest workspace root, index policy, index state, embedding model,
stored paths, refresh status, and suggested next action.

--check-ready preserves the normal output and exits non-zero unless the
Workspace index is ready.";

const CONFIG_HELP: &str = r"Usage:
  zg --config provider set <provider> --api-key <key>
  zg --config model set <model> [--endpoint <url> | --device <device>] [--default]

Provider options:
  --api-key <key>                   Default API key for the provider

Model options:
  --endpoint <url>                  Endpoint for a remote embedding model
  --device <device>                 Local device: auto, cpu, metal, vulkan, cuda
  --default                         Use this model for new indexes

Remote models support --endpoint; local models support --device. At least one
model option is required. --default may be used alone or with a runtime option.
Each workspace uses one model for text content. Existing indexes continue to
use their stored model.

Global configuration is stored in ~/.zvec-grep/config.json.";

const AUTH_HELP: &str = r"Usage:
  zg --auth grant [root] --capability embedding --scope workspace [--embedding <model>]
  zg --auth status [root]
  zg --auth revoke [root]

Manage the Remote Embedding grant stored in the Workspace under
.zvec-grep/authorization.json. Workspace grants are shared by zg CLI and zg MCP.

--embedding selects the Remote Embedding model to authorize; it does not run
embedding. If omitted, auth grant uses the existing Workspace index model, then
ZVEC_GREP_EMBEDDING.

--endpoint selects the exact provider URL to authorize; otherwise the stored
index endpoint, ZVEC_GREP_ENDPOINT, or provider default is used. Grants bind the
canonical source root, username, model, and endpoint. Moving the source or changing
the username requires authorization again. A matching path and username can reuse
the grant on another machine. Granting sends no remote data.

Output:
  --color <auto|always|never>       Color output (default: auto)
  --no-color                        Disable color output

Scopes used during operations:
  once                              Current CLI command or Agent tool call only
  workspace                         Persisted in this Workspace

Use --allow-remote on zg <query> or zg --index to authorize Remote Embedding for
that command only. This authorization is not persisted. API credentials
configure a provider but do not grant permission.

Environment used by auth grant:
  ZVEC_GREP_EMBEDDING               Default model for new indexes and auth grant
  ZVEC_GREP_API_KEY                 Embedding provider credential fallback
  ZVEC_GREP_ENDPOINT                Remote Embedding endpoint fallback";

const SERVER_HELP: &str = r"Usage:
  zg --server --stdio [--token-file <path>] [--mcp-toolset <agent|full>]
  zg --server on [--listen 127.0.0.1:7999] [--token-file <path>] [--mcp-toolset <agent|full>]
  zg --server off [--token-file <path>]
  zg --server status [--check-ready]
  zg --server run [--listen 127.0.0.1:7999] [--token-file <path>] [--mcp-toolset <agent|full>]

--stdio is the MCP client bootstrap transport. It safely starts or reuses the
shared daemon, proxies MCP over stdin/stdout, and leaves the daemon running
when the client disconnects.

The server listens on loopback. Authentication is disabled by default; pass a
token file or set ZVEC_GREP_SERVER_TOKEN to require Bearer authentication.
The public MCP endpoint defaults to the agent toolset (indexed search only).
Use --mcp-toolset full, or ZVEC_GREP_MCP_TOOLSET=full, to expose managed rg and
the four index and status tools. CLI managed rg, index, and status commands
continue to use the daemon's internal administration endpoint.
--check-ready exits non-zero unless the server is ready.

Environment:
  ZVEC_GREP_HOME               Runtime and daemon state directory; Workspace indexes stay under <root>/.zvec-grep
  ZVEC_GREP_SERVER_URL         MCP server URL used by CLI clients
  ZVEC_GREP_SERVER_TOKEN       Server/client Bearer token
  ZVEC_GREP_SERVER_TOKEN_FILE  File containing the Server/client Bearer token
  ZVEC_GREP_MCP_TOOLSET        Server MCP surface: agent or full

See zg --help environment for daemon startup scope.";

const INSTALL_HELP: &str = r"Usage:
  zg --install [--target codex|claude|qwen|qoder|opencode|cursor|copilot|vscode|grok|pi|all|auto] [--mcp-transport stdio|http] [--mcp-toolset agent|full] [--yes] [--force]
  zg --install --upgrade-installed [--dry-run]

Options:
  --target <agent>                  codex, claude, qwen, qoder, opencode, cursor, copilot, vscode, grok, pi, auto, or all; repeatable
  --mcp-transport <stdio|http>      MCP connection mode (default: stdio)
  --mcp-toolset <agent|full>        Daemon MCP toolset (default: agent)
  --mcp-tool-timeout <seconds>      MCP tool timeout where supported (default: 600)
  --mcp-token-env <name>            HTTP mode Bearer token environment variable
  --yes                             Install detected agents without prompting
  --force                           Replace conflicting unmanaged configuration
  --upgrade-installed              Migrate existing integrations without installing new ones
  --dry-run                        Preview migration without writing files

Upgrade scans existing entries in known user configuration files.
It preserves transport, custom settings, permissions, guidance and comments;
HTTP entries are unchanged. Unsupported launchers require manual migration.
Changed files receive a unique .zg-upgrade-*.bak backup beside the original.
No daemon is started. Reconnect agents afterwards; HTTP users run zg --server on.
Upgrade options cannot be combined with target or installation settings.

The qoder target configures Qoder CLI and Qoder IDE together.
The copilot target configures GitHub Copilot CLI and Agent Host. The vscode
target configures detected VS Code profiles and shared Copilot instructions.
The grok target configures Grok Build's user-level config.toml, global
rules file, and tool pre-approval.
The pi target configures built-in MCP in ~/.pi/agent/mcp.json and the active
global context file. PI_CODING_AGENT_DIR overrides this directory; use native
filesystem paths (file:// and Windows shell drive paths are unsupported).
New servers use direct exposure and a timeout in seconds. Reinstall preserves
user exposure/enabled settings and existing HTTP authentication when no token
option is supplied. Use /reload or a new session; replacement MCP extensions
must be disabled separately. Remote Embedding may require the user to run
zg --auth grant in their terminal before retrying.

Interactive setup detects supported agents, configures stdio by default, and
starts the shared daemon. In stdio mode an agent reconnect also starts the
daemon automatically after a reboot. HTTP users manage later daemon restarts.
Codex, Claude Code, Qwen Code, Qoder CLI, OpenCode, Copilot, VS Code, and
Grok Build and Pi receive managed guidance. Qoder IDE has no supported global Rules
file, so only its MCP configuration is managed.
Codex and Claude Code receive local tool pre-approval. Grok Build receives
pre-approval through its permission rules unless the configuration already
defines a [permission] table. Remote Embedding
authorization remains separate and is requested by zvec-grep on first remote
use. Restart the agent or open a new session after installation. This does not
install the npm package.";

const UNINSTALL_HELP: &str = r"Usage:
  zg --uninstall [--target codex|claude|qwen|qoder|opencode|cursor|copilot|vscode|grok|pi|all|auto] [--yes]

Removes zvec-grep-managed MCP configuration, agent-specific approval, and
guidance. The qoder target removes the managed Qoder CLI and IDE integration
together. The pi target removes its managed server and guidance blocks from
the effective agent directory; unrelated configuration and context are kept.";

const HELP_HELP: &str = r"Usage:
  zg --help [topic]
  zg -h [topic]

Topics:
  search                             Indexed search and managed ripgrep
  index, status, config, auth         Workspace management
  server, install, uninstall          Server and agent integrations
  help, version                      Help and version output
  models                             Supported embedding models
  file-types                         Supported file types and structural parsing
  environment, env                   Environment variables and precedence";

const VERSION_HELP: &str = r"Usage:
  zg -v
  zg --version";

const MODELS_HELP: &str = r"Usage:
  zg --help models

Supported text embedding models (one per workspace):
  MODEL                               RUNTIME  INPUT       DIMS  TOKENS  BACKEND
  ----------------------------------  -------  ----------  ----  ------  ---------------
  local/all-minilm-l6-v2              local    text         384     256  transformers
  local/bge-small-en-v1.5             local    text         384     512  transformers
  local/embeddinggemma-300m           local    text         768    2048  llama-cpp
  local/gte-modernbert-base           local    text         768    8192  transformers
  local/jina-embeddings-v2-base-code  local    text         768    8192  transformers
  local/multilingual-e5-small         local    text         384     512  transformers
  local/nomic-embed-text-v1.5         local    text         768    8192  transformers
  local/potion-code-16m-v2            local    text         256    1024  model2vec
  local/potion-multilingual-128m      local    text         256    1024  model2vec
  local/potion-retrieval-32m          local    text         512    1024  model2vec
  local/qwen3-embedding-0.6b          local    text        1024    8192  llama-cpp
  qwen/qwen3-vl-embedding             remote   text        2560   32000  qwen
  qwen/qwen3.7-text-embedding         remote   text        1024  128000  qwen
  qwen/text-embedding-v4              remote   text        1024    8192  qwen

Local models are downloaded to the model cache on first use. Remote models
require provider credentials plus --allow-remote or a Workspace authorization.
This version uses text input only, including for models with image capabilities.

Existing indexes keep their stored model. See zg --help environment for
new-index model selection and runtime precedence.";

const FILE_TYPES_HELP: &str = r"Usage:
  zg --help file-types

Structured code (symbols and scopes):
  TYPE        FILES
  ----------  -------------------------
  c           .c
  cpp         .cc, .cpp, .cxx, .h, .hpp
  go          .go
  java        .java
  javascript  .js, .mjs, .cjs
  jsx         .jsx
  python      .py
  rust        .rs
  tsx         .tsx
  typescript  .ts

Component code (JavaScript and TypeScript script blocks):
  TYPE    FILES
  ------  -------
  svelte  .svelte
  vue     .vue

Other code (plain-text chunks):
  TYPE        FILES
  ----------  ----------------
  bash        .sh, .bash, .zsh
  csharp      .cs
  css         .css
  dockerfile  Dockerfile
  kotlin      .kt, .kts
  less        .less
  makefile    Makefile
  php         .php
  ruby        .rb
  scala       .scala
  scss        .scss
  sql         .sql
  swift       .swift

Documents and data:
  TYPE      FILES
  --------  -------------
  csv       .csv
  html      .html, .htm
  json      .json, .jsonc
  markdown  .md, .mdx
  rst       .rst
  text      .txt
  toml      .toml
  xml       .xml
  yaml      .yaml, .yml
  Markdown preserves heading structure; other formats use text chunks.

Images and other non-text content are not indexed in this version.

Other text:
  Unknown non-binary extensions and extensionless files use text chunks.

Skipped binary types:
  GROUP      EXTENSIONS
  ---------  ----------------------------------------------------------
  Archives   .zip, .tar, .gz, .bz2, .xz, .7z, .rar
  Compiled   .exe, .dll, .dylib, .so, .a, .o, .obj, .wasm, .class, .jar
  Documents  .pdf, .doc, .docx, .ppt, .pptx, .xls, .xlsx
  Media      .mp3, .mp4, .mov, .avi, .mkv
  Databases  .db, .sqlite
  Files detected as binary by content are also skipped.

Indexing rules:
  Default size limits:
  KIND   MAX SIZE
  -----  --------
  Code   1 MiB
  Text   256 MiB
  Data   16 MiB
  Image  10 MiB
  Empty files are skipped. Use --max-filesize to override the size limit.
  Common dependencies, build output, generated files, and lock files are
  ignored by default. .git and .zvec-grep are always skipped.";

const ENVIRONMENT_HELP: &str = r"Usage:
  zg --help environment
  zg --help env

Client and Server:
  ZVEC_GREP_MODE               Default client mode: direct, server, or auto
  ZVEC_GREP_SERVER_URL         MCP server URL used by CLI clients
  ZVEC_GREP_SERVER_TOKEN       Server/client Bearer token
  ZVEC_GREP_SERVER_TOKEN_FILE  File containing the Server/client Bearer token
  ZVEC_GREP_MCP_TOOLSET        Server MCP surface: agent or full

Embedding:
  ZVEC_GREP_EMBEDDING    Default model for new indexes and auth grant
  ZVEC_GREP_API_KEY      Embedding provider credential fallback
  ZVEC_GREP_ENDPOINT     Remote Embedding endpoint fallback
  ZVEC_GREP_MODEL_CACHE  Local embedding model cache directory
  ZVEC_GREP_DEVICE       Local embedding device: auto, cpu, metal, vulkan, or cuda

Index-only embedding concurrency:
  ZVEC_GREP_EMBEDDING_CONCURRENCY  Positive index/update concurrency override
  Applies to automatic indexing and refresh, never query-vector inference.
  Precedence: explicit CLI/API index setting, this variable, legacy llama.cpp
  index setting, then backend defaults. Invalid values warn and use defaults.
  Server mode reads the Server process environment; restart it after changes.
  These overrides are not persisted in workspace settings.

Qwen credential aliases:
  DASHSCOPE_API_KEY  Qwen credential fallback after ZVEC_GREP_API_KEY
  QWEN_API_KEY       Qwen credential fallback after DASHSCOPE_API_KEY

State:
  ZVEC_GREP_HOME                    Runtime and daemon state directory; Workspace indexes stay under <root>/.zvec-grep

Advanced:
  ZVEC_GREP_METAL_KEEP_RESIDENCY       Set to 1 to keep llama.cpp Metal residency enabled (advanced)
  ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM  Legacy index-only llama.cpp fallback after the index concurrency variable
  NO_COLOR                             Disable terminal colors

Agent integration paths:
  CODEX_HOME            Codex configuration directory used by zg --install
  CLAUDE_CONFIG_DIR     Claude configuration directory used by zg --install
  GROK_HOME             Grok Build configuration directory used by zg --install
  PI_CODING_AGENT_DIR   Pi agent directory used by zg --install/--uninstall
  QWEN_HOME             Qwen Code configuration directory used by zg --install
  QODER_CONFIG_DIR      Qoder CLI configuration directory used by zg --install
  QODER_IDE_MCP_PATH    Full Qoder IDE SharedClientCache/mcp.json path used by zg --install
  QODER_IDE_EXECUTABLE  Qoder IDE executable used by automatic install-target detection
  OPENCODE_CONFIG       OpenCode configuration file used by zg --install
  CURSOR_CONFIG_DIR     Cursor configuration directory used by zg --install
  COPILOT_HOME          GitHub Copilot configuration and instructions directory
  VSCODE_USER_DIR       Complete VS Code User profile directory override
  VSCODE_PORTABLE       VS Code portable installation directory
  VSCODE_APPDATA        Base directory for VS Code release channels

Precedence:
  Remote credentials               CLI > Workspace snapshot > Global config > Environment
  Device/model cache               CLI > Environment > Workspace > Global config > Backend default
  New-index model                  --embedding > ZVEC_GREP_EMBEDDING > Global config
  Client mode                      --mode > ZVEC_GREP_MODE > Global config
  Qwen environment credential      ZVEC_GREP_API_KEY > DASHSCOPE_API_KEY > QWEN_API_KEY

Server scope:
  zg --index forwards its ZVEC_GREP_EMBEDDING default to Server and auto modes.
  Direct MCP calls use the embedding environment inherited by the daemon.
  Restart the daemon after changing its embedding runtime environment.

Explicit CLI options take priority. Help output never prints or stores
environment values.";

#[cfg(test)]
mod output_tests {
    use super::*;
    use crate::{OutputOptions, PreviewMode};
    use zg_engine::api::context::result::{
        ContextContentRole, ContextCoverage, ContextDiagnostics, ContextItemKind, ContextSource,
        MatchedBy,
    };

    #[cfg(unix)]
    #[test]
    fn file_output_preserves_non_utf8_path_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"file-\xff.ts".to_vec()));
        let mut output = Vec::new();
        write_file_paths(&mut output, &[path], true).expect("file output");
        assert_eq!(output, b"file-\xff.ts\0");
    }

    fn indexed_item() -> ContextItem {
        ContextItem {
            kind: ContextItemKind::IndexedEntity,
            rank: 1,
            absolute_path: std::env::temp_dir().join("sample.rs"),
            relative_path: "sample.rs".into(),
            range: ContentRange::Text {
                start_line: 1,
                end_line: 20,
                start_byte_offset: 0,
                end_byte_offset: 130,
                start_byte_column: 0,
                end_byte_column: 6,
            },
            content_range: ContentRange::Text {
                start_line: 1,
                end_line: 20,
                start_byte_offset: 0,
                end_byte_offset: 130,
                start_byte_column: 0,
                end_byte_column: 6,
            },
            excerpt_range: None,
            match_ranges: Vec::new(),
            outline: None,
            content: (1..=20)
                .map(|n| format!("line{n}"))
                .collect::<Vec<_>>()
                .join("\n"),
            content_role: Some(ContextContentRole::Source),
            status: ContextItemStatus::Fresh,
            score: Some(0.75),
            matched_by: MatchedBy::Fts,
            metadata: None,
            entity_id: None,
            container: None,
            trace: None,
            query_groups: vec![],
            selection_reason: None,
            coverage_group: None,
        }
    }

    fn indexed_result() -> ContextResult {
        ContextResult {
            query: "needle".into(),
            freshness: None,
            background_refresh: None,
            indexing: None,
            root: std::env::temp_dir(),
            source: ContextSource::Index,
            coverage: ContextCoverage::RankedSample,
            workspace_index: None,
            group_results: vec![],
            diagnostics: ContextDiagnostics::default(),
            items: vec![indexed_item()],
        }
    }

    #[test]
    fn context_output_highlights_query_terms_in_source() {
        let mut result = indexed_result();
        result.query = "find ImportantSymbol".into();
        result.items[0].relative_path = "ImportantSymbol.rs".into();
        result.items[0].content = "İ 中文 ImportantSymbol here".into();
        for source in [ContextSource::Index, ContextSource::Rg] {
            result.source = source;
            result.items[0].match_ranges = if source == ContextSource::Rg {
                let start = "İ 中文 ".len();
                let end = start + "ImportantSymbol".len();
                vec![ContentRange::Text {
                    start_line: 1,
                    end_line: 1,
                    start_byte_offset: start,
                    end_byte_offset: end,
                    start_byte_column: start,
                    end_byte_column: end,
                }]
            } else {
                Vec::new()
            };
            let original = result.clone();
            for preview in [
                PreviewMode::None,
                PreviewMode::Short,
                PreviewMode::Full,
                PreviewMode::Detail,
            ] {
                for (color, terminal, styled) in [
                    (crate::ColorMode::Always, false, true),
                    (crate::ColorMode::Never, true, false),
                    (crate::ColorMode::Auto, false, false),
                    (
                        crate::ColorMode::Auto,
                        true,
                        std::env::var_os("NO_COLOR").is_none(),
                    ),
                ] {
                    let mut output = Vec::new();
                    write_context_with_options(
                        &mut output,
                        &result,
                        OutputOptions {
                            preview,
                            color,
                            ..OutputOptions::default()
                        },
                        terminal,
                    )
                    .expect("context output");
                    let output = String::from_utf8(output).expect("UTF-8");
                    let expected = if styled {
                        "İ 中文 \x1b[1;33mImportantSymbol\x1b[0m here"
                    } else {
                        "İ 中文 ImportantSymbol here"
                    };
                    assert!(
                        output.contains(expected),
                        "{source:?} {color:?}: {output:?}"
                    );
                    assert_eq!(output.contains('\x1b'), styled);
                    assert!(output.contains("ImportantSymbol.rs"));
                    if preview == PreviewMode::Detail && source == ContextSource::Index && styled {
                        assert!(output.contains(
                            "\x1b[2mQuery\x1b[0m: \x1b[1;33mfind ImportantSymbol\x1b[0m"
                        ));
                    }
                    assert_eq!(result, original);
                }
            }
        }
    }

    #[test]
    fn context_highlighting_follows_visible_preview_without_coloring_metadata() {
        let mut result = indexed_result();
        result.query = "needle".into();
        result.items[0].content = format!("{}needle\nneedle", "x".repeat(160));
        result.items[0].metadata = Some(EntityMetadata::Markdown(MarkdownMetadata {
            heading: Some("needle".into()),
            level: Some(1),
            scope: None,
        }));
        for (preview, expected_matches) in [
            (PreviewMode::None, 0),
            (PreviewMode::Short, 1),
            (PreviewMode::Full, 2),
        ] {
            let mut output = Vec::new();
            write_context_with_options(
                &mut output,
                &result,
                OutputOptions {
                    preview,
                    color: crate::ColorMode::Always,
                    ..OutputOptions::default()
                },
                false,
            )
            .expect("context preview output");
            let output = String::from_utf8(output).expect("UTF-8");
            assert_eq!(
                output.matches("\x1b[1;33mneedle\x1b[0m").count(),
                expected_matches
            );
            assert!(output.contains("heading: needle\n"));
        }
    }

    #[test]
    fn context_formats_paths_without_rewriting_source_content() {
        let path = r"\\?\D:\workspace\sample.rs";
        let expected = if cfg!(windows) {
            r"D:\workspace\sample.rs"
        } else {
            path
        };
        let mut result = indexed_result();
        result.items[0].relative_path = path.into();
        result.items[0].content = path.into();
        let original = result.clone();
        for source in [ContextSource::Index, ContextSource::Rg] {
            result.source = source;
            let mut bytes = Vec::new();
            write_context_with_options(
                &mut bytes,
                &result,
                OutputOptions {
                    preview: PreviewMode::Full,
                    ..OutputOptions::default()
                },
                false,
            )
            .expect("context output");
            let text = String::from_utf8(bytes).expect("UTF-8");
            assert!(text.contains(expected));
            assert!(text.contains(&format!("1: {path}")));
        }
        result.source = original.source;
        assert_eq!(result, original);
    }

    #[test]
    fn preview_numbers_follow_content_range_instead_of_match_or_entity_range() {
        for trailing_newline in [false, true] {
            for whole_entity in [false, true] {
                let mut item = indexed_item();
                let source = format!(
                    "# Heading\nprefix needle{}",
                    if trailing_newline { "\n" } else { "" }
                );
                item.range = ContentRange::Text {
                    start_line: 1,
                    end_line: if trailing_newline { 3 } else { 2 },
                    start_byte_offset: 0,
                    end_byte_offset: source.len(),
                    start_byte_column: 0,
                    end_byte_column: if trailing_newline { 0 } else { 13 },
                };
                let mut excerpt = item.range.clone();
                if let ContentRange::Text {
                    start_line,
                    start_byte_offset,
                    start_byte_column,
                    ..
                } = &mut excerpt
                {
                    *start_line = 2;
                    *start_byte_offset = 17;
                    *start_byte_column = 7;
                }
                item.excerpt_range = Some(excerpt.clone());
                item.content_range = if whole_entity {
                    item.range.clone()
                } else {
                    excerpt
                };
                item.content = if whole_entity {
                    source
                } else {
                    source[17..].to_owned()
                };
                for preview in [PreviewMode::Short, PreviewMode::Full] {
                    let mut output = Vec::new();
                    write_item_preview(
                        &mut output,
                        &item,
                        OutputOptions {
                            preview,
                            ..OutputOptions::default()
                        },
                        &QueryHighlighter::default(),
                    )
                    .expect("source preview");
                    let expected = if whole_entity {
                        "  1: # Heading\n  2: prefix needle\n"
                    } else {
                        "  2: needle\n"
                    };
                    assert_eq!(String::from_utf8(output).expect("UTF-8"), expected);
                }
            }
        }
    }

    #[test]
    fn context_status_is_independent_of_results_and_reports_index_progress() {
        use zg_engine::api::context::result::ContextIndexingStatus;
        let mut result = indexed_result();
        result.freshness = Some("fresh".into());
        result.background_refresh = Some("idle".into());
        result.items.clear();
        let render = |result: &ContextResult| {
            let mut output = Vec::new();
            super::write_context_status(&mut output, result).expect("status");
            String::from_utf8(output).expect("UTF-8")
        };
        assert_eq!(render(&result), "");
        result.indexing = Some(ContextIndexingStatus {
            state: "idle".into(),
            completed: Some(2),
            total: Some(3),
        });
        assert_eq!(
            render(&result),
            "status: possibly_stale\nresults: served_from_current_index\nbackground_refresh: idle (2/3)\n"
        );
        for state in ["queued", "running", "failed", "cancelled", "succeeded"] {
            result.indexing = Some(ContextIndexingStatus {
                state: state.into(),
                completed: None,
                total: Some(3),
            });
            assert_eq!(
                render(&result),
                format!(
                    "status: possibly_stale\nresults: served_from_current_index\nbackground_refresh: {state}\n"
                )
            );
        }
        result.indexing = None;
        result.freshness = Some("served_from_current_index".into());
        assert_eq!(render(&result), "");
        result.freshness = Some("possibly_stale".into());
        result.background_refresh = None;
        assert_eq!(render(&result), "status: possibly_stale\n");
        result.source = ContextSource::Rg;
        assert_eq!(render(&result), "");
    }

    #[test]
    fn terminal_freshness_keeps_refresh_counts_and_redirected_contract() {
        use zg_engine::api::context::result::ContextIndexingStatus;
        let mut result = indexed_result();
        result.freshness = Some("possibly_stale".into());
        result.indexing = Some(ContextIndexingStatus {
            state: "running".into(),
            completed: Some(2),
            total: Some(3),
        });
        let options = OutputOptions {
            color: crate::ColorMode::Never,
            ..OutputOptions::default()
        };
        let mut terminal = Vec::new();
        super::write_context_status_with_options(&mut terminal, &result, options, true)
            .expect("terminal status");
        assert_eq!(
            String::from_utf8(terminal).expect("UTF-8"),
            "! Index may be out of date\n  Results use the current index\n  Refresh: running · 2/3 files\n"
        );
        let mut redirected = Vec::new();
        let mut original = Vec::new();
        super::write_context_status_with_options(&mut redirected, &result, options, false)
            .expect("redirected status");
        super::write_context_status(&mut original, &result).expect("original status");
        assert_eq!(redirected, original);
    }

    #[test]
    fn preview_modes_and_trace_preserve_indexed_result_information() {
        let mut result = indexed_result();
        let render = |preview, trace| {
            let mut buffer = Vec::new();
            write_context_with_options(
                &mut buffer,
                &result,
                OutputOptions {
                    trace,
                    preview,
                    ..OutputOptions::default()
                },
                false,
            )
            .expect("render");
            String::from_utf8(buffer).expect("UTF-8")
        };
        let minimal = render(PreviewMode::None, false);
        assert!(minimal.contains("#1 matchedBy=fts sample.rs:1-20"));
        assert!(minimal.contains("1\tline1"));
        assert!(!minimal.contains("2\tline2"));
        let short = render(PreviewMode::Short, false);
        assert!(short.contains("10: line10"));
        assert!(!short.contains("11: line11"));
        let full = render(PreviewMode::Full, true);
        assert!(full.contains("20: line20"));
        assert!(full.contains("score: 0.7500"));

        result.items[0].range = ContentRange::Text {
            start_line: 1,
            end_line: 21,
            start_byte_offset: 0,
            end_byte_offset: 131,
            start_byte_column: 0,
            end_byte_column: 0,
        };
        result.items[0].content.push('\n');
        result.items[0].content_range = result.items[0].range.clone();
        let mut buffer = Vec::new();
        write_context_with_options(&mut buffer, &result, OutputOptions::default(), false)
            .expect("render trailing newline");
        let rendered = String::from_utf8(buffer).expect("UTF-8");
        assert!(rendered.contains("sample.rs:1-20"));
        assert!(!rendered.contains("sample.rs:1-21"));
    }

    #[test]
    fn symbol_preview_preserves_names_with_optional_classification() {
        let mut item = indexed_item();
        for (symbol_type, expected) in [
            (None, "symbol: User scope: app"),
            (
                Some(zg_engine::api::context::options::SymbolType::Enum),
                "symbol: enum User scope: app",
            ),
        ] {
            item.metadata = Some(EntityMetadata::Code(CodeMetadata {
                symbol_type,
                symbol_name: Some("User".into()),
                scope: Some("app".into()),
                signature: None,
                visibility: None,
                language: None,
                documentation: None,
            }));
            let mut buffer = Vec::new();
            write_item_preview(
                &mut buffer,
                &item,
                OutputOptions::default(),
                &QueryHighlighter::default(),
            )
            .expect("render symbol metadata");
            let rendered = String::from_utf8(buffer).expect("UTF-8");
            assert_eq!(rendered.lines().next(), Some(expected));
        }
    }
}
