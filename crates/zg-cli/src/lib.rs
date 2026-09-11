//! Command-line parsing, validation, request construction, and terminal rendering.

mod authorization;
mod install;
mod jsonc;
mod managed_rg;
mod render;

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    str::FromStr,
};

use chrono::{DateTime, Local, NaiveDate, TimeZone};
use clap::{Args, Parser, Subcommand, ValueEnum};
use thiserror::Error;
use zg_engine::api::{
    context::{
        ContextOptions,
        options::{ContextRoute, ContextRouteMode, SymbolType},
    },
    index::{
        IndexOptions,
        options::{Device, DiscoveryOptions, EmbeddingModelSpec, RootPath},
    },
    info::InfoOptions,
};

pub use authorization::{AuthorizationDecision, prompt_index_authorization};

pub use install::{
    InstallError, InstallOutcome, execute_install, execute_uninstall, resolve_server_listen,
    resolve_server_url,
};
pub use managed_rg::{ManagedRgArgumentError, parse_managed_rg_args};
pub use render::{
    HelpTopicError, help_text, print_help, write_context_result, write_index_result,
    write_info_result,
};

const DEFAULT_LISTEN: &str = "127.0.0.1:7999";

#[derive(Debug, Parser)]
#[command(
    name = "zg",
    about = "Agent-friendly workspace search",
    disable_help_flag = true,
    disable_help_subcommand = true,
    disable_version_flag = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<CommandLine>,
}

#[derive(Debug, Subcommand)]
pub enum CommandLine {
    /// Search indexed context or run managed ripgrep.
    Query(QueryArgs),
    /// Build, rebuild, or drop the workspace index.
    Index(IndexArgs),
    /// Show workspace and index status.
    Status(StatusArgs),
    /// Manage the resident MCP daemon.
    Server(ServerArgs),
    /// Configure provider credentials and model defaults.
    Config(UnsupportedArgs),
    /// Manage workspace Remote Embedding authorization.
    Auth(AuthArgs),
    /// Install agent integrations.
    Install(InstallArgs),
    /// Remove agent integrations.
    Uninstall(UninstallArgs),
    /// Show help for a command or topic.
    Help(HelpArgs),
    /// Print the installed version.
    Version,
}

#[derive(Debug, Args)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub action: AuthAction,
    #[arg(global = true)]
    pub root: Option<PathBuf>,
    #[arg(long, global = true)]
    pub embedding: Option<String>,
    #[arg(long, global = true)]
    pub endpoint: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum AuthAction {
    Grant {
        #[arg(long, value_parser = ["embedding"], required = true)]
        capability: String,
        #[arg(long, value_parser = ["workspace"], required = true)]
        scope: String,
    },
    Status,
    Revoke,
}

#[derive(Debug, Args)]
pub struct UnsupportedArgs {
    #[arg(
        value_name = "ARG",
        allow_hyphen_values = true,
        trailing_var_arg = true
    )]
    pub args: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum McpInstallTransport {
    #[default]
    Stdio,
    Http,
}

#[derive(Clone, Debug, Args)]
pub struct InstallArgs {
    #[arg(long = "target", value_name = "AGENT", action = clap::ArgAction::Append)]
    pub targets: Vec<String>,
    #[arg(long = "mcp-transport", value_enum)]
    pub transport: Option<McpInstallTransport>,
    #[arg(long = "mcp-toolset", value_enum)]
    pub mcp_toolset: Option<McpToolset>,
    #[arg(
        long = "mcp-tool-timeout",
        value_name = "SECONDS",
        default_value_t = 600,
        value_parser = parse_positive_u64
    )]
    pub mcp_tool_timeout_seconds: u64,
    #[arg(long = "mcp-token-env", value_name = "NAME", value_parser = parse_environment_variable)]
    pub mcp_token_env: Option<String>,
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub force: bool,
    #[arg(value_name = "AGENT")]
    pub positional_targets: Vec<String>,
}

#[derive(Clone, Debug, Args)]
pub struct UninstallArgs {
    #[arg(long = "target", value_name = "AGENT", action = clap::ArgAction::Append)]
    pub targets: Vec<String>,
    #[arg(long)]
    pub yes: bool,
    #[arg(value_name = "AGENT")]
    pub positional_targets: Vec<String>,
}

#[derive(Debug, Args)]
pub struct HelpArgs {
    pub topic: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ClientMode {
    Direct,
    Server,
    Auto,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum McpToolset {
    #[default]
    Agent,
    Full,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum PreviewMode {
    #[default]
    None,
    Short,
    Full,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum RefreshMode {
    Background,
    Wait,
    #[default]
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum DeviceArg {
    Auto,
    Cpu,
    Metal,
    Vulkan,
    Cuda,
}

impl From<DeviceArg> for Device {
    fn from(value: DeviceArg) -> Self {
        match value {
            DeviceArg::Auto => Self::Auto,
            DeviceArg::Cpu => Self::Cpu,
            DeviceArg::Metal => Self::Metal,
            DeviceArg::Vulkan => Self::Vulkan,
            DeviceArg::Cuda => Self::Cuda,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SymbolTypeArg {
    Module,
    Class,
    Interface,
    Function,
    Value,
    Alias,
}

impl From<SymbolTypeArg> for SymbolType {
    fn from(value: SymbolTypeArg) -> Self {
        match value {
            SymbolTypeArg::Module => Self::Module,
            SymbolTypeArg::Class => Self::Class,
            SymbolTypeArg::Interface => Self::Interface,
            SymbolTypeArg::Function => Self::Function,
            SymbolTypeArg::Value => Self::Value,
            SymbolTypeArg::Alias => Self::Alias,
        }
    }
}

#[derive(Clone, Debug, Default, Args)]
pub struct FileSelectionArgs {
    #[arg(short = 'g', long = "glob", value_name = "GLOB")]
    pub globs: Vec<String>,
    #[arg(long = "iglob", value_name = "GLOB")]
    pub insensitive_globs: Vec<String>,
    #[arg(short = 't', long = "type", value_name = "TYPE")]
    pub file_types: Vec<String>,
    #[arg(short = 'T', long = "type-not", value_name = "TYPE")]
    pub excluded_file_types: Vec<String>,
    #[arg(long)]
    pub hidden: bool,
    #[arg(long = "no-ignore")]
    pub no_ignore: bool,
    #[arg(long = "ignore-file", value_name = "PATH")]
    pub ignore_files: Vec<PathBuf>,
    #[arg(long = "max-depth", value_parser = parse_non_negative_usize)]
    pub max_depth: Option<usize>,
    #[arg(long = "max-filesize", value_parser = parse_byte_size)]
    pub max_file_size_bytes: Option<u64>,
    #[arg(short = 'L', long)]
    pub follow: bool,
}

impl FileSelectionArgs {
    fn discovery(&self) -> DiscoveryOptions {
        DiscoveryOptions {
            globs: self.globs.clone(),
            insensitive_globs: self.insensitive_globs.clone(),
            file_types: self.file_types.clone(),
            excluded_file_types: self.excluded_file_types.clone(),
            hidden: self.hidden,
            no_ignore: self.no_ignore,
            ignore_files: self.ignore_files.clone(),
            max_depth: self.max_depth,
            max_file_size_bytes: self.max_file_size_bytes,
            follow: self.follow,
            ..DiscoveryOptions::default()
        }
    }

    fn apply_context(&self, request: &mut ContextOptions) {
        request.globs.extend(self.globs.iter().cloned());
        request
            .insensitive_globs
            .extend(self.insensitive_globs.iter().cloned());
        request.file_types.extend(self.file_types.iter().cloned());
        request
            .excluded_file_types
            .extend(self.excluded_file_types.iter().cloned());
        request.hidden |= self.hidden;
        request.no_ignore |= self.no_ignore;
        request
            .ignore_files
            .extend(self.ignore_files.iter().cloned());
        request.max_depth = self.max_depth.or(request.max_depth);
        request.max_file_size_bytes = self.max_file_size_bytes.or(request.max_file_size_bytes);
        request.follow |= self.follow;
    }

    fn is_empty(&self) -> bool {
        self.globs.is_empty()
            && self.insensitive_globs.is_empty()
            && self.file_types.is_empty()
            && self.excluded_file_types.is_empty()
            && !self.hidden
            && !self.no_ignore
            && self.ignore_files.is_empty()
            && self.max_depth.is_none()
            && self.max_file_size_bytes.is_none()
            && !self.follow
    }
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct QueryArgs {
    #[arg(long, env = "ZVEC_GREP_MODE", default_value = "auto")]
    pub mode: ClientMode,
    #[arg(long = "force-direct")]
    pub force_direct: bool,
    #[arg(long)]
    pub rg: bool,
    #[arg(long)]
    pub debug: bool,
    #[arg(long)]
    pub trace: bool,
    #[arg(long)]
    pub human: bool,
    #[arg(long, value_enum)]
    pub preview: Option<PreviewMode>,
    #[arg(long, value_enum)]
    pub color: Option<ColorMode>,
    #[arg(long = "no-color", conflicts_with = "color")]
    pub no_color: bool,
    #[arg(long, value_parser = parse_positive_usize)]
    pub limit: Option<usize>,
    #[arg(long = "hybrid", value_name = "QUERY")]
    pub hybrid_queries: Vec<String>,
    #[arg(long, value_name = "QUERY")]
    pub fts: Vec<String>,
    #[arg(long, value_name = "QUERY")]
    pub vector: Vec<String>,
    #[arg(long)]
    pub fuse: bool,
    #[arg(long, value_enum)]
    pub refresh: Option<RefreshMode>,
    #[arg(long = "prefer-symbol")]
    pub prefer_symbol: bool,
    #[arg(long = "symbol-type", value_enum)]
    pub symbol_types: Vec<SymbolTypeArg>,
    #[arg(long = "modified-after", value_parser = parse_modified_time)]
    pub modified_after: Option<u64>,
    #[arg(long = "modified-before", value_parser = parse_modified_time)]
    pub modified_before: Option<u64>,
    #[arg(long, env = "ZVEC_GREP_HOME")]
    pub home: Option<PathBuf>,
    #[arg(long = "api-key", env = "ZVEC_GREP_API_KEY")]
    pub api_key: Option<String>,
    #[arg(long = "model-cache", env = "ZVEC_GREP_MODEL_CACHE")]
    pub model_cache: Option<PathBuf>,
    #[arg(long, env = "ZVEC_GREP_DEVICE", value_enum, ignore_case = true)]
    pub device: Option<DeviceArg>,
    #[arg(long = "allow-remote")]
    pub allow_remote: bool,
    #[command(flatten)]
    pub files: FileSelectionArgs,
    #[arg(value_name = "QUERY", allow_hyphen_values = true)]
    pub values: Vec<String>,
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct IndexArgs {
    pub root: Option<PathBuf>,
    #[arg(long, env = "ZVEC_GREP_MODE", default_value = "auto")]
    pub mode: ClientMode,
    #[arg(long)]
    pub rebuild: bool,
    #[arg(long)]
    pub drop: bool,
    #[arg(long)]
    pub yes: bool,
    #[arg(long = "reset-paths")]
    pub reset_paths: bool,
    #[arg(long)]
    pub debug: bool,
    #[arg(long, value_enum)]
    pub color: Option<ColorMode>,
    #[arg(long = "no-color", conflicts_with = "color")]
    pub no_color: bool,
    #[arg(long, env = "ZVEC_GREP_HOME")]
    pub home: Option<PathBuf>,
    #[arg(long, env = "ZVEC_GREP_EMBEDDING")]
    pub embedding: Option<String>,
    #[arg(long = "model-cache", env = "ZVEC_GREP_MODEL_CACHE")]
    pub model_cache: Option<PathBuf>,
    #[arg(long, env = "ZVEC_GREP_DEVICE", value_enum, ignore_case = true)]
    pub device: Option<DeviceArg>,
    #[arg(long = "api-key", env = "ZVEC_GREP_API_KEY")]
    pub api_key: Option<String>,
    #[arg(long, env = "ZVEC_GREP_ENDPOINT")]
    pub endpoint: Option<String>,
    #[arg(long = "embedding-concurrency", value_parser = parse_positive_usize)]
    pub embedding_concurrency: Option<usize>,
    #[arg(long = "allow-remote")]
    pub allow_remote: bool,
    #[command(flatten)]
    pub files: FileSelectionArgs,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    pub root: Option<PathBuf>,
    #[arg(long, env = "ZVEC_GREP_MODE", default_value = "auto")]
    pub mode: ClientMode,
    #[arg(long = "check-ready")]
    pub check_ready: bool,
    #[arg(long)]
    pub human: bool,
    #[arg(long, value_enum)]
    pub color: Option<ColorMode>,
    #[arg(long = "no-color", conflicts_with = "color")]
    pub no_color: bool,
    #[arg(long, env = "ZVEC_GREP_HOME")]
    pub home: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ServerArgs {
    #[arg(long)]
    pub stdio: bool,
    #[arg(long, value_name = "ADDRESS")]
    pub listen: Option<String>,
    #[arg(long, env = "ZVEC_GREP_HOME")]
    pub home: Option<PathBuf>,
    #[arg(long, env = "ZVEC_GREP_MCP_TOOLSET", value_enum)]
    pub mcp_toolset: Option<McpToolset>,
    #[arg(long = "token-file", env = "ZVEC_GREP_SERVER_TOKEN_FILE")]
    pub token_file: Option<PathBuf>,
    #[command(subcommand)]
    pub action: Option<ServerAction>,
}

#[derive(Debug, Subcommand)]
pub enum ServerAction {
    On(ServerStartArgs),
    Off(ServerStopArgs),
    Status(ServerStatusArgs),
    #[command(hide = true)]
    Run(ServerStartArgs),
}

#[derive(Clone, Debug, Args)]
pub struct ServerStartArgs {
    #[arg(long, default_value = DEFAULT_LISTEN)]
    pub listen: String,
    #[arg(long, env = "ZVEC_GREP_HOME")]
    pub home: Option<PathBuf>,
    #[arg(
        long,
        env = "ZVEC_GREP_MCP_TOOLSET",
        value_enum,
        default_value = "agent"
    )]
    pub mcp_toolset: McpToolset,
    #[arg(long = "token-file", env = "ZVEC_GREP_SERVER_TOKEN_FILE")]
    pub token_file: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
pub struct ServerStopArgs {
    #[arg(long, env = "ZVEC_GREP_HOME")]
    pub home: Option<PathBuf>,
    #[arg(long = "token-file", env = "ZVEC_GREP_SERVER_TOKEN_FILE")]
    pub token_file: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
pub struct ServerStatusArgs {
    #[arg(long, env = "ZVEC_GREP_HOME")]
    pub home: Option<PathBuf>,
    #[arg(long = "check-ready")]
    pub check_ready: bool,
}

#[derive(Debug)]
pub enum CliPlan {
    Query {
        mode: ClientMode,
        home: Option<PathBuf>,
        request: Box<ContextOptions>,
        output: OutputOptions,
    },
    Index {
        mode: ClientMode,
        home: Option<PathBuf>,
        operation: IndexOperation,
        output: OutputOptions,
    },
    Status {
        mode: ClientMode,
        home: Option<PathBuf>,
        request: InfoOptions,
        check_ready: bool,
    },
    Auth(AuthArgs),
    Server(ServerPlan),
    Install(InstallArgs),
    Uninstall(UninstallArgs),
    Help(Option<String>),
    Version,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OutputOptions {
    pub debug: bool,
    pub trace: bool,
    pub human: bool,
    pub preview: PreviewMode,
    pub color: ColorMode,
}

#[derive(Debug)]
pub enum IndexOperation {
    Build(Box<IndexOptions>),
    Drop(InfoOptions),
}

#[derive(Debug)]
pub enum ServerPlan {
    Stdio(ServerStartArgs),
    On(ServerStartArgs),
    Off(ServerStopArgs),
    Status(ServerStatusArgs),
    Run(ServerStartArgs),
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("zg query requires text or --hybrid/--fts/--vector routes")]
    MissingQuery,
    #[error("--rg cannot be combined with --hybrid, --fts, --vector, or --fuse")]
    RgWithIndexedRoutes,
    #[error("--rg cannot be combined with indexed preview, trace, refresh, or symbol options")]
    RgWithIndexedOptions,
    #[error("--force-direct requires --mode direct")]
    ForceDirectMode,
    #[error("--json has been removed; use the default agent markdown output or --human")]
    RemovedJson,
    #[error("unknown option: {0}")]
    UnknownQueryOption(String),
    #[error("--modified-after must not be later than --modified-before")]
    InvalidModifiedRange,
    #[error("zg index --drop cannot be combined with indexing options")]
    DropWithIndexOptions,
    #[error("zg index --drop requires --yes in non-interactive Rust mode")]
    DropNeedsConfirmation,
    #[error("use `zg server --stdio` or choose one of: on, off, status, run")]
    MissingServerAction,
    #[error("--stdio cannot be combined with a server action")]
    StdioWithServerAction,
    #[error("--mcp-token-env requires --mcp-transport http")]
    InstallTokenRequiresHttp,
    #[error("{0} is parsed by Rust but its handler has not been ported yet")]
    UnsupportedCommand(&'static str),
    #[error(transparent)]
    ManagedRg(#[from] ManagedRgArgumentError),
}

impl Cli {
    /// Parses process arguments after normalizing the TS CLI's option-anywhere
    /// query syntax for clap.
    #[must_use]
    pub fn parse() -> Self {
        match Self::try_parse_from(std::env::args_os()) {
            Ok(cli) => cli,
            Err(error) => error.exit(),
        }
    }

    /// Parses an explicit argument iterator with TS-compatible query ordering.
    ///
    /// # Errors
    ///
    /// Returns clap's structured parse error for invalid command syntax.
    pub fn try_parse_from<I, T>(arguments: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let arguments = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
        let arguments = normalize_help_and_version(arguments)?;
        <Self as Parser>::try_parse_from(normalize_query_argument_order(arguments))
    }

    /// Converts terminal arguments into a transport-independent execution plan.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when options are incompatible or a command handler
    /// has not yet been ported to Rust.
    pub fn into_plan(self, current_dir: PathBuf) -> Result<CliPlan, CliError> {
        let Some(command) = self.command else {
            return Ok(CliPlan::Help(None));
        };
        match command {
            CommandLine::Query(args) => query_plan(args, current_dir),
            CommandLine::Index(args) => index_plan(args, &current_dir),
            CommandLine::Status(args) => Ok(CliPlan::Status {
                mode: args.mode,
                home: args.home,
                request: InfoOptions {
                    root: Some(resolve_from(&current_dir, args.root.as_deref())),
                    include_status: true,
                },
                check_ready: args.check_ready,
            }),
            CommandLine::Server(args) => server_plan(args).map(CliPlan::Server),
            CommandLine::Help(args) => Ok(CliPlan::Help(args.topic)),
            CommandLine::Version => Ok(CliPlan::Version),
            CommandLine::Config(_) => Err(CliError::UnsupportedCommand("zg config")),
            CommandLine::Auth(mut args) => {
                args.root = Some(resolve_from(&current_dir, args.root.as_deref()));
                Ok(CliPlan::Auth(args))
            }
            CommandLine::Install(args) => {
                if args.mcp_token_env.is_some() && args.transport != Some(McpInstallTransport::Http)
                {
                    return Err(CliError::InstallTokenRequiresHttp);
                }
                Ok(CliPlan::Install(args))
            }
            CommandLine::Uninstall(args) => Ok(CliPlan::Uninstall(args)),
        }
    }
}

fn normalize_help_and_version(mut arguments: Vec<OsString>) -> Result<Vec<OsString>, clap::Error> {
    let first = arguments
        .get(1)
        .and_then(|value| value.to_str())
        .map(str::to_owned);
    match first.as_deref() {
        Some("-h" | "--help") => {
            if arguments.len() != 2 {
                return Err(clap::Error::raw(
                    clap::error::ErrorKind::TooManyValues,
                    format!(
                        "{} does not accept arguments",
                        arguments[1].to_string_lossy()
                    ),
                ));
            }
            arguments[1] = "help".into();
        }
        Some("-v" | "--version") => {
            if arguments.len() != 2 {
                return Err(clap::Error::raw(
                    clap::error::ErrorKind::TooManyValues,
                    format!(
                        "{} does not accept arguments",
                        arguments[1].to_string_lossy()
                    ),
                ));
            }
            arguments[1] = "version".into();
        }
        Some("version")
            if arguments.len() == 3
                && matches!(arguments[2].to_str(), Some("-v" | "--version")) =>
        {
            arguments.pop();
        }
        Some(command)
            if matches!(
                command,
                "query"
                    | "index"
                    | "status"
                    | "config"
                    | "auth"
                    | "server"
                    | "install"
                    | "uninstall"
            ) =>
        {
            let help_requested = arguments
                .iter()
                .skip(2)
                .take_while(|argument| *argument != OsStr::new("--"))
                .any(|argument| matches!(argument.to_str(), Some("-h" | "--help")));
            if help_requested {
                arguments.truncate(1);
                arguments.push("help".into());
                arguments.push(command.into());
            }
        }
        _ => {}
    }
    Ok(arguments)
}

fn normalize_query_argument_order(arguments: Vec<OsString>) -> Vec<OsString> {
    if arguments.get(1).and_then(|value| value.to_str()) != Some("query") {
        return arguments;
    }
    let mut prefix = arguments[..2].to_vec();
    let mut options = Vec::new();
    let mut values = Vec::new();
    let mut literal = Vec::new();
    let mut index = 2;
    let mut after_separator = false;
    while index < arguments.len() {
        let argument = &arguments[index];
        if after_separator {
            literal.push(argument.clone());
            index += 1;
            continue;
        }
        if argument == OsStr::new("--") {
            after_separator = true;
            index += 1;
            continue;
        }
        let text = argument.to_string_lossy();
        if query_option_without_value(&text) || query_attached_option(&text) {
            options.push(argument.clone());
        } else if query_option_with_value(&text) {
            options.push(argument.clone());
            if let Some(value) = arguments.get(index + 1) {
                options.push(value.clone());
                index += 1;
            }
        } else {
            values.push(argument.clone());
        }
        index += 1;
    }
    prefix.extend(options);
    prefix.extend(values);
    if after_separator {
        prefix.push("--".into());
        prefix.extend(literal);
    }
    prefix
}

fn query_option_without_value(value: &str) -> bool {
    matches!(
        value,
        "--force-direct"
            | "--rg"
            | "--debug"
            | "--trace"
            | "--human"
            | "--no-color"
            | "--fuse"
            | "--prefer-symbol"
            | "--allow-remote"
            | "--hidden"
            | "--no-ignore"
            | "--follow"
            | "-L"
    )
}

fn query_option_with_value(value: &str) -> bool {
    matches!(
        value,
        "--mode"
            | "--preview"
            | "--color"
            | "--limit"
            | "--hybrid"
            | "--fts"
            | "--vector"
            | "--refresh"
            | "--symbol-type"
            | "--modified-after"
            | "--modified-before"
            | "--home"
            | "--api-key"
            | "--model-cache"
            | "--device"
            | "--glob"
            | "-g"
            | "--iglob"
            | "--type"
            | "-t"
            | "--type-not"
            | "-T"
            | "--ignore-file"
            | "--max-depth"
            | "--max-filesize"
    )
}

fn query_attached_option(value: &str) -> bool {
    value.starts_with("--mode=")
        || value.starts_with("--preview=")
        || value.starts_with("--color=")
        || value.starts_with("--limit=")
        || value.starts_with("--hybrid=")
        || value.starts_with("--fts=")
        || value.starts_with("--vector=")
        || value.starts_with("--refresh=")
        || value.starts_with("--symbol-type=")
        || value.starts_with("--modified-after=")
        || value.starts_with("--modified-before=")
        || value.starts_with("--home=")
        || value.starts_with("--api-key=")
        || value.starts_with("--model-cache=")
        || value.starts_with("--device=")
        || value.starts_with("--glob=")
        || value.starts_with("--iglob=")
        || value.starts_with("--type=")
        || value.starts_with("--type-not=")
        || value.starts_with("--ignore-file=")
        || value.starts_with("--max-depth=")
        || value.starts_with("--max-filesize=")
        || (value.starts_with("-g") && value.len() > 2)
        || (value.starts_with("-t") && value.len() > 2)
        || (value.starts_with("-T") && value.len() > 2)
}

fn context_refresh_policy(
    mode: ClientMode,
    refresh: Option<RefreshMode>,
) -> zg_engine::api::context::options::RefreshPolicy {
    use zg_engine::api::context::options::RefreshPolicy;
    if mode == ClientMode::Direct && matches!(refresh, Some(RefreshMode::Background)) {
        eprintln!(
            "warning: --refresh background is unavailable in direct mode; falling back to off"
        );
    }
    match (mode, refresh) {
        (_, Some(RefreshMode::Wait)) => RefreshPolicy::Wait,
        (ClientMode::Direct, _) | (_, Some(RefreshMode::Off)) => RefreshPolicy::Off,
        _ => RefreshPolicy::Background,
    }
}

fn query_plan(args: QueryArgs, current_dir: PathBuf) -> Result<CliPlan, CliError> {
    if args.force_direct && args.mode != ClientMode::Direct {
        return Err(CliError::ForceDirectMode);
    }
    if args.rg
        && (!args.hybrid_queries.is_empty()
            || !args.fts.is_empty()
            || !args.vector.is_empty()
            || args.fuse)
    {
        return Err(CliError::RgWithIndexedRoutes);
    }
    if args.rg
        && (args.preview.is_some()
            || args.trace
            || args.refresh.is_some()
            || args.prefer_symbol
            || !args.symbol_types.is_empty())
    {
        return Err(CliError::RgWithIndexedOptions);
    }
    if !args.rg {
        if args.values.iter().any(|value| value == "--json") {
            return Err(CliError::RemovedJson);
        }
        if let Some(option) = args.values.iter().find(|value| value.starts_with("--")) {
            return Err(CliError::UnknownQueryOption(option.clone()));
        }
    }
    let mode = args.mode;
    let home = args.home.clone();
    let output = OutputOptions {
        debug: args.debug,
        trace: args.trace,
        human: args.human,
        preview: args.preview.unwrap_or_default(),
        color: if args.no_color {
            ColorMode::Never
        } else {
            args.color.unwrap_or_default()
        },
    };
    let mut request = if args.rg {
        parse_managed_rg_args(&args.values)?
    } else {
        let queries = args
            .values
            .into_iter()
            .chain(args.hybrid_queries)
            .map(|query| query.trim().to_owned())
            .filter(|query| !query.is_empty())
            .collect::<Vec<_>>();
        let routes = args
            .fts
            .into_iter()
            .map(|query| ContextRoute {
                mode: ContextRouteMode::Fts,
                query,
            })
            .chain(args.vector.into_iter().map(|query| ContextRoute {
                mode: ContextRouteMode::Vector,
                query,
            }))
            .collect::<Vec<_>>();
        if queries.is_empty() && routes.is_empty() {
            return Err(CliError::MissingQuery);
        }
        ContextOptions {
            queries,
            routes,
            fuse: args.fuse,
            limit: args.limit,
            refresh: Some(context_refresh_policy(mode, args.refresh)),
            auto_update: !matches!(args.refresh, Some(RefreshMode::Off))
                && (mode != ClientMode::Direct || matches!(args.refresh, Some(RefreshMode::Wait))),
            trace: args.trace,
            prefer_symbol: args.prefer_symbol,
            symbol_types: args.symbol_types.into_iter().map(Into::into).collect(),
            ..ContextOptions::default()
        }
    };
    request.allow_remote = args.allow_remote;
    request.api_key = args.api_key;
    request.limit = args.limit;
    request.modified_after_epoch_ms = args.modified_after;
    request.modified_before_epoch_ms = args.modified_before;
    if request
        .modified_after_epoch_ms
        .zip(request.modified_before_epoch_ms)
        .is_some_and(|(after, before)| after > before)
    {
        return Err(CliError::InvalidModifiedRange);
    }
    request.root = Some(current_dir);
    args.files.apply_context(&mut request);
    Ok(CliPlan::Query {
        mode,
        home,
        request: Box::new(request),
        output,
    })
}

fn index_plan(args: IndexArgs, current_dir: &Path) -> Result<CliPlan, CliError> {
    let root = resolve_from(current_dir, args.root.as_deref());
    let mode = args.mode;
    let home = args.home.clone();
    let output = OutputOptions {
        debug: args.debug,
        color: if args.no_color {
            ColorMode::Never
        } else {
            args.color.unwrap_or_default()
        },
        ..OutputOptions::default()
    };
    if args.drop {
        if args.rebuild
            || args.reset_paths
            || args.embedding.is_some()
            || args.model_cache.is_some()
            || args.device.is_some()
            || args.api_key.is_some()
            || args.endpoint.is_some()
            || args.embedding_concurrency.is_some()
            || args.debug
            || !args.files.is_empty()
        {
            return Err(CliError::DropWithIndexOptions);
        }
        if !args.yes {
            return Err(CliError::DropNeedsConfirmation);
        }
        return Ok(CliPlan::Index {
            mode,
            home,
            operation: IndexOperation::Drop(InfoOptions {
                root: Some(root),
                include_status: false,
            }),
            output,
        });
    }
    let discovery = args.files.discovery();
    let embedding = args.embedding.map(|reference| EmbeddingModelSpec {
        reference,
        revision: None,
        cache_dir: args.model_cache,
        endpoint: args.endpoint.clone(),
        device: args.device.unwrap_or(DeviceArg::Auto).into(),
    });
    let root_path = RootPath {
        path: root.clone(),
        recursive: true,
        discovery: discovery.clone(),
    };
    Ok(CliPlan::Index {
        mode,
        home,
        operation: IndexOperation::Build(Box::new(IndexOptions {
            root: Some(root),
            roots: vec![root_path],
            rebuild: args.rebuild,
            reset_paths: args.reset_paths,
            discovery,
            embedding,
            allow_remote: args.allow_remote,
            api_key: args.api_key,
            endpoint: args.endpoint.clone(),
            embedding_concurrency: args.embedding_concurrency,
            ..IndexOptions::default()
        })),
        output,
    })
}

fn server_plan(args: ServerArgs) -> Result<ServerPlan, CliError> {
    if args.stdio {
        if args.action.is_some() {
            return Err(CliError::StdioWithServerAction);
        }
        return Ok(ServerPlan::Stdio(ServerStartArgs {
            listen: args.listen.unwrap_or_else(|| DEFAULT_LISTEN.to_owned()),
            home: args.home,
            mcp_toolset: args.mcp_toolset.unwrap_or_default(),
            token_file: args.token_file,
        }));
    }
    if args.listen.is_some() || args.mcp_toolset.is_some() || args.token_file.is_some() {
        return Err(CliError::MissingServerAction);
    }
    match args.action.ok_or(CliError::MissingServerAction)? {
        ServerAction::On(args) => Ok(ServerPlan::On(args)),
        ServerAction::Off(args) => Ok(ServerPlan::Off(args)),
        ServerAction::Status(args) => Ok(ServerPlan::Status(args)),
        ServerAction::Run(args) => Ok(ServerPlan::Run(args)),
    }
}

fn resolve_from(current_dir: &Path, value: Option<&Path>) -> PathBuf {
    let path = value.unwrap_or(current_dir);
    if path.is_absolute() {
        path.to_owned()
    } else {
        current_dir.join(path)
    }
}

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "requires a positive integer".to_owned())
}

fn parse_positive_u64(value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "requires a positive integer".to_owned())
}

fn parse_environment_variable(value: &str) -> Result<String, String> {
    let mut characters = value.chars();
    let valid_first = characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
    if valid_first
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        Ok(value.to_owned())
    } else {
        Err("requires an environment variable name".to_owned())
    }
}

fn parse_non_negative_usize(value: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|_| "requires a non-negative integer".to_owned())
}

/// Parses the TS CLI's integer byte sizes and binary K/M/G/T suffixes.
///
/// # Errors
///
/// Returns an error for malformed or overflowing values.
pub fn parse_byte_size(value: &str) -> Result<u64, String> {
    let normalized = value.trim();
    let split = normalized
        .char_indices()
        .find(|(_, character)| !character.is_ascii_digit())
        .map_or(normalized.len(), |(index, _)| index);
    let (amount, suffix) = normalized.split_at(split);
    let amount = u64::from_str(amount)
        .map_err(|_| "requires bytes or an integer K/M/G/T size".to_owned())?;
    let multiplier = match suffix.to_ascii_uppercase().as_str() {
        "" => 1,
        "K" => 1024,
        "M" => 1024_u64.pow(2),
        "G" => 1024_u64.pow(3),
        "T" => 1024_u64.pow(4),
        _ => return Err("requires bytes or an integer K/M/G/T size".to_owned()),
    };
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| "size is too large".to_owned())
}

/// Parses epoch milliseconds, local `YYYY-MM-DD`, or RFC 3339 timestamps.
///
/// # Errors
///
/// Returns an error when the value is not a supported time representation.
pub fn parse_modified_time(value: &str) -> Result<u64, String> {
    if value.chars().all(|character| character.is_ascii_digit()) {
        return value
            .parse()
            .map_err(|_| "requires epoch milliseconds or a parseable date".to_owned());
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        let local = date
            .and_hms_opt(0, 0, 0)
            .and_then(|time| Local.from_local_datetime(&time).single())
            .ok_or_else(|| "requires epoch milliseconds or a parseable date".to_owned())?;
        return u64::try_from(local.timestamp_millis())
            .map_err(|_| "requires epoch milliseconds or a parseable date".to_owned());
    }
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|_| "requires epoch milliseconds or a parseable date".to_owned())?;
    u64::try_from(parsed.timestamp_millis())
        .map_err(|_| "requires epoch milliseconds or a parseable date".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{Cli, CliPlan, IndexOperation, parse_byte_size, parse_modified_time};
    use std::path::PathBuf;

    #[test]
    fn refresh_policy_preserves_mode_defaults_and_explicit_wait() {
        use zg_engine::api::context::options::RefreshPolicy;
        let current_dir = std::env::current_dir().expect("current directory should resolve");
        for (mode, flag, expected) in [
            ("server", None, RefreshPolicy::Background),
            ("auto", None, RefreshPolicy::Background),
            ("direct", None, RefreshPolicy::Off),
            ("direct", Some("background"), RefreshPolicy::Off),
            ("direct", Some("wait"), RefreshPolicy::Wait),
            ("server", Some("wait"), RefreshPolicy::Wait),
            ("server", Some("off"), RefreshPolicy::Off),
        ] {
            let mut args = vec!["zg", "query", "example", "--mode", mode];
            if let Some(flag) = flag {
                args.extend(["--refresh", flag]);
            }
            let CliPlan::Query { request, .. } = Cli::try_parse_from(args)
                .expect("parse")
                .into_plan(current_dir.clone())
                .expect("plan")
            else {
                panic!("query")
            };
            assert_eq!(request.refresh, Some(expected));
        }
    }

    #[test]
    fn parses_index_options_into_engine_request() {
        let cli = Cli::try_parse_from([
            "zg",
            "index",
            "repo",
            "--mode",
            "server",
            "--embedding",
            "local/potion-code-16m-v2",
            "--device",
            "CPU",
            "--model-cache",
            "/models",
            "--embedding-concurrency",
            "4",
            "-g",
            "src/**",
            "--max-filesize",
            "2M",
        ])
        .expect("CLI should parse");
        let CliPlan::Index {
            operation: IndexOperation::Build(request),
            ..
        } = cli.into_plan(PathBuf::from("/workspace")).expect("plan")
        else {
            panic!("index build")
        };
        assert_eq!(request.root, Some(PathBuf::from("/workspace/repo")));
        assert_eq!(request.embedding_concurrency, Some(4));
        assert_eq!(request.discovery.globs, ["src/**"]);
        assert_eq!(request.discovery.max_file_size_bytes, Some(2 * 1024 * 1024));
    }

    #[test]
    fn parses_indexed_query_routes_and_filters() {
        let cli = Cli::try_parse_from([
            "zg",
            "query",
            "--human",
            "--trace",
            "--limit",
            "7",
            "--hybrid",
            "zero",
            "--fts",
            "one",
            "--vector",
            "two",
            "--symbol-type",
            "class",
            "query text",
        ])
        .expect("CLI should parse");
        let CliPlan::Query {
            request, output, ..
        } = cli
            .into_plan(PathBuf::from("/workspace"))
            .expect("query plan")
        else {
            panic!("query")
        };
        assert_eq!(request.queries, ["query text", "zero"]);
        assert_eq!(request.routes.len(), 2);
        assert_eq!(request.limit, Some(7));
        assert!(output.human);
        assert!(output.trace);
    }

    #[test]
    fn query_options_can_follow_positionals_like_typescript() {
        let cli = Cli::try_parse_from(["zg", "query", "query text", "--limit", "3", "--fts=exact"])
            .expect("option-anywhere syntax should parse");
        let CliPlan::Query { request, .. } = cli
            .into_plan(PathBuf::from("/workspace"))
            .expect("query plan")
        else {
            panic!("query")
        };
        assert_eq!(request.queries, ["query text"]);
        assert_eq!(request.limit, Some(3));
        assert_eq!(request.routes.len(), 1);

        let removed = Cli::try_parse_from(["zg", "query", "--json", "query"])
            .expect("shape validation follows syntax parsing")
            .into_plan(PathBuf::from("/workspace"));
        assert!(removed.is_err());
    }

    #[test]
    fn parses_managed_rg_short_groups() {
        let cli = Cli::try_parse_from([
            "zg",
            "query",
            "--rg",
            "-nHFiwPSsuvxUzL",
            "-einline",
            "-g*.js",
            "-tts",
            "-T",
            "json",
            "-A2",
            "-B",
            "3",
            "-C4",
            "needle",
        ])
        .expect("CLI should collect rg args");
        let CliPlan::Query { request, .. } =
            cli.into_plan(PathBuf::from("/workspace")).expect("rg plan")
        else {
            panic!("query")
        };
        assert!(request.rg);
        assert_eq!(request.queries, ["inline"]);
        assert_eq!(request.rg_options.before_context, 4);
        assert_eq!(request.rg_options.after_context, 4);
        assert!(request.follow);
    }

    #[test]
    fn typescript_size_and_time_forms() {
        assert_eq!(parse_byte_size("2M").expect("size"), 2 * 1024 * 1024);
        assert_eq!(
            parse_modified_time("1700000000000").expect("epoch"),
            1_700_000_000_000
        );
        assert!(parse_modified_time("2026-13-40").is_err());
    }
}
