use std::{
    error::Error,
    io::{self, IsTerminal},
    path::Path,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(target_os = "macos")]
use std::{ffi::OsString, os::unix::process::CommandExt, process::Command};

use tokio::runtime::Builder;
use tracing::debug;
use tracing_subscriber::EnvFilter;
use zg_cli::{
    Cli, CliPlan, ClientMode, IndexOperation, InstallOutcome, McpInstallTransport, McpToolset,
    ServerPlan, ServerStartArgs,
};
use zg_daemon::{DaemonStatus, ListenAddress, McpToolset as DaemonMcpToolset, ServerConfig};
use zg_daemon_protocol::{DaemonCommand, DaemonReply};
use zg_engine::{EngineError, ZvecGrep, api::context::ContextOptions};

static DAEMON_LOGGING: AtomicBool = AtomicBool::new(false);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let Some(error) = error.downcast_ref::<EngineError>() {
                eprintln!("{}", error.report());
            } else {
                eprintln!("Error: {error}");
            }
            if DAEMON_LOGGING.load(Ordering::Relaxed) {
                tracing::error!(%error, "daemon failed");
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let warning = zg_cli::compatibility_warning_for_args(&arguments[1..]);
    let warn = || {
        if let Some(warning) = &warning {
            eprintln!("{warning}");
        }
    };
    let cli = Cli::try_parse_from(arguments).unwrap_or_else(|error| {
        warn();
        error.exit();
    });
    let plan = cli
        .into_plan(std::env::current_dir()?)
        .inspect_err(|_| warn())?;
    match &plan {
        CliPlan::Help(topic) => {
            warn();
            zg_cli::print_help(topic.as_deref())?;
            return Ok(());
        }
        CliPlan::Version => {
            warn();
            println!("{}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }
    install_darwin_metal_residency_mitigation()?;
    // Emit after a possible re-exec so macOS prints each warning only once.
    warn();
    let debug = match &plan {
        CliPlan::Query { output, .. }
        | CliPlan::Index { output, .. }
        | CliPlan::Status { output, .. } => output.debug,
        _ => false,
    };
    if !matches!(plan, CliPlan::Server(ServerPlan::Run(_))) {
        init_tracing(debug);
    }

    let runtime = Builder::new_multi_thread().enable_all().build()?;

    runtime.block_on(async move { execute_plan(plan).await })
}

#[cfg(target_os = "macos")]
fn install_darwin_metal_residency_mitigation() -> io::Result<()> {
    if std::env::var_os("GGML_METAL_NO_RESIDENCY").is_some()
        || std::env::var_os("ZVEC_GREP_METAL_KEEP_RESIDENCY").as_deref()
            == Some(std::ffi::OsStr::new("1"))
    {
        return Ok(());
    }

    // Changing the process environment after Tokio starts is not thread-safe. Re-exec
    // before building the runtime so llama.cpp observes the same Metal default as main.
    let executable = std::env::current_exe()?;
    let arguments = std::env::args_os().skip(1).collect::<Vec<OsString>>();
    let error = Command::new(executable)
        .args(arguments)
        .env("GGML_METAL_NO_RESIDENCY", "1")
        .exec();
    Err(error)
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)]
fn install_darwin_metal_residency_mitigation() -> io::Result<()> {
    Ok(())
}

async fn execute_plan(plan: CliPlan) -> Result<(), Box<dyn Error>> {
    match plan {
        CliPlan::Query {
            mode,
            home,
            request,
            output,
        } => execute_request(mode, home.as_deref(), *request, output).await,
        CliPlan::Index {
            mode,
            home,
            operation,
            output,
        } => execute_index(mode, home.as_deref(), operation, output).await,
        CliPlan::Status {
            mode,
            home,
            request,
            check_ready,
            output,
        } => execute_status(mode, home.as_deref(), request, check_ready, output).await,
        CliPlan::Config(args) => {
            use zg_cli::{ConfigAction, ModelAction, ProviderAction};
            let (label, reference, path) = match args.action {
                ConfigAction::Provider {
                    action: ProviderAction::Set { reference, api_key },
                } => {
                    let path = zg_engine::config::set_provider(&reference, &api_key)?;
                    ("Provider", reference, path)
                }
                ConfigAction::Model {
                    action:
                        ModelAction::Set {
                            reference,
                            endpoint,
                            device,
                            default_model,
                        },
                } => {
                    let path = zg_engine::config::set_model(
                        &reference,
                        endpoint.as_deref(),
                        device.map(Into::into),
                        default_model,
                    )?;
                    ("Model", reference, path)
                }
            };
            println!("{label} config: {reference}");
            println!("Global config: {}", path.display());
            Ok(())
        }
        CliPlan::Auth(args) => {
            let root = args
                .root
                .as_deref()
                .ok_or_else(|| io::Error::other("auth root is required"))?;
            let status = match args.action {
                zg_cli::AuthAction::Grant { .. } => zg_engine::authorization::grant(
                    root,
                    args.embedding.as_deref(),
                    args.endpoint.as_deref(),
                )?,
                zg_cli::AuthAction::Status => zg_engine::authorization::status(root)?,
                zg_cli::AuthAction::Revoke => zg_engine::authorization::revoke(root)?,
            };
            println!("{status}");
            Ok(())
        }
        CliPlan::Server(plan) => execute_server_plan(plan).await,
        CliPlan::Install(args) => execute_install_plan(&args).await,
        CliPlan::Uninstall(args) => zg_cli::execute_uninstall(&args).map_err(Into::into),
        CliPlan::Help(_) | CliPlan::Version => Ok(()),
    }
}

async fn execute_install_plan(args: &zg_cli::InstallArgs) -> Result<(), Box<dyn Error>> {
    let outcome = zg_cli::execute_install(args)?;
    if outcome.agent_labels.is_empty() {
        return Ok(());
    }

    let status = if std::env::var("ZVEC_GREP_INSTALL_SKIP_SERVER").as_deref() == Ok("1") {
        None
    } else {
        Some(start_installed_server(&outcome).await?)
    };
    if let Some(status) = &status {
        if status.ready {
            println!("  ✓ Server");
            println!(
                "    ready at {}",
                status
                    .server_url
                    .as_deref()
                    .unwrap_or("http://127.0.0.1:7999/mcp")
            );
        } else {
            println!("  ○ Server");
            println!("    not started; run `zg --server on`");
        }
    } else {
        println!("  ○ Server");
        println!("    not started; run `zg --server on`");
    }
    if outcome.transport == McpInstallTransport::Stdio {
        println!("  ✓ Connection");
        println!("    stdio; reconnects start the server automatically");
    }
    println!("\nzvec-grep is ready\n");
    println!("  Agents       {}", outcome.agent_labels.join(", "));
    println!("  Remote data  Authorization requested on first remote use");
    println!("\nRestart the selected agents or start a new session to load the integration.");
    Ok(())
}

async fn start_installed_server(outcome: &InstallOutcome) -> Result<DaemonStatus, Box<dyn Error>> {
    let listen = zg_cli::resolve_server_listen()?.parse::<ListenAddress>()?;
    let home = zg_daemon::resolve_home(None)?;
    let mut config = ServerConfig::new(listen, home);
    config.mcp_toolset = match outcome.mcp_toolset {
        Some(McpToolset::Agent) => Some(DaemonMcpToolset::Agent),
        Some(McpToolset::Full) => Some(DaemonMcpToolset::Full),
        None => std::env::var_os("ZVEC_GREP_MCP_TOOLSET")
            .map(|environment| match environment.to_string_lossy().as_ref() {
                "agent" => Ok(DaemonMcpToolset::Agent),
                "full" => Ok(DaemonMcpToolset::Full),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ZVEC_GREP_MCP_TOOLSET must be agent or full",
                )),
            })
            .transpose()?,
    };
    let executable = std::env::current_exe()?;
    Ok(zg_daemon::start_server(&executable, &config).await?)
}

async fn execute_request(
    mode: ClientMode,
    home: Option<&Path>,
    mut request: ContextOptions,
    output: zg_cli::OutputOptions,
) -> Result<(), Box<dyn Error>> {
    if request.rg {
        if mode == ClientMode::Server {
            debug!("managed --rg remains local in server mode");
        }
        return execute_direct_context(request, output).await;
    }
    let server = use_server(mode, home).await?;
    zg_cli::finalize_refresh(&mut request, server);
    authorize_query(&mut request, server, home).await?;
    match execute_context(request.clone(), server, home, output).await {
        Err(error) if is_not_found(error.as_ref()) => {
            // Probing first would block searches that can borrow an active writer.
            if !ensure_query_index(&request, server, home, output).await? {
                return Err(error);
            }
            authorize_query(&mut request, server, home).await?;
            execute_context(request, server, home, output).await
        }
        result => result,
    }
}

fn is_not_found(error: &(dyn Error + 'static)) -> bool {
    if let Some(error) = error.downcast_ref::<EngineError>() {
        return error.code() == EngineError::NOT_FOUND;
    }
    match error.downcast_ref::<zg_daemon::DaemonError>() {
        Some(zg_daemon::DaemonError::Remote { report, .. }) => {
            report.code == EngineError::NOT_FOUND
        }
        Some(zg_daemon::DaemonError::Engine(error)) => error.code() == EngineError::NOT_FOUND,
        _ => false,
    }
}

async fn execute_context(
    request: ContextOptions,
    server: bool,
    home: Option<&Path>,
    output: zg_cli::OutputOptions,
) -> Result<(), Box<dyn Error>> {
    if server {
        let home = zg_daemon::resolve_home(home.map(Path::to_owned))?;
        let reply = zg_daemon::execute_command(&home, DaemonCommand::Context(request)).await?;
        let DaemonReply::Context(result) = reply else {
            return Err(protocol_mismatch("context"));
        };
        zg_cli::write_context_with_options(
            io::stdout().lock(),
            &result,
            output,
            io::stdout().is_terminal(),
        )?;
        if output.debug {
            eprintln!(
                "Diagnostics: {}",
                serde_json::to_string(&result.diagnostics)?
            );
        }
        return Ok(());
    }
    execute_direct_context(request, output).await
}

async fn ensure_query_index(
    request: &ContextOptions,
    server: bool,
    home: Option<&Path>,
    output: zg_cli::OutputOptions,
) -> Result<bool, Box<dyn Error>> {
    use zg_engine::api::{
        index::{IndexOptions, options::EmbeddingModelSpec},
        info::{InfoOptions, result::WorkspaceIndexPolicy},
    };

    let server_home = if server {
        Some(zg_daemon::resolve_home(home.map(Path::to_owned))?)
    } else {
        None
    };
    let info_request = InfoOptions {
        root: request.root.clone(),
        ..InfoOptions::default()
    };
    let info = if let Some(home) = &server_home {
        let reply = zg_daemon::execute_command(home, DaemonCommand::Info(info_request)).await?;
        let DaemonReply::Info(info) = reply else {
            return Err(protocol_mismatch("info"));
        };
        *info
    } else {
        let engine = ZvecGrep::new();
        let info = engine.info(info_request).await;
        engine.close();
        info?
    };
    if info.indexed || info.index_policy == WorkspaceIndexPolicy::Disabled {
        return Ok(false);
    }

    let embedding = zg_engine::config::implicit_embedding_reference()?;
    eprintln!("No index found; creating one with {embedding}.");
    // Implicit builds must not inherit remote credentials or authorization.
    let mut index_request = IndexOptions {
        root: Some(info.root),
        embedding: Some(EmbeddingModelSpec {
            reference: embedding,
            revision: None,
            cache_dir: None,
            endpoint: None,
            device: zg_engine::api::index::options::Device::Auto,
        }),
        device: request.device,
        model_cache: request.model_cache.clone(),
        embedding_concurrency: request.embedding_concurrency,
        lock_timeout_ms: request.lock_timeout_ms,
        ..IndexOptions::default()
    };
    let progress =
        zg_cli::IndexProgressDisplay::new(io::stderr(), io::stderr().is_terminal(), output.color);
    let reporter = progress.reporter();
    let result: Result<_, Box<dyn Error>> = if let Some(home) = &server_home {
        zg_daemon::index_with_progress(home, index_request, &reporter)
            .await
            .map_err(Into::into)
    } else {
        let engine = ZvecGrep::new();
        index_request.on_progress = Some(reporter.prioritize_model_progress());
        let result = engine.index(index_request).await;
        engine.close();
        result.map_err(Into::into)
    };
    progress.finish();
    result?;
    Ok(true)
}

async fn authorize_query(
    request: &mut ContextOptions,
    server: bool,
    home: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    // Redirected input must never block waiting for interactive consent.
    if request.allow_remote || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(());
    }
    authorize_query_with_io(
        request,
        server,
        home,
        io::stdin().lock(),
        io::stderr().lock(),
    )
    .await
}

async fn authorize_query_with_io(
    request: &mut ContextOptions,
    server: bool,
    home: Option<&Path>,
    mut input: impl io::BufRead,
    mut output: impl io::Write,
) -> Result<(), Box<dyn Error>> {
    use zg_cli::QueryAuthorizationDecision;
    let server_home = if server {
        Some(zg_daemon::resolve_home(home.map(Path::to_owned))?)
    } else {
        None
    };
    let targets = query_authorization_targets(request, server_home.as_deref()).await?;
    if targets.is_empty() {
        return Ok(());
    }
    let mut decisions = Vec::new();
    for authorization in &targets {
        let decision = zg_cli::prompt_query_authorization(
            &authorization.target,
            authorization.query_text,
            authorization.workspace_content,
            &mut input,
            &mut output,
        )?;
        match decision {
            QueryAuthorizationDecision::Cancel => {
                return Err(io::Error::other(
                    "Remote Embedding authorization was declined. No remote data was sent.",
                )
                .into());
            }
            QueryAuthorizationDecision::FtsOnly => {
                zg_cli::use_fts_only(request);
                return Ok(());
            }
            QueryAuthorizationDecision::Once | QueryAuthorizationDecision::Workspace => {
                decisions.push(decision);
            }
        }
    }
    if query_authorization_targets(request, server_home.as_deref()).await? != targets {
        return Err(io::Error::other("Workspace authorization scope changed while awaiting consent; retry to review all destinations").into());
    }
    for (authorization, decision) in targets.iter().zip(decisions) {
        match decision {
            QueryAuthorizationDecision::Once => {
                request.authorized_remote.push(authorization.target.clone());
            }
            QueryAuthorizationDecision::Workspace => {
                grant_authorization(&authorization.target, server_home.as_deref()).await?;
            }
            QueryAuthorizationDecision::Cancel | QueryAuthorizationDecision::FtsOnly => {
                unreachable!()
            }
        }
    }
    // A single remote query destination can retain the legacy binding. Multiple
    // models keep their individual saved endpoints instead of a global override.
    if targets.len() == 1 {
        request.authorization_model = Some(targets[0].target.model.clone());
    }
    Ok(())
}

async fn query_authorization_targets(
    request: &ContextOptions,
    server_home: Option<&Path>,
) -> Result<Vec<zg_engine::authorization::QueryAuthorization>, Box<dyn Error>> {
    if let Some(home) = server_home {
        let reply =
            zg_daemon::execute_command(home, DaemonCommand::QueryAuthorization(request.clone()))
                .await?;
        let DaemonReply::QueryAuthorization(targets) = reply else {
            return Err(protocol_mismatch("query_authorization"));
        };
        Ok(targets)
    } else {
        Ok(zg_engine::authorization::query_authorizations(request)?)
    }
}

async fn grant_authorization(
    target: &zg_engine::authorization::IndexAuthorization,
    server_home: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    if let Some(home) = server_home {
        let reply = zg_daemon::execute_command(
            home,
            DaemonCommand::GrantIndexAuthorization(target.clone()),
        )
        .await?;
        if !matches!(reply, DaemonReply::GrantIndexAuthorization) {
            return Err(protocol_mismatch("grant_index_authorization"));
        }
    } else {
        zg_engine::authorization::grant_index(target)?;
    }
    Ok(())
}

async fn execute_direct_context(
    mut request: ContextOptions,
    output: zg_cli::OutputOptions,
) -> Result<(), Box<dyn Error>> {
    // A short-lived direct process cannot retain a background refresh job.
    zg_cli::finalize_refresh(&mut request, false);
    let engine = ZvecGrep::new();
    let result = engine.context(request).await?;
    engine.close();
    zg_cli::write_context_with_options(
        io::stdout().lock(),
        &result,
        output,
        io::stdout().is_terminal(),
    )?;
    if output.debug {
        eprintln!(
            "Diagnostics: {}",
            serde_json::to_string(&result.diagnostics)?
        );
    }
    Ok(())
}

async fn execute_index(
    mode: ClientMode,
    home: Option<&Path>,
    operation: IndexOperation,
    output: zg_cli::OutputOptions,
) -> Result<(), Box<dyn Error>> {
    let root = match &operation {
        IndexOperation::Build(request) => request.root.clone(),
        IndexOperation::Drop(request) => request.root.clone(),
    }
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "index root is required"))?;
    let server = use_server(mode, home).await?;
    match operation {
        IndexOperation::Build(mut request) => {
            authorize_index(&mut request, server, home).await?;
            let progress = zg_cli::IndexProgressDisplay::new(
                io::stderr(),
                io::stderr().is_terminal(),
                output.color,
            );
            let reporter = progress.reporter();
            let result = if server {
                let home = zg_daemon::resolve_home(home.map(Path::to_owned))?;
                zg_daemon::index_with_progress(&home, *request, &reporter).await?
            } else {
                let engine = ZvecGrep::new();
                request.on_progress = Some(reporter.prioritize_model_progress());
                let result = engine.index(*request).await?;
                engine.close();
                result
            };
            progress.finish();
            let color = output.color == zg_cli::ColorMode::Always
                || (output.color == zg_cli::ColorMode::Auto
                    && io::stdout().is_terminal()
                    && std::env::var_os("NO_COLOR").is_none());
            if color {
                print!("\x1b[36m");
            }
            zg_cli::write_index_result(io::stdout().lock(), &root, &result)?;
            if color {
                print!("\x1b[0m");
            }
            if output.debug {
                eprintln!("Index diagnostics: {}", serde_json::to_string(&result)?);
            }
            if result.files_failed > 0 {
                return Err(io::Error::other(format!(
                    "indexing completed with {} failed {}; workspace index is not ready",
                    result.files_failed,
                    if result.files_failed == 1 {
                        "file"
                    } else {
                        "files"
                    }
                ))
                .into());
            }
        }
        IndexOperation::Drop(request) => {
            let removed = if server {
                let home = zg_daemon::resolve_home(home.map(Path::to_owned))?;
                let reply =
                    zg_daemon::execute_command(&home, DaemonCommand::DropIndex(request)).await?;
                let DaemonReply::DropIndex(removed) = reply else {
                    return Err(protocol_mismatch("drop_index"));
                };
                removed
            } else {
                let engine = ZvecGrep::new();
                let removed = engine.drop_index(request).await?;
                engine.close();
                removed
            };
            println!(
                "Workspace index: {}",
                if removed { "dropped" } else { "missing" }
            );
            println!("Root: {}", root.display());
        }
    }
    Ok(())
}

async fn authorize_index(
    request: &mut zg_engine::api::index::IndexOptions,
    server: bool,
    home: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    if request.allow_remote || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(());
    }
    let server_home = if server {
        Some(zg_daemon::resolve_home(home.map(Path::to_owned))?)
    } else {
        None
    };
    let targets = index_authorization_targets(request, server_home.as_deref()).await?;
    if targets.is_empty() {
        return Ok(());
    }
    let mut decisions = Vec::new();
    for target in &targets {
        let decision =
            zg_cli::prompt_index_authorization(target, io::stdin().lock(), io::stderr().lock())?;
        if decision == zg_cli::AuthorizationDecision::Cancel {
            return Err(io::Error::other(
                "Remote Embedding authorization was declined. No remote data was sent.",
            )
            .into());
        }
        decisions.push(decision);
    }
    if index_authorization_targets(request, server_home.as_deref()).await? != targets {
        return Err(io::Error::other("Workspace authorization scope changed while awaiting consent; retry to review all destinations").into());
    }
    for (target, decision) in targets.iter().zip(decisions) {
        match decision {
            zg_cli::AuthorizationDecision::Once => request.authorized_remote.push(target.clone()),
            zg_cli::AuthorizationDecision::Workspace => {
                grant_authorization(target, server_home.as_deref()).await?;
            }
            zg_cli::AuthorizationDecision::Cancel => unreachable!(),
        }
    }
    request.root = Some(targets[0].root.clone());
    Ok(())
}

async fn index_authorization_targets(
    request: &zg_engine::api::index::IndexOptions,
    server_home: Option<&Path>,
) -> Result<Vec<zg_engine::authorization::IndexAuthorization>, Box<dyn Error>> {
    if let Some(home) = server_home {
        let reply =
            zg_daemon::execute_command(home, DaemonCommand::IndexAuthorization(request.clone()))
                .await?;
        let DaemonReply::IndexAuthorization(targets) = reply else {
            return Err(protocol_mismatch("index_authorization"));
        };
        Ok(targets)
    } else {
        Ok(zg_engine::authorization::index_authorizations(request)?)
    }
}

async fn execute_status(
    mode: ClientMode,
    home: Option<&Path>,
    request: zg_engine::api::info::InfoOptions,
    check_ready: bool,
    output: zg_cli::OutputOptions,
) -> Result<(), Box<dyn Error>> {
    let result = if use_server(mode, home).await? {
        let home = zg_daemon::resolve_home(home.map(Path::to_owned))?;
        let reply = zg_daemon::execute_command(&home, DaemonCommand::Info(request)).await?;
        let DaemonReply::Info(result) = reply else {
            return Err(protocol_mismatch("info"));
        };
        *result
    } else {
        let engine = ZvecGrep::new();
        let result = engine.info(request).await?;
        engine.close();
        result
    };
    zg_cli::write_info_with_options(
        io::stdout().lock(),
        &result,
        output,
        io::stdout().is_terminal(),
    )?;
    if output.debug {
        eprintln!("Status diagnostics: indexed={}", result.indexed);
    }
    let ready = result.index_status() == zg_engine::api::info::result::IndexStatus::Ready;
    if check_ready && !ready {
        return Err(io::Error::other("workspace index is not ready").into());
    }
    Ok(())
}

async fn use_server(mode: ClientMode, home: Option<&Path>) -> Result<bool, Box<dyn Error>> {
    match mode {
        ClientMode::Direct => Ok(false),
        ClientMode::Server => Ok(true),
        ClientMode::Auto => {
            let home = zg_daemon::resolve_home(home.map(Path::to_owned))?;
            Ok(zg_daemon::server_status(&home).await?.ready)
        }
    }
}

fn protocol_mismatch(expected: &str) -> Box<dyn Error> {
    io::Error::other(format!("daemon returned a reply other than {expected}")).into()
}

async fn execute_server_plan(plan: ServerPlan) -> Result<(), Box<dyn Error>> {
    match plan {
        ServerPlan::Stdio(args) => {
            let config = server_config(args)?;
            let executable = std::env::current_exe()?;
            zg_daemon::run_stdio_bridge(&executable, &config).await?;
        }
        ServerPlan::On(args) => {
            let config = server_config(args)?;
            let executable = std::env::current_exe()?;
            let status = zg_daemon::start_server(&executable, &config).await?;
            write_server_status(&status);
        }
        ServerPlan::Off(args) => {
            let home = zg_daemon::resolve_home(args.home)?;
            let status = zg_daemon::stop_server_with_token(
                &home,
                zg_daemon::default_stop_timeout(),
                args.token_file.as_deref(),
            )
            .await?;
            write_server_status(&status);
        }
        ServerPlan::Status(args) => {
            let home = zg_daemon::resolve_home(args.home)?;
            let status = zg_daemon::server_status(&home).await?;
            write_server_status(&status);
            if args.check_ready && !status.ready {
                return Err(io::Error::other("server is not ready").into());
            }
        }
        ServerPlan::Run(args) => {
            let config = server_config(args)?;
            zg_daemon::run_server_with_logging(config, Arc::new(ZvecGrep::new()), |home| {
                let options = zg_engine::config::daemon_log_options()?;
                let log = zg_daemon::rolling_log::RollingLog::open(home, options)?;
                init_daemon_tracing(log, options.debug);
                DAEMON_LOGGING.store(true, Ordering::Relaxed);
                Ok(())
            })
            .await?;
        }
    }
    Ok(())
}

fn server_config(args: ServerStartArgs) -> Result<ServerConfig, Box<dyn Error>> {
    zg_daemon::resolve_token(args.token_file.as_deref())?;
    let listen = args.listen.parse::<ListenAddress>()?;
    let home = zg_daemon::resolve_home(args.home)?;
    let mut config = ServerConfig::new(listen, home);
    config.token_file = args.token_file;
    config.mcp_toolset = args.mcp_toolset.map(|toolset| match toolset {
        McpToolset::Agent => DaemonMcpToolset::Agent,
        McpToolset::Full => DaemonMcpToolset::Full,
    });
    Ok(config)
}

fn write_server_status(status: &DaemonStatus) {
    let label = if status.ready {
        "ready"
    } else if status.running {
        "starting"
    } else {
        "stopped"
    };
    println!("Server: {label}");
    if let Some(pid) = status.pid {
        println!("PID: {pid}");
    }
    if let Some(url) = &status.server_url {
        println!("URL: {url}");
    }
    if let Some(toolset) = &status.mcp_toolset {
        println!("MCP toolset: {toolset}");
    }
}

fn init_tracing(debug: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(if debug {
            "zg=debug,zg_engine=debug,zg_daemon=debug"
        } else {
            "warn"
        })
    });
    let _ = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(filter)
        .try_init();
}

fn init_daemon_tracing(log: zg_daemon::rolling_log::RollingLog, debug: bool) {
    let filter = EnvFilter::new(if debug {
        "zg=debug,zg_engine=debug,zg_daemon=debug,zg_transport_mcp=debug"
    } else {
        "zg=info,zg_engine=info,zg_daemon=info,zg_transport_mcp=info"
    });
    let _ = tracing_subscriber::fmt()
        .json()
        .with_ansi(false)
        .with_writer(move || log.writer())
        .with_env_filter(filter)
        .try_init();
}

#[cfg(test)]
mod query_authorization_tests {
    use super::*;

    #[tokio::test]
    async fn query_choices_apply_to_execution_and_workspace_consent_is_reused() {
        const FIXTURE_ENV: &str = "ZG_QUERY_AUTHORIZATION_TEST_ROOT";
        let Some(root) = std::env::var_os(FIXTURE_ENV) else {
            // Isolate signing and global configuration without mutating process environment.
            let workspace = tempfile::tempdir().expect("workspace");
            let state = tempfile::tempdir().expect("state");
            let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args(["--exact", "query_authorization_tests::query_choices_apply_to_execution_and_workspace_consent_is_reused", "--nocapture"])
                .env(FIXTURE_ENV, workspace.path())
                .env("HOME", state.path()).env("USERPROFILE", state.path())
                .env("ZVEC_GREP_AUTHORIZATION_KEY_FILE", state.path().join("key"))
                .env_remove("ZVEC_GREP_API_KEY").env_remove("ZVEC_GREP_ENDPOINT")
                .env_remove("ZVEC_GREP_EMBEDDING").env_remove("DASHSCOPE_API_KEY").env_remove("QWEN_API_KEY")
                .output().expect("isolated test");
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        };
        let root = std::path::PathBuf::from(root);
        let engine = ZvecGrep::new();
        engine
            .index(zg_engine::api::index::IndexOptions {
                root: Some(root.clone()),
                allow_remote: true,
                api_key: Some("test-key".into()),
                embedding: Some(zg_engine::api::index::options::EmbeddingModelSpec {
                    reference: "qwen/text-embedding-v4".into(),
                    revision: None,
                    cache_dir: None,
                    endpoint: None,
                    device: zg_engine::api::index::options::Device::Auto,
                }),
                endpoint: Some("https://query.test/embeddings".into()),
                ..zg_engine::api::index::IndexOptions::default()
            })
            .await
            .expect("empty remote index without network");
        let make = || ContextOptions {
            root: Some(root.clone()),
            query: Some("bookstore".into()),
            refresh: Some(zg_engine::api::context::options::RefreshPolicy::Off),
            ..ContextOptions::default()
        };
        let mut once = make();
        let mut prompt = Vec::new();
        authorize_query_with_io(&mut once, false, None, "1\n".as_bytes(), &mut prompt)
            .await
            .expect("once");
        assert!(
            !once.allow_remote,
            "interactive consent must be destination scoped"
        );
        assert_eq!(once.authorized_remote.len(), 1);
        assert_eq!(once.authorized_remote[0].model, "qwen/text-embedding-v4");
        assert_eq!(
            once.authorization_model.as_deref(),
            Some("qwen/text-embedding-v4")
        );
        assert!(!root.join(".zvec-grep/authorization.json").exists());
        assert!(
            String::from_utf8(prompt)
                .expect("prompt")
                .contains("Send query text?")
        );
        let mut cancelled = make();
        assert!(
            authorize_query_with_io(&mut cancelled, false, None, "4\n".as_bytes(), Vec::new())
                .await
                .is_err()
        );
        assert!(!cancelled.allow_remote);
        assert!(cancelled.authorized_remote.is_empty());
        let mut fts = make();
        authorize_query_with_io(&mut fts, false, None, "3\n".as_bytes(), Vec::new())
            .await
            .expect("FTS");
        engine
            .context(fts)
            .await
            .expect("FTS executes without remote credentials");
        assert!(!root.join(".zvec-grep/authorization.json").exists());
        let mut workspace = make();
        authorize_query_with_io(&mut workspace, false, None, "2\n".as_bytes(), Vec::new())
            .await
            .expect("workspace");
        assert!(!workspace.allow_remote);
        assert!(root.join(".zvec-grep/authorization.json").exists());
        let mut repeated = make();
        let mut prompt = Vec::new();
        authorize_query_with_io(&mut repeated, false, None, "".as_bytes(), &mut prompt)
            .await
            .expect("existing grant");
        assert!(prompt.is_empty());
        engine.close();
    }
}
