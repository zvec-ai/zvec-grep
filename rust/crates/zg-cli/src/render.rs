use std::{
    io::{self, Write},
    path::Path,
};

use thiserror::Error;
use zg_engine::api::{
    context::{
        ContextResult,
        result::{
            CodeMetadata, ContentRange, ContextItem, ContextItemStatus, EntityMetadata,
            MarkdownMetadata,
        },
    },
    index::IndexResult,
    info::{InfoResult, result::IndexCompatibility},
};

/// Writes a context reply in the stable CLI text layout.
///
/// # Errors
///
/// Returns the underlying writer error.
pub fn write_context_result(mut writer: impl Write, result: &ContextResult) -> io::Result<()> {
    if result.items.is_empty() {
        writeln!(writer, "No matches.")?;
        return Ok(());
    }
    let mut previous_path: Option<&Path> = None;
    for item in &result.items {
        if previous_path != Some(item.relative_path.as_path()) {
            if previous_path.is_some() {
                writeln!(writer)?;
            }
            writeln!(writer, "{}", item.relative_path.display())?;
            previous_path = Some(item.relative_path.as_path());
        }
        if let Some(first) = item.content_range.start_line() {
            writeln!(writer, "  {first}: {}", item.content.trim_end())?;
        } else {
            writeln!(writer, "  {}", item.content.trim_end())?;
        }
    }
    Ok(())
}

/// Writes context using the selected terminal presentation.
/// # Errors
/// Returns the underlying writer error.
pub fn write_context_with_options(
    mut writer: impl Write,
    result: &ContextResult,
    options: crate::OutputOptions,
    terminal: bool,
) -> io::Result<()> {
    use crate::ColorMode;
    use zg_engine::api::context::result::ContextSource;
    let color = options.color == ColorMode::Always
        || (options.color == ColorMode::Auto && terminal && std::env::var_os("NO_COLOR").is_none());
    let heading = |value: String| {
        if color {
            format!("\x1b[1;36m{value}\x1b[0m")
        } else {
            value
        }
    };
    if options.human {
        writeln!(writer, "{}: {:?}", heading("Context".into()), result.source)?;
        writeln!(writer, "Query: {}", result.query)?;
        writeln!(writer, "Hits: {}", result.items.len())?;
    }
    if result.source == ContextSource::Rg {
        if color {
            let mut buffer = Vec::new();
            write_context_result(&mut buffer, result)?;
            for line in String::from_utf8_lossy(&buffer).lines() {
                writeln!(
                    writer,
                    "{}",
                    if line.starts_with("  ") {
                        line.to_owned()
                    } else {
                        heading(line.to_owned())
                    }
                )?;
            }
            return Ok(());
        }
        return write_context_result(writer, result);
    }
    if result.items.is_empty() {
        return writeln!(writer, "No matches.");
    }
    for (index, item) in result.items.iter().enumerate() {
        if index > 0 {
            writeln!(writer)?;
        }
        let range = range_label(&item.range);
        let matched_by = serde_json::to_value(item.matched_by).map_err(io::Error::other)?;
        let label = if options.human {
            format!("{}. {}:{}", item.rank, item.relative_path.display(), range)
        } else {
            format!(
                "#{} matchedBy={} {}:{}",
                item.rank,
                matched_by.as_str().unwrap_or_default(),
                item.relative_path.display(),
                range
            )
        };
        writeln!(writer, "{}", heading(label))?;
        write_item_preview(&mut writer, item, options)?;
        if options.trace {
            if let Some(score) = item.score {
                writeln!(writer, "score: {score:.4}")?;
            }
            if let Some(trace) = &item.trace {
                writeln!(
                    writer,
                    "trace: {}",
                    serde_json::to_string(trace).map_err(io::Error::other)?
                )?;
            }
        }
    }
    Ok(())
}

fn write_item_preview(
    mut writer: impl Write,
    item: &ContextItem,
    options: crate::OutputOptions,
) -> io::Result<()> {
    use crate::PreviewMode;
    if item.status == ContextItemStatus::PossiblyStale {
        writeln!(writer, "status: possibly_stale")?;
    }
    if let Some(metadata) = &item.metadata {
        match metadata {
            EntityMetadata::Code(CodeMetadata {
                symbol_type,
                symbol_name: Some(name),
                scope,
                ..
            }) => {
                write!(writer, "symbol: ")?;
                if let Some(symbol_type) = symbol_type {
                    let kind = serde_json::to_value(symbol_type).map_err(io::Error::other)?;
                    write!(writer, "{} ", kind.as_str().unwrap_or_default())?;
                }
                write!(writer, "{name}")?;
                if let Some(scope) = scope {
                    write!(writer, " scope: {scope}")?;
                }
                writeln!(writer)?;
            }
            EntityMetadata::Markdown(MarkdownMetadata {
                heading,
                level,
                scope,
            }) => {
                if let Some(heading) = heading {
                    writeln!(writer, "heading: {heading}")?;
                }
                if let Some(level) = level {
                    writeln!(writer, "heading_level: {level}")?;
                }
                if let Some(scope) = scope {
                    writeln!(writer, "scope: {scope}")?;
                }
            }
            EntityMetadata::Code(_) => {}
        }
    }
    let max_lines = match options.preview {
        PreviewMode::None => 1,
        PreviewMode::Short => 10,
        PreviewMode::Full => usize::MAX,
    };
    let first = item.content_range.start_line();
    let lines: Vec<_> = item.content.lines().collect();
    let anchor = item
        .excerpt_range
        .as_ref()
        .unwrap_or(&item.range)
        .start_line()
        .zip(first)
        .map_or(0, |(matched, first)| matched.saturating_sub(first))
        .min(lines.len().saturating_sub(1));
    let from = if options.preview == PreviewMode::None {
        anchor
    } else if options.preview == PreviewMode::Short {
        anchor.saturating_sub(3)
    } else {
        0
    };
    for (offset, line) in lines.iter().enumerate().skip(from).take(max_lines) {
        let line = if options.preview == PreviewMode::Full {
            (*line).to_owned()
        } else {
            line.chars()
                .take(if options.human { 120 } else { 160 })
                .collect()
        };
        if let Some(first) = first {
            writeln!(writer, "  {}: {line}", first + offset)?;
        } else {
            writeln!(writer, "  {line}")?;
        }
    }
    if options.preview == PreviewMode::Short && lines.len() > from + max_lines {
        writeln!(writer, "  …")?;
    }
    Ok(())
}

fn range_label(range: &ContentRange) -> String {
    if let (Some(first), Some(last)) = (range.start_line(), range.last_line()) {
        return format!("{first}-{last}");
    }
    if let ContentRange::Byte {
        start_offset,
        end_offset,
    } = range
    {
        return format!("bytes:{start_offset}-{end_offset}");
    }
    "file".to_owned()
}

/// Writes workspace status with the requested presentation and color policy.
/// # Errors
/// Returns the underlying writer error.
pub fn write_info_with_options(
    mut writer: impl Write,
    result: &InfoResult,
    options: crate::OutputOptions,
    terminal: bool,
) -> io::Result<()> {
    let mut buffer = Vec::new();
    write_info_result(&mut buffer, result)?;
    let color = options.color == crate::ColorMode::Always
        || (options.color == crate::ColorMode::Auto
            && terminal
            && std::env::var_os("NO_COLOR").is_none());
    for line in String::from_utf8_lossy(&buffer).lines() {
        if color && let Some((key, value)) = line.split_once(':') {
            writeln!(writer, "\x1b[1;36m{key}\x1b[0m:{value}")?;
            continue;
        }
        if options.human {
            writeln!(writer, "  {line}")?;
        } else {
            writeln!(writer, "{line}")?;
        }
    }
    Ok(())
}

/// Writes a completed index reply.
///
/// # Errors
///
/// Returns the underlying writer error.
pub fn write_index_result(
    mut writer: impl Write,
    root: &Path,
    result: &IndexResult,
) -> io::Result<()> {
    writeln!(writer, "Workspace index: ready")?;
    writeln!(writer, "Root: {}", root.display())?;
    writeln!(
        writer,
        "Files: scanned={} added={} modified={} deleted={} unchanged={} failed={}",
        result.files_scanned,
        result.files_added,
        result.files_modified,
        result.files_deleted,
        result.files_unchanged,
        result.files_failed
    )?;
    for file in &result.failed_files {
        writeln!(writer, "Failed: {}: {}", file.path.display(), file.reason)?;
    }
    writeln!(writer, "Entities: {}", result.entities_created)
}

/// Writes workspace index status.
///
/// # Errors
///
/// Returns the underlying writer error.
pub fn write_info_result(mut writer: impl Write, result: &InfoResult) -> io::Result<()> {
    let state = result.index_status().as_str();
    writeln!(writer, "Workspace index: {state}")?;
    writeln!(writer, "Root: {}", result.root.display())?;
    writeln!(writer, "Index path: {}", result.index_path.display())?;
    match &result.compatibility {
        IndexCompatibility::Unbuilt => {}
        IndexCompatibility::Compatible { version } => {
            writeln!(writer, "Index version: {version}")?;
        }
        IndexCompatibility::RebuildRequired {
            actual_version,
            expected_version,
            reason,
        } => {
            let actual =
                actual_version.map_or_else(|| "unknown".to_owned(), |version| version.to_string());
            writeln!(
                writer,
                "Index version: {actual} (expected {expected_version})"
            )?;
            writeln!(writer, "Reason: {reason}")?;
            if result.suggestion.is_none() {
                writeln!(writer, "Suggestion: zg --index --rebuild")?;
            }
        }
    }
    if let Some(index) = &result.workspace_index {
        writeln!(
            writer,
            "Nested Git repositories: {}",
            if index.scan.nested_git {
                "included"
            } else {
                "excluded"
            }
        )?;
        if let Some(embedding) = &index.embedding {
            writeln!(
                writer,
                "Embedding: {}/{}",
                embedding.provider, embedding.model
            )?;
        }
        if let Some(fts) = &index.fts {
            writeln!(
                writer,
                "FTS: tokenizer={} filters={}",
                fts.tokenizer,
                fts.filters.join(", ")
            )?;
        }
    }
    if let Some(status) = &result.status {
        writeln!(
            writer,
            "Files: scanned={} indexed={} pending={} failed={}",
            status.files_scanned, status.files_indexed, status.files_pending, status.files_failed
        )?;
        for file in &status.failed_files {
            writeln!(writer, "Failed: {}: {}", file.path.display(), file.reason)?;
        }
        writeln!(writer, "Entities: {}", status.entities_indexed)?;
        writeln!(
            writer,
            "Indexed source size: {} bytes",
            status.indexed_size_bytes
        )?;
    }
    if let Some(suggestion) = &result.suggestion {
        writeln!(writer, "Suggestion: {suggestion}")?;
    }
    Ok(())
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
/// Returns an error when `topic` is not part of the public help surface.
pub fn print_help(topic: Option<&str>) -> Result<(), HelpTopicError> {
    println!("{}", help_text(topic)?);
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
Terminal searches use human-readable output and full previews; pipes use compact
output. Use --compact to force compact output.

Examples:
  zg "where authentication is validated"
  zg --fts "AuthService"
  zg --rg -F "AuthService" src
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

Search routes:
  positional query                  Hybrid FTS and vector search
  --hybrid <query>                  Add an explicit hybrid query
  --fts <query>                     Add an exact/lexical query
  --vector <query>                  Add a semantic/vector query
  --fuse                            Fuse all query groups into one ranked list
  --rg                              Run exhaustive managed ripgrep

Result options:
  --limit <n>                       Maximum results per group (default: 7)
  --compact                         Force compact output (default for pipes)
  --preview <none|short|full>       Indexed preview size (default: full on TTY, none in compact mode)
  --debug                           Print diagnostics to stderr
  --trace                           Include per-hit indexed search trace
  --refresh <background|wait|off>   Refresh policy (defaults: server=background, direct=off)
                                    In direct mode, background warns and falls back to off
  --mode <direct|server|auto>       Select indexed query transport (default: auto)

Indexed results are shown by query group, preserving each group's own rank.
A result that matches more than one group is shown in each matching group.

Embedding runtime:
  --api-key <key>                   Embedding provider API key
  --model-cache <path>              Local model cache directory
  --device <device>                 auto, cpu, metal, vulkan, cuda
  --allow-remote                    Allow Remote Embedding for this command only

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
  --name <NAME>                     Set or rename the unique workspace name
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
  --embedding-concurrency <n>       Embedding task concurrency
  --allow-remote                    Allow Remote Embedding for this command only

Scan rules:
  -g, --glob <glob>                 Include paths; prefix with ! to exclude; repeatable
  --iglob <glob>                    Case-insensitive path glob; repeatable
  --hidden[=true|false]              Include hidden paths except .git and .zvec-grep
  --no-ignore[=true|false]           Do not apply default or .gitignore rules
  --nested-git[=true|false]          Scan nested Git repositories and submodules
  --ignore-file <path>              Add an explicit ignore file; repeatable
  --max-depth <n>                   Maximum directory depth
  --max-filesize <size>             Maximum bytes or K/M/G/T size
  -L, --follow[=true|false]          Follow symbolic links safely
  --reset-paths                     Clear inherited scanning settings

Interactive remote indexing asks to allow once, allow for this workspace, or
cancel. Non-interactive indexing requires --allow-remote or a workspace grant.

A new workspace name defaults to root directory name; use --name if it is taken.
Names are case-sensitive and unique within the per-user registry. Naming an
existing workspace renames it while preserving file IDs and active storage.

This version indexes text only with one embedding model per workspace.
Explicit zg --index requires --embedding, ZVEC_GREP_EMBEDDING, or a configured
default when creating a new index.
Search automatically creates a missing index with a configured local model or
local/potion-code-16m-v2, never a remote model.
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

Manage the signed Remote Embedding grant stored in the Workspace under
.zvec-grep/authorization.json. Workspace grants are shared by zg CLI and zg MCP.

--embedding selects the Remote Embedding model to authorize; it does not run
embedding. If omitted, auth grant uses the existing Workspace index model, then
ZVEC_GREP_EMBEDDING.

--endpoint selects the exact provider URL to authorize; otherwise the stored
index endpoint, ZVEC_GREP_ENDPOINT, or provider default is used. Grants bind the
canonical workspace root, model, and endpoint. Granting sends no remote data.

Scopes used during operations:
  once                              Current CLI command or Agent tool call only
  workspace                         Persisted in this Workspace

Use --allow-remote on zg <query> or zg --index to authorize Remote Embedding for
that command only. This authorization is not persisted. API credentials
configure a provider but do not grant permission.

Environment used by auth grant:
  ZVEC_GREP_EMBEDDING               Default model for new indexes and auth grant
  ZVEC_GREP_API_KEY                 Embedding provider credential fallback
  ZVEC_GREP_ENDPOINT                Remote Embedding endpoint fallback
  ZVEC_GREP_AUTHORIZATION_KEY_FILE  Workspace grant signing-key file (advanced)";

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
  zg --install [--target codex|claude|qwen|qoder|opencode|cursor|copilot|vscode|all|auto] [--mcp-transport stdio|http] [--mcp-toolset agent|full] [--yes] [--force]

Options:
  --target <agent>                  codex, claude, qwen, qoder, opencode, cursor, copilot, vscode, auto, or all; repeatable
  --mcp-transport <stdio|http>      MCP connection mode (default: stdio)
  --mcp-toolset <agent|full>        Daemon MCP toolset (default: agent)
  --mcp-tool-timeout <seconds>      MCP tool timeout where supported (default: 600)
  --mcp-token-env <name>            HTTP mode Bearer token environment variable
  --yes                             Install detected agents without prompting
  --force                           Replace conflicting unmanaged configuration

The qoder target configures Qoder CLI and Qoder IDE together.
The copilot target configures GitHub Copilot CLI and Agent Host. The vscode
target configures detected VS Code profiles and shared Copilot instructions.

Interactive setup detects supported agents, configures stdio by default, and
starts the shared daemon. In stdio mode an agent reconnect also starts the
daemon automatically after a reboot. HTTP users manage later daemon restarts.
Codex, Claude Code, Qwen Code, Qoder CLI, OpenCode, Copilot, and VS Code receive managed
guidance. Qoder IDE has no supported global Rules file, so only its MCP
configuration is managed.
Codex and Claude Code receive local tool pre-approval. Remote Embedding
authorization remains separate and is requested by zvec-grep on first remote
use. Restart the agent or open a new session after installation. This does not
install the npm package.";

const UNINSTALL_HELP: &str = r"Usage:
  zg --uninstall [--target codex|claude|qwen|qoder|opencode|cursor|copilot|vscode|all|auto] [--yes]

Removes zvec-grep-managed MCP configuration, agent-specific approval, and
guidance. The qoder target removes the managed Qoder CLI and IDE integration
together.";

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

Qwen credential aliases:
  DASHSCOPE_API_KEY  Qwen credential fallback after ZVEC_GREP_API_KEY
  QWEN_API_KEY       Qwen credential fallback after DASHSCOPE_API_KEY

State and authorization:
  ZVEC_GREP_HOME                    Runtime and daemon state directory; Workspace indexes stay under <root>/.zvec-grep
  ZVEC_GREP_AUTHORIZATION_KEY_FILE  Workspace grant signing-key file (advanced)

Advanced:
  ZVEC_GREP_METAL_KEEP_RESIDENCY       Set to 1 to keep llama.cpp Metal residency enabled (advanced)
  ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM  Positive llama.cpp context parallelism override (advanced)
  NO_COLOR                             Disable terminal colors

Agent integration paths:
  CODEX_HOME            Codex configuration directory used by zg --install
  CLAUDE_CONFIG_DIR     Claude configuration directory used by zg --install
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
  Embedding runtime                 CLI > Workspace snapshot > Global config > Environment
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

    #[test]
    fn incompatible_status_shows_versions_reason_and_rebuild_guidance() {
        use zg_engine::api::info::result::{InfoSource, WorkspaceIndexPolicy};
        for (actual_version, actual_label) in [(Some(1), "1"), (None, "unknown")] {
            let result = InfoResult {
                root: "/workspace".into(),
                indexed: false,
                compatibility: IndexCompatibility::RebuildRequired {
                    actual_version,
                    expected_version: 2,
                    reason: "unsupported persisted index".into(),
                },
                index_policy: WorkspaceIndexPolicy::Enabled,
                home: "/workspace/.zvec-grep".into(),
                index_path: "/workspace/.zvec-grep/storage".into(),
                source: InfoSource::Unindexed,
                workspace_index: None,
                status: None,
                suggestion: None,
            };
            let mut output = Vec::new();
            write_info_result(&mut output, &result).expect("status output");
            let output = String::from_utf8(output).expect("UTF-8");
            assert!(output.contains("Workspace index: rebuild_required"));
            assert!(output.contains(&format!("Index version: {actual_label} (expected 2)")));
            assert!(output.contains("unsupported persisted index"));
            assert!(output.contains("zg --index --rebuild"));
        }
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
    fn preview_modes_and_trace_preserve_indexed_result_information() {
        let mut result = ContextResult {
            query: "needle".into(),
            freshness: None,
            background_refresh: None,
            root: std::env::temp_dir(),
            source: ContextSource::Index,
            coverage: ContextCoverage::RankedSample,
            workspace_index: None,
            group_results: vec![],
            diagnostics: ContextDiagnostics::default(),
            items: vec![indexed_item()],
        };
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
        assert!(minimal.contains("1: line1"));
        assert!(!minimal.contains("2: line2"));
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
                documentation: None,
            }));
            let mut buffer = Vec::new();
            write_item_preview(&mut buffer, &item, OutputOptions::default())
                .expect("render symbol metadata");
            let rendered = String::from_utf8(buffer).expect("UTF-8");
            assert_eq!(rendered.lines().next(), Some(expected));
        }
    }
}
