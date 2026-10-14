use std::{
    io::{self, Write},
    path::Path,
};

use thiserror::Error;
use zg_engine::api::context::{
    ContextResult,
    result::{
        CodeMetadata, ContentPreview, ContentRange, ContextItem, ContextItemStatus, EntityMetadata,
        MarkdownMetadata,
    },
};

use crate::output_path;

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
            writeln!(writer, "{}", output_path(&item.relative_path).display())?;
            previous_path = Some(item.relative_path.as_path());
        }
        if let ContentPreview::Image { format, size_bytes } = &item.preview {
            writeln!(writer, "  image/{} ({} bytes)", format.as_str(), size_bytes)?;
        } else if let Some(first) = item.content_range.start_line() {
            writeln!(
                writer,
                "  {first}: {}",
                item.preview.text().unwrap_or_default().trim_end()
            )?;
        } else {
            writeln!(
                writer,
                "  {}",
                item.preview.text().unwrap_or_default().trim_end()
            )?;
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
    if options.json {
        #[cfg(windows)]
        let result = &{
            let mut result = result.clone();
            result.root = output_path(&result.root).to_path_buf();
            if let Some(index) = &mut result.workspace_index {
                index.path = output_path(&index.path).to_path_buf();
            }
            for item in result.items.iter_mut().chain(
                result
                    .group_results
                    .iter_mut()
                    .flat_map(|group| &mut group.items),
            ) {
                item.absolute_path = output_path(&item.absolute_path).to_path_buf();
                item.relative_path = output_path(&item.relative_path).to_path_buf();
            }
            if let Some(rg) = &mut result.diagnostics.rg {
                for path in std::iter::once(&mut rg.command)
                    .chain(&mut rg.ignored_directories)
                    .chain(&mut rg.missing_paths)
                    .chain(&mut rg.searched_paths)
                {
                    *path = output_path(path).to_path_buf();
                }
            }
            result
        };
        serde_json::to_writer_pretty(&mut writer, result).map_err(io::Error::other)?;
        return writeln!(writer);
    }
    let color = options.color == ColorMode::Always
        || (options.color == ColorMode::Auto && terminal && std::env::var_os("NO_COLOR").is_none());
    let heading = |value: String| {
        if color {
            format!("\x1b[1;36m{value}\x1b[0m")
        } else {
            value
        }
    };
    if options.trace {
        writeln!(writer, "{}: {:?}", heading("Context".into()), result.source)?;
    }
    if options.human {
        writeln!(writer, "Query: {}", result.query)?;
    }
    write_index_diagnostics(&mut writer, result, options.trace)?;
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
    write_indexed_items(writer, result, options, heading)
}

fn write_indexed_items(
    mut writer: impl Write,
    result: &ContextResult,
    options: crate::OutputOptions,
    heading: impl Fn(String) -> String,
) -> io::Result<()> {
    if result.items.is_empty() {
        let message = empty_message(result);
        return writeln!(writer, "{message}");
    }
    let groups: Vec<_> = result
        .diagnostics
        .index
        .iter()
        .flat_map(|diagnostics| &diagnostics.result_groups)
        .filter(|group| group.item_count > 0)
        .collect();
    for (index, item) in result.items.iter().enumerate() {
        if index > 0 {
            writeln!(writer)?;
        }
        if groups.len() > 1
            && let Some((group_index, group)) = groups
                .iter()
                .enumerate()
                .find(|(_, group)| group.item_start == index)
        {
            let mut matched_kinds: Vec<_> = result
                .items
                .iter()
                .skip(group.item_start)
                .take(group.item_count)
                .map(|item| item.preview.kind())
                .collect();
            matched_kinds.sort_unstable();
            matched_kinds.dedup();
            writeln!(
                writer,
                "{}",
                heading(format!(
                    "Group {} · matches: {}",
                    group_index + 1,
                    matched_kinds
                        .iter()
                        .map(|kind| kind.as_str())
                        .collect::<Vec<_>>()
                        .join(" + "),
                ))
            )?;
        }
        let range = if matches!(item.preview, ContentPreview::Image { .. }) {
            String::new()
        } else {
            format!(":{}", range_label(&item.range))
        };
        let matched_by = serde_json::to_value(item.matched_by).map_err(io::Error::other)?;
        let label = if options.human {
            format!(
                "{}. {}{} [{}]",
                item.rank,
                output_path(&item.relative_path).display(),
                range,
                item.preview.kind().as_str()
            )
        } else {
            format!(
                "#{} matchedBy={} {}{}",
                item.rank,
                matched_by.as_str().unwrap_or_default(),
                output_path(&item.relative_path).display(),
                range
            )
        };
        writeln!(writer, "{}", heading(label))?;
        if !options.human && !matches!(item.preview, ContentPreview::Image { .. }) {
            writeln!(writer, "type: {}", item.preview.kind().as_str())?;
        }
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

fn empty_message(result: &ContextResult) -> &'static str {
    use zg_engine::api::context::result::EmptyReason;
    if result.diagnostics.empty_reason == Some(EmptyReason::NoSupportedTargets) {
        "No enabled index table supports this query."
    } else {
        "No matches."
    }
}

fn write_index_diagnostics(
    mut writer: impl Write,
    result: &ContextResult,
    trace: bool,
) -> io::Result<()> {
    if let Some(index) = &result.diagnostics.index {
        if index.incomplete {
            writeln!(
                writer,
                "Incomplete results: one or more target searches failed."
            )?;
        }
        for target in &index.targets {
            if trace || target.reason.is_some() {
                writeln!(
                    writer,
                    "route: input={} target={} model={} status={:?}{}",
                    index.input_kind.as_str(),
                    target.kind.as_str(),
                    target.model_ref,
                    target.status,
                    target
                        .reason
                        .as_ref()
                        .map_or_else(String::new, |reason| format!(" ({reason})")),
                )?;
            }
        }
        if trace {
            for group in &index.result_groups {
                writeln!(
                    writer,
                    "group: {} kinds={} models={} scoring={} recall={:?} merge={}; ranked within group",
                    group.id,
                    group
                        .kinds
                        .iter()
                        .map(|kind| kind.as_str())
                        .collect::<Vec<_>>()
                        .join(","),
                    group.model_refs.join(","),
                    if group.rule_ranking {
                        "rule".to_owned()
                    } else {
                        format!("{:?}", group.scoring)
                    },
                    group.scoring,
                    if group.kinds.len() > 1 {
                        "shared_recall_then_rules"
                    } else {
                        "independent_kind"
                    }
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
    if let ContentPreview::Image { format, size_bytes } = &item.preview {
        writeln!(
            writer,
            "type: image/{}; size: {} bytes",
            format.as_str(),
            size_bytes
        )?;
        return Ok(());
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
    let lines: Vec<_> = item.preview.text().unwrap_or_default().lines().collect();
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
  zg --input <path> [options]
  zg --<management-command> [options]

Search:
  <query>        Search indexed context (no command prefix)
  --rg           Run managed ripgrep
  --input        Search using a file's contents
  --kind         Restrict indexed search to text, code or image

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
  zg --input photo.png --kind image
  zg --fts "AuthService"
  zg --rg -F "AuthService" src
  zg --index --embedding local/potion-code-16m-v2
  zg --status
  zg --auth status
  zg --server on
  zg --config model set local/potion-code-16m-v2 --device metal
  zg --install

Environment:
  ZVEC_GREP_INDEX_DIR   Complete workspace data directory; --index-dir wins
  ZVEC_GREP_HOME        Runtime and daemon state directory; separate from workspace indexes
  ZVEC_GREP_MODE        Default client mode: direct, server, or auto
  ZVEC_GREP_EMBEDDING   Default model for new indexes and auth grant
  ZVEC_GREP_API_KEY     Embedding provider credential fallback
  ZVEC_GREP_SERVER_URL  MCP server URL used by CLI clients

Run zg --help models or zg --help file-types for supported indexing capabilities.
--index-dir overrides ZVEC_GREP_INDEX_DIR. Paths are relative to the command
working directory. Existing indexes resolve their saved source directory relative
to the index directory; --rg ignores index selection.
Run zg --help environment for all variables, scopes, aliases, and precedence.
Run zg --help search or zg --help <topic> for specific help.
Use zg -h/--help for this page and zg -v/--version for the version."#;

const SEARCH_HELP: &str = r#"Usage:
  zg <query> [options]
  zg --input <path> [options]
  zg --hybrid <query> --fts <query> --vector <query> [--fuse]
  zg --rg [rg-options] <pattern> [path...]

Query input:
  positional query                  Literal text, even when it names an existing file
  --input <path>                   Read one file as the query; PNG, JPEG or static WebP only

--input cannot be combined with text queries. Its image contents use vector
search; positional text uses hybrid search by default.

Search modes:
  --hybrid <query>                  Add an explicit hybrid query
  --fts <query>                     Add an exact/lexical query
  --vector <query>                  Add a semantic/vector query
  --fuse                            Fuse query groups within each compatible result group
  --rg                              Run exhaustive managed ripgrep

Search scope:
  --index-dir <path>               Exact index data directory; overrides ZVEC_GREP_INDEX_DIR
  --kind <text|code|image>           Search only this content kind; default: all supported targets

Without an explicit index directory, search finds the nearest .zvec-grep upwards
from the current directory. An explicit directory must contain an existing index.
Its saved source location is independent of the query's working directory.
--rg ignores index selection and scans its current directory or supplied paths.

Result options:
  --limit <n>                       Total result cap, 1..2000 (default: 30); shared across groups
  --compact                         Force compact output (default for pipes)
  --json                            Emit complete structured results and groups
  --preview <none|short|full>       Indexed preview size (default: full on TTY, none in compact mode)
  --debug                           Print diagnostics to stderr
  --trace                           Show search source, targets, grouping and per-hit trace
  --refresh <background|wait|off>   Refresh policy (defaults: server=background, direct=off)
                                    In direct mode, background warns and falls back to off
  --mode <direct|server|auto>       Select indexed query transport (default: auto)

--refresh off does not stop an already active watcher; it prevents this request
from scheduling an update.

Input type and target kind are independent. Text can search image tables whose
model supports text-to-image retrieval. Without --kind, all supported
configured tables are searched. Unsupported paths are reported and skipped.
Text/code tables with compatible query encoding share vector recall; full-text
recall with the same configuration can also be combined using approximate BM25
scores from each table. Hybrid recall is fused before shared rule ranking.
All text/code searches apply rule ranking; images keep vector ranking separately.
Incompatible model/encoding groups stay separate. --limit is allocated one item
per group in text/code/image order until exhausted; each group keeps its own
ranking. Scores are retrieval or rule scores, not probabilities.

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
  ZVEC_GREP_INDEX_DIR    Complete workspace data directory; --index-dir wins
  ZVEC_GREP_MODE         Default client mode: direct, server, or auto
  ZVEC_GREP_API_KEY      Embedding provider credential fallback
  ZVEC_GREP_ENDPOINT     Remote Embedding endpoint fallback
  ZVEC_GREP_MODEL_CACHE  Local embedding model cache directory
  ZVEC_GREP_DEVICE       Local embedding device: auto, cpu, metal, vulkan, or cuda

See zg --help environment for precedence and Server-mode scope."#;

const INDEX_HELP: &str = r"Usage:
  zg --index [root] [options]
  zg --index [root] --rebuild [options]
  zg --index [root] --index-dir <path> [options]
  zg --index [root] --drop [--yes]

Index options:
  --index-dir <path>               Complete workspace data directory; overrides ZVEC_GREP_INDEX_DIR
  --name <NAME>                     Set or rename the workspace label
  --rebuild                         Rebuild the existing index
  --drop                            Permanently remove the workspace index
  --yes                             Confirm --drop without prompting
  --debug                           Print skipped-file diagnostics to stderr
  --mode <direct|server|auto>       Select indexing transport

Embedding options:
  --embedding <model|kind=model>    Set the default or a text/code/image model; repeatable
  --api-key <key>                   Save the default remote model's key encrypted
  --endpoint <url>                  Endpoint override for the default model
  --model-cache <path>              Local model cache directory
  --device <device>                 auto, cpu, metal, vulkan, cuda
  --index-embedding-concurrency <n>  Positive index embedding concurrency
  --allow-remote                    Allow Remote Embedding for this command only

Index embedding concurrency:
  Applies only to index construction and updates, including automatic indexing
  and refresh; query-vector inference keeps its own defaults. The CLI option
  is accepted only with --index and is never saved in workspace settings.
  llama.cpp: context limit capped at 8, with serialized batches; defaults to 1.
  Transformers/ORT: in-flight batch limit capped at 8; defaults to 1. Native
    sessions honor that budget; CoreML uses one physical session.
  EmbeddingGemma2: one batch at a time across its shared ONNX sessions.
  Potion/model2vec: concurrent batch tasks; defaults to 2, with an independent
    CPU worker-pool capacity. No cap of 8 is applied.
  Remote: concurrent requests; adaptive scheduling may reduce the limit after
    failures or rate limits. No cap of 8 is applied.
  Precedence: CLI/API override, ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY, legacy
    ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM for llama.cpp indexing, then defaults.
  Explicit CLI overrides work without restarting the Server. Environment
    changes require restarting the Server so it inherits the new value.

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

The saved key is portable with the complete index data directory. Environment and
global configuration keys are not saved. Omit --api-key to keep a saved key;
repeat it to replace the key after successful indexing. Keep the index directory private.

Interactive remote indexing asks to allow once, allow for this workspace, or
cancel. Non-interactive indexing requires --allow-remote or a workspace grant.

A new workspace name defaults to its root directory name. Names are labels;
different roots may share a name. --name updates it after successful indexing
while preserving file IDs and active storage. No global registration is required.

An existing index uses its saved source directory when [root] is omitted.
Supply [root] to create a new explicit --index-dir or to relocate its source.
The source is saved relative to the index directory; cross-drive layouts that
cannot be represented by a relative path are rejected. Move both directories
together, keeping their relative layout, while no writers or watchers are active.
Missing or unreadable source directories fail indexed operations; --rg still
searches its current directory and requested paths without reading the index.

Indexes text, code, PNG, JPEG and static WebP images. Each content kind selects
one model. Unsupported default kinds are skipped; explicit unsupported models fail.
Vector queries search enabled tables whose models support the query's input kind.
Explicit zg --index requires a default model from --embedding <model>,
ZVEC_GREP_EMBEDDING, or configuration when creating a new index.
Text search automatically creates a missing index only when no --index-dir or
ZVEC_GREP_INDEX_DIR is selected, using a configured local model or
local/potion-code-16m-v2, never a remote model. --input requires an existing index.

Repeat --embedding to configure different kinds, for example:
  zg --index --embedding local/potion-code-16m-v2 --embedding image=local/embeddinggemma-2
Use --embedding image=default to remove that override and inherit the workspace
default, including future changes. If the default does not support images, their
index is disabled. Omitted settings preserve saved values; a bare model changes
only the default. Repeating the default or the same kind is an error.
Changing enabled tables, models, vector dimensions or metrics requires --rebuild.
Changing an endpoint does not. --endpoint applies only to the default model;
configure other remote models with zg --config model set <model> --endpoint <url>.
Failed files are recorded; successful files remain searchable after a rebuild.
Compatible indexes reuse their stored model.
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
  ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY  Positive index/update concurrency override
  Applies to automatic indexing and refresh, never query-vector inference.
  Explicit CLI/API index settings take precedence. Invalid values warn and use
  automatic selection. Server mode reads the Server process environment;
  restart the Server after changing it. Overrides are not persisted.

See zg --help environment for precedence and Server-mode scope.";

const STATUS_HELP: &str = r"Usage:
  zg --status [root] [--index-dir <path>] [--mode <direct|server|auto>] [--check-ready] [--json]

Shows the workspace root and policy, effective model and entity count for each
content-kind table, index state, refresh status, and suggested next action.
--json emits the complete structured status. [root] is the default-index discovery
starting point; --index-dir selects an exact index and its saved source location.

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
Workspaces may route content kinds to different models. Existing indexes retain
their default and routes until an explicit reconfiguration and rebuild.

Global configuration is stored in ~/.zvec-grep/config.json.";

const AUTH_HELP: &str = r"Usage:
  zg --auth grant [root] --capability embedding --scope workspace [--embedding <model>]
  zg --auth status [root]
  zg --auth revoke [root]

Manage the signed Remote Embedding grant stored in the Workspace under
authorization.json in the selected index directory. Workspace grants are shared
by zg CLI and zg MCP. --index-dir selects that directory; moving source roots or
changing machines requires fresh remote authorization.

--embedding selects the Remote Embedding model to authorize; it does not run
embedding. If omitted, auth grant uses the existing Workspace index model, then
ZVEC_GREP_EMBEDDING.

--endpoint selects the exact provider URL to authorize; otherwise the stored
index endpoint, ZVEC_GREP_ENDPOINT, or provider default is used. Grants bind the
canonical workspace root, model, and endpoint. Granting sends no remote data.

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
  ZVEC_GREP_INDEX_DIR   Complete workspace data directory; --index-dir wins
  ZVEC_GREP_HOME               Runtime and daemon state directory; separate from workspace indexes
  ZVEC_GREP_SERVER_URL         MCP server URL used by CLI clients
  ZVEC_GREP_SERVER_TOKEN       Server/client Bearer token
  ZVEC_GREP_SERVER_TOKEN_FILE  File containing the Server/client Bearer token
  ZVEC_GREP_MCP_TOOLSET        Server MCP surface: agent or full

See zg --help environment for daemon startup scope.";

const INSTALL_HELP: &str = r"Usage:
  zg --install [--target codex|claude|qwen|qoder|opencode|cursor|copilot|vscode|grok|all|auto] [--mcp-transport stdio|http] [--mcp-toolset agent|full] [--yes] [--force]

Options:
  --target <agent>                  codex, claude, qwen, qoder, opencode, cursor, copilot, vscode, grok, auto, or all; repeatable
  --mcp-transport <stdio|http>      MCP connection mode (default: stdio)
  --mcp-toolset <agent|full>        Daemon MCP toolset (default: agent)
  --mcp-tool-timeout <seconds>      MCP tool timeout where supported (default: 600)
  --mcp-token-env <name>            HTTP mode Bearer token environment variable
  --yes                             Install detected agents without prompting
  --force                           Replace conflicting unmanaged configuration

The qoder target configures Qoder CLI and Qoder IDE together.
The copilot target configures GitHub Copilot CLI and Agent Host. The vscode
target configures detected VS Code profiles and shared Copilot instructions.
The grok target configures Grok Build's user-level config.toml, global
rules file, and tool pre-approval.

Interactive setup detects supported agents, configures stdio by default, and
starts the shared daemon. In stdio mode an agent reconnect also starts the
daemon automatically after a reboot. HTTP users manage later daemon restarts.
Codex, Claude Code, Qwen Code, Qoder CLI, OpenCode, Copilot, VS Code, and
Grok Build receive managed guidance. Qoder IDE has no supported global Rules
file, so only its MCP configuration is managed.
Codex and Claude Code receive local tool pre-approval. Grok Build receives
pre-approval through its permission rules unless the configuration already
defines a [permission] table. Remote Embedding
authorization remains separate and is requested by zvec-grep on first remote
use. Restart the agent or open a new session after installation. This does not
install the npm package.";

const UNINSTALL_HELP: &str = r"Usage:
  zg --uninstall [--target codex|claude|qwen|qoder|opencode|cursor|copilot|vscode|grok|all|auto] [--yes]

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

Supported embedding models (content kinds route to one model each):
  MODEL                               RUNTIME  INPUT       DIMS  TOKENS  BACKEND
  ----------------------------------  -------  ----------  ----  ------  ---------------
  local/all-minilm-l6-v2              local    text         384     256  transformers
  local/bge-small-en-v1.5             local    text         384     512  transformers
  local/embeddinggemma-300m           local    text         768    2048  llama-cpp
  local/embeddinggemma-2              local    text,image   768    8192  onnx
  local/gte-modernbert-base           local    text         768    8192  transformers
  local/jina-embeddings-v2-base-code  local    text         768    8192  transformers
  local/multilingual-e5-small         local    text         384     512  transformers
  local/nomic-embed-text-v1.5         local    text         768    8192  transformers
  local/potion-code-16m-v2            local    text         256    1024  model2vec
  local/potion-multilingual-128m      local    text         256    1024  model2vec
  local/potion-retrieval-32m          local    text         512    1024  model2vec
  local/qwen3-embedding-0.6b          local    text        1024    8192  llama-cpp
  qwen/qwen3-vl-embedding             remote   text,image  2560   32000  qwen
  qwen/qwen3.7-text-embedding         remote   text        1024  128000  qwen
  qwen/text-embedding-v4              remote   text        1024    8192  qwen

Local models are downloaded to the model cache on first use. Remote models
require provider credentials plus --allow-remote or a Workspace authorization.
Text models also support code content. PNG, JPEG and static WebP image content
is supported by local/embeddinggemma-2 and qwen/qwen3-vl-embedding.
EmbeddingGemma 2 runs on CPU in this release (--device auto or cpu).

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

Images (one entity per independent file):
  png       .png
  jpeg      .jpg, .jpeg
  webp      .webp (static only)
  Animated PNG/WebP, embedded images and PDF image extraction are not supported.
  Image inputs must be at most 10 MiB and 40 megapixels, within model limits.

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
  ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY  Positive index/update concurrency override
  Applies to automatic indexing and refresh, never query-vector inference.
  Precedence: explicit CLI/API index setting, this variable, legacy llama.cpp
  index setting, then backend defaults. Invalid values warn and use defaults.
  Server mode reads the Server process environment; restart it after changes.
  These overrides are not persisted in workspace settings.

Qwen credential aliases:
  DASHSCOPE_API_KEY  Qwen credential fallback after ZVEC_GREP_API_KEY
  QWEN_API_KEY       Qwen credential fallback after DASHSCOPE_API_KEY

State and authorization:
  ZVEC_GREP_CONFIG                  Global configuration file override; use for isolated configurations
  ZVEC_GREP_INDEX_DIR               Complete workspace index directory; --index-dir wins
  ZVEC_GREP_WORKSPACE_REGISTRY      Workspace-name registry file override
  ZVEC_GREP_HOME                    Runtime and daemon state directory; separate from workspace indexes
  ZVEC_GREP_AUTHORIZATION_KEY_FILE  Workspace grant signing-key file (advanced)

Advanced:
  ZVEC_GREP_METAL_KEEP_RESIDENCY       Set to 1 to keep llama.cpp Metal residency enabled (advanced)
  ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM  Legacy index-only llama.cpp fallback after the index concurrency variable
  NO_COLOR                             Disable terminal colors

Agent integration paths:
  CODEX_HOME            Codex configuration directory used by zg --install
  CLAUDE_CONFIG_DIR     Claude configuration directory used by zg --install
  GROK_HOME             Grok Build configuration directory used by zg --install
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
  Remote endpoint/credential      CLI > Workspace snapshot > Global config > Environment
  Local device/model cache        CLI > Environment > Global config > Default
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
            preview: ContentPreview::Text(
                (1..=20)
                    .map(|n| format!("line{n}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            content_role: Some(ContextContentRole::Source),
            status: ContextItemStatus::Fresh,
            score: Some(0.75),
            matched_by: MatchedBy::Fts,
            metadata: None,
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
                item.preview = ContentPreview::Text(if whole_entity {
                    source
                } else {
                    source[17..].to_owned()
                });
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
    fn image_result_displays_metadata_without_line_numbers() {
        let mut item = indexed_item();
        item.preview = ContentPreview::Image {
            format: zg_engine::api::context::options::FileFormat::Png,
            size_bytes: 123,
        };
        item.relative_path = "picture.png".into();
        item.range = ContentRange::File;
        item.content_range = ContentRange::File;
        item.excerpt_range = None;
        let mut result = ContextResult {
            query: "image:input.png".into(),
            freshness: None,
            background_refresh: None,
            root: std::env::temp_dir(),
            source: ContextSource::Index,
            coverage: ContextCoverage::RankedSample,
            workspace_index: None,
            items: vec![item],
            group_results: vec![],
            diagnostics: ContextDiagnostics::default(),
        };
        result.diagnostics.index = Some(serde_json::from_value(serde_json::json!({
            "input_kind":"image", "targets":[], "incomplete":false, "limit":30,
            "hits_returned":1, "query_groups":[], "routes":[], "result_groups":[
                {"id":"text", "kinds":["text"], "model_refs":["fixture/model"], "scoring":"vector", "rule_ranking":true, "item_start":0, "item_count":0},
                {"id":"image", "kinds":["image"], "model_refs":["fixture/model"], "scoring":"vector", "rule_ranking":false, "item_start":0, "item_count":1}
            ]
        })).expect("leading empty group"));
        let mut output = Vec::new();
        write_context_with_options(&mut output, &result, OutputOptions::default(), false)
            .expect("render");
        let text = String::from_utf8(output).expect("UTF-8");
        assert!(text.contains("picture.png\n"));
        assert!(text.contains("image/png; size: 123 bytes"));
        assert!(!text.contains("entity:"));
        assert!(!text.contains("generation:"));
        assert!(!text.contains("picture.png:"));
        assert!(!text.contains("Group "));
        assert!(!text.contains("ranked"));

        result.items.insert(0, indexed_item());
        let groups = &mut result
            .diagnostics
            .index
            .as_mut()
            .expect("index diagnostics")
            .result_groups;
        groups[0]
            .kinds
            .push(zg_engine::api::context::options::ContentKind::Code);
        groups[0].item_count = 1;
        groups[1].item_start = 1;
        for human in [false, true] {
            let mut output = Vec::new();
            write_context_with_options(
                &mut output,
                &result,
                OutputOptions {
                    human,
                    ..OutputOptions::default()
                },
                false,
            )
            .expect("render separate result groups");
            let text = String::from_utf8(output).expect("UTF-8");
            assert!(!text.contains("Results are ranked within each group."));
            assert!(!text.contains("Hits:"));
            assert!(text.contains("Group 1 · matches: text\n"));
            assert!(text.contains("Group 2 · matches: image\n"));
            assert!(!text.contains("text + code"));
            assert!(!text.contains("rule-ranked"));
            if human {
                assert!(text.contains("1. sample.rs:1-20 [text]"));
                assert!(text.contains("1. picture.png [image]"));
                assert!(!text.contains("type: text"));
            } else {
                assert!(text.contains("#1 matchedBy=fts sample.rs:1-20"));
                assert!(text.contains("#1 matchedBy=fts picture.png"));
                assert!(text.contains("type: text"));
            }
        }
    }

    #[test]
    fn json_formats_path_fields_without_changing_queries_or_content() {
        use zg_engine::api::context::result::{
            ContextGroupResult, ContextQueryGroupRole, ContextWorkspaceIndex, RgDiagnostics,
        };
        let root = r"\\?\D:\workspace\repo";
        let file = r"\\?\D:\workspace\repo\sample.rs";
        let mut item = indexed_item();
        item.absolute_path = file.into();
        item.preview = ContentPreview::Text(file.into());
        let result = ContextResult {
            query: root.into(),
            freshness: None,
            background_refresh: None,
            root: root.into(),
            source: ContextSource::Index,
            coverage: ContextCoverage::RankedSample,
            workspace_index: Some(ContextWorkspaceIndex {
                name: root.into(),
                path: r"\\?\D:\workspace\repo\.zvec-grep".into(),
            }),
            items: vec![item.clone()],
            group_results: vec![ContextGroupResult {
                id: "primary".into(),
                query: root.into(),
                role: ContextQueryGroupRole::Primary,
                items: vec![item],
            }],
            diagnostics: ContextDiagnostics {
                rg: Some(RgDiagnostics {
                    backend: "embedded".into(),
                    command: r"\\?\D:\tools\zg.exe".into(),
                    args: vec![root.into()],
                    ignored_directories: vec![r"\\?\D:\workspace\repo\ignored".into()],
                    missing_paths: vec![r"\\?\D:\workspace\repo\missing".into()],
                    searched_paths: vec![root.into()],
                    limit: None,
                    truncated: false,
                }),
                ..ContextDiagnostics::default()
            },
        };
        let original = result.clone();
        let mut expected = serde_json::to_value(&result).expect("context");
        if cfg!(windows) {
            expected["root"] = r"D:\workspace\repo".into();
            expected["workspace_index"]["path"] = r"D:\workspace\repo\.zvec-grep".into();
            expected["items"][0]["absolute_path"] = r"D:\workspace\repo\sample.rs".into();
            expected["group_results"][0]["items"][0]["absolute_path"] =
                r"D:\workspace\repo\sample.rs".into();
            expected["diagnostics"]["rg"]["command"] = r"D:\tools\zg.exe".into();
            expected["diagnostics"]["rg"]["ignored_directories"][0] =
                r"D:\workspace\repo\ignored".into();
            expected["diagnostics"]["rg"]["missing_paths"][0] = r"D:\workspace\repo\missing".into();
            expected["diagnostics"]["rg"]["searched_paths"][0] = r"D:\workspace\repo".into();
        }
        let mut bytes = Vec::new();
        write_context_with_options(
            &mut bytes,
            &result,
            OutputOptions {
                json: true,
                ..OutputOptions::default()
            },
            false,
        )
        .expect("JSON query output");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).expect("JSON"),
            expected
        );
        assert_eq!(result, original);
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
        result.diagnostics.index = Some(serde_json::from_value(serde_json::json!({
            "input_kind":"text", "targets":[], "incomplete":false, "limit":30,
            "hits_returned":1, "query_groups":[], "routes":[], "result_groups":[
                {"id":"text", "kinds":["text","code"], "model_refs":["fixture/model"], "scoring":"full_text", "rule_ranking":true, "item_start":0, "item_count":1}
            ]
        })).expect("shared text/code group"));
        let render = |preview, trace, human| {
            let mut buffer = Vec::new();
            write_context_with_options(
                &mut buffer,
                &result,
                OutputOptions {
                    trace,
                    human,
                    preview,
                    ..OutputOptions::default()
                },
                false,
            )
            .expect("render");
            String::from_utf8(buffer).expect("UTF-8")
        };
        let minimal = render(PreviewMode::None, false, false);
        assert!(minimal.contains("#1 matchedBy=fts sample.rs:1-20"));
        assert!(minimal.contains("1: line1"));
        assert!(!minimal.contains("2: line2"));
        let short = render(PreviewMode::Short, false, false);
        assert!(short.contains("10: line10"));
        assert!(!short.contains("11: line11"));
        let full = render(PreviewMode::Full, true, true);
        assert!(full.contains("20: line20"));
        assert!(full.contains("score: 0.7500"));
        assert!(full.contains("Context: Index"));
        assert!(full.contains("group: text kinds=text,code models=fixture/model scoring=rule"));
        assert!(!minimal.contains("Context:"));
        assert!(!minimal.contains("Group "));
        assert!(!minimal.contains("rule-ranked"));
        let human = render(PreviewMode::Full, false, true);
        assert!(!human.contains("Context:"));
        assert!(human.contains("Query: needle\n1. sample.rs:1-20 [text]"));
        assert!(!human.contains("type: text"));
        assert!(!human.contains("Group "));

        result.items[0].range = ContentRange::Text {
            start_line: 1,
            end_line: 21,
            start_byte_offset: 0,
            end_byte_offset: 131,
            start_byte_column: 0,
            end_byte_column: 0,
        };
        let ContentPreview::Text(text) = &mut result.items[0].preview else {
            panic!("text preview")
        };
        text.push('\n');
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
