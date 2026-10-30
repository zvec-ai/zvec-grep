use std::{
    error::Error,
    io::{self, IsTerminal, Write},
    path::Path,
    process::ExitCode,
    sync::{
        Arc, OnceLock,
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
static OUTPUT_OPTIONS: OnceLock<zg_cli::OutputOptions> = OnceLock::new();

fn output_options() -> zg_cli::OutputOptions {
    OUTPUT_OPTIONS.get().copied().unwrap_or_default()
}

fn report_error(error: &(dyn Error + 'static)) {
    let options = output_options();
    let terminal = zg_cli::stderr_supports_ansi();
    if terminal {
        let report = if let Some(error) = error.downcast_ref::<EngineError>() {
            Some((error.report(), None))
        } else {
            match error.downcast_ref::<zg_daemon::DaemonError>() {
                Some(zg_daemon::DaemonError::Remote { report, retryable }) => {
                    Some(((**report).clone(), Some(*retryable)))
                }
                Some(zg_daemon::DaemonError::Engine(error)) => Some((error.report(), None)),
                _ => None,
            }
        };
        if let Some((report, retryable)) = report {
            let _ = zg_cli::write_error_report(
                io::stderr().lock(),
                &report,
                retryable,
                options,
                terminal,
            );
        } else {
            let _ = zg_cli::write_error_message(
                io::stderr().lock(),
                &error.to_string(),
                options,
                terminal,
            );
        }
    } else if let Some(error) = error.downcast_ref::<EngineError>() {
        eprintln!("{}", error.report());
    } else {
        eprintln!("Error: {error}");
    }
}

fn report_warning(message: &str) {
    if zg_cli::stderr_supports_ansi() {
        let _ = zg_cli::write_message(
            io::stderr().lock(),
            zg_cli::MessageKind::Warning,
            message.trim_start_matches("warning: "),
            &[],
            output_options().color,
            true,
        );
    } else {
        eprintln!("{message}");
    }
}

fn animated_progress(options: zg_cli::OutputOptions) -> bool {
    zg_cli::stderr_supports_ansi() && !options.debug && std::env::var_os("RUST_LOG").is_none()
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) if is_engine_error(error.as_ref(), EngineError::CANCELLED) => {
            if zg_cli::stderr_supports_ansi() {
                let _ = zg_cli::write_message(
                    io::stderr().lock(),
                    zg_cli::MessageKind::Info,
                    "Cancelled",
                    &[],
                    output_options().color,
                    true,
                );
            }
            ExitCode::from(130)
        }
        Err(error) => {
            report_error(error.as_ref());
            if DAEMON_LOGGING.load(Ordering::Relaxed) {
                tracing::error!(%error, "daemon failed");
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn Error>> {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let warning = zg_cli::compatibility_warning_for_args(&arguments[1..]);
    let warn = || {
        if let Some(warning) = &warning {
            report_warning(warning);
        }
    };
    let cli = match Cli::try_parse_from(arguments) {
        Ok(cli) => cli,
        Err(error) => {
            warn();
            if zg_cli::stderr_supports_ansi() && error.use_stderr() {
                // Parse errors may precede valid presentation options; keep this path colorless.
                zg_cli::write_error_message(
                    io::stderr().lock(),
                    &error.to_string(),
                    zg_cli::OutputOptions {
                        color: zg_cli::ColorMode::Never,
                        ..Default::default()
                    },
                    true,
                )?;
                return Ok(ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(2)));
            }
            error.exit();
        }
    };
    let _ = OUTPUT_OPTIONS.set(cli.output_options());
    let positional_warning = cli.positional_query_warning();
    let plan = cli
        .into_plan(std::env::current_dir()?)
        .inspect_err(|_| warn())?;
    match &plan {
        CliPlan::Help(topic) => {
            warn();
            zg_cli::print_help(topic.as_deref())?;
            return Ok(ExitCode::SUCCESS);
        }
        CliPlan::Version => {
            warn();
            writeln!(zg_cli::stdout(), "{}", env!("CARGO_PKG_VERSION"))?;
            return Ok(ExitCode::SUCCESS);
        }
        CliPlan::Install(args) if args.upgrade_installed => {
            zg_cli::execute_upgrade_installed(args)?;
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }
    let lexical_only = matches!(&plan, CliPlan::Files { .. })
        || matches!(&plan, CliPlan::Query { request, .. } if request.rg);
    if !lexical_only {
        install_darwin_metal_residency_mitigation()?;
    }
    // Emit after a possible re-exec so macOS prints each warning only once.
    warn();
    if let Some(warning) = positional_warning {
        report_warning(&warning);
    }
    let debug = match &plan {
        CliPlan::Query { output, .. }
        | CliPlan::Index { output, .. }
        | CliPlan::Status { output, .. } => output.debug,
        _ => false,
    };
    if !matches!(plan, CliPlan::Server(ServerPlan::Run(_))) {
        init_tracing(debug);
    }

    // The grep walker owns its search threads; a Tokio worker pool would duplicate them.
    let runtime = if lexical_only {
        Builder::new_current_thread().enable_all().build()?
    } else {
        Builder::new_multi_thread().enable_all().build()?
    };

    runtime.block_on(Box::pin(execute_plan(plan)))
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

async fn execute_plan(plan: CliPlan) -> Result<ExitCode, Box<dyn Error>> {
    match plan {
        CliPlan::Files { request, null } => return Ok(execute_files(*request, null).await),
        CliPlan::Query {
            mode,
            home,
            request,
            path_lookup,
            output,
        } => {
            Box::pin(execute_request(
                mode,
                home.as_deref(),
                *request,
                path_lookup,
                output,
            ))
            .await
        }
        CliPlan::Index {
            mode,
            home,
            operation,
            output,
        } => return execute_index(mode, home.as_deref(), operation, output).await,
        CliPlan::Status {
            mode,
            home,
            request,
            check_ready,
            output,
        } => execute_status(mode, home.as_deref(), request, check_ready, output).await,
        CliPlan::Config(args) => execute_config(args),
        CliPlan::Auth(args) => {
            let root = args
                .root
                .as_deref()
                .ok_or_else(|| io::Error::other("auth root is required"))?;
            if matches!(args.action, zg_cli::AuthAction::Revoke) {
                let count = zg_engine::authorization::revoke_all(root)?;
                zg_cli::write_authorization_revoke_with_options(
                    zg_cli::stdout(),
                    count,
                    output_options().color,
                    zg_cli::stdout_supports_ansi(),
                )?;
                return Ok(ExitCode::SUCCESS);
            }
            if matches!(args.action, zg_cli::AuthAction::Grant { .. }) {
                zg_engine::authorization::grant(
                    root,
                    args.embedding.as_deref(),
                    args.endpoint.as_deref(),
                )?;
            }
            let status = zg_engine::authorization::status_snapshot(root)?;
            zg_cli::write_authorization_status(
                zg_cli::stdout(),
                &status,
                if args.no_color {
                    zg_cli::ColorMode::Never
                } else {
                    args.color.unwrap_or_default()
                },
                zg_cli::stdout_supports_ansi(),
            )?;
            Ok(())
        }
        CliPlan::Server(plan) => execute_server_plan(plan).await,
        CliPlan::Install(args) => execute_install_plan(&args).await,
        CliPlan::Uninstall(args) => zg_cli::execute_uninstall(&args).map_err(Into::into),
        CliPlan::Help(_) | CliPlan::Version => Ok(()),
    }?;
    Ok(ExitCode::SUCCESS)
}

fn execute_config(args: zg_cli::ConfigArgs) -> Result<(), Box<dyn Error>> {
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
    zg_cli::write_config_result(
        zg_cli::stdout(),
        label,
        &reference,
        &path,
        output_options().color,
        zg_cli::stdout_supports_ansi(),
    )?;
    Ok(())
}

async fn execute_files(options: zg_engine::api::files::FilesOptions, null: bool) -> ExitCode {
    let engine = ZvecGrep::new();
    let result = engine.files(options).await;
    engine.close();
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            report_error(&error);
            return ExitCode::from(2);
        }
    };
    for error in &result.errors {
        let _ = zg_cli::write_error_message(
            io::stderr().lock(),
            error,
            output_options(),
            zg_cli::stderr_supports_ansi(),
        );
    }
    if let Err(error) = zg_cli::write_file_paths(io::stdout().lock(), &result.paths, null)
        && error.kind() != io::ErrorKind::BrokenPipe
    {
        report_error(&error);
        return ExitCode::from(2);
    }
    if !result.errors.is_empty() {
        ExitCode::from(2)
    } else if result.paths.is_empty() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
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
    if zg_cli::stdout_supports_ansi() {
        if let Some(status) = &status {
            write_server_status(status)?;
        } else {
            zg_cli::write_message(
                zg_cli::stdout(),
                zg_cli::MessageKind::Info,
                "Server stopped",
                &["Run: zg --server on"],
                zg_cli::ColorMode::Auto,
                true,
            )?;
        }
        let agents = format!("Agents: {}", outcome.agent_labels.join(", "));
        let mut details = vec![agents.as_str()];
        if outcome.transport == McpInstallTransport::Stdio {
            details.push("stdio · reconnecting starts the server automatically");
        }
        details.push("Remote data access requires authorization on first use");
        details.push("Restart the selected agents or start a new session to load the integration");
        zg_cli::write_message(
            zg_cli::stdout(),
            zg_cli::MessageKind::Success,
            "Integration installed",
            &details,
            zg_cli::ColorMode::Auto,
            true,
        )?;
        return Ok(());
    }
    if let Some(status) = &status {
        if status.ready {
            writeln!(zg_cli::stdout(), "  ✓ Server")?;
            writeln!(
                zg_cli::stdout(),
                "    ready at {}",
                status
                    .server_url
                    .as_deref()
                    .unwrap_or("http://127.0.0.1:7999/mcp")
            )?;
        } else {
            writeln!(zg_cli::stdout(), "  ○ Server")?;
            writeln!(zg_cli::stdout(), "    not started; run `zg --server on`")?;
        }
    } else {
        writeln!(zg_cli::stdout(), "  ○ Server")?;
        writeln!(zg_cli::stdout(), "    not started; run `zg --server on`")?;
    }
    if outcome.transport == McpInstallTransport::Stdio {
        writeln!(zg_cli::stdout(), "  ✓ Connection")?;
        writeln!(
            zg_cli::stdout(),
            "    stdio; reconnects start the server automatically"
        )?;
    }
    writeln!(zg_cli::stdout(), "\nzvec-grep is ready\n")?;
    writeln!(
        zg_cli::stdout(),
        "  Agents       {}",
        outcome.agent_labels.join(", ")
    )?;
    writeln!(
        zg_cli::stdout(),
        "  Remote data  Authorization requested on first remote use"
    )?;
    writeln!(
        zg_cli::stdout(),
        "\nRestart the selected agents or start a new session to load the integration."
    )?;
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
    path_lookup: bool,
    output: zg_cli::OutputOptions,
) -> Result<(), Box<dyn Error>> {
    if path_lookup {
        // Current-file lookup is local in every client mode, before index or model admission.
        report_warning(
            "warning: automatically performing path lookup; use --hybrid to force hybrid search.",
        );
        let engine = ZvecGrep::new();
        let result = engine
            .lookup_path(zg_engine::api::paths::PathsOptions {
                query: request.queries[0].clone(),
                root: request.root.clone(),
                limit: request.limit,
                filter: request.filter.clone(),
                signal: request.signal.clone(),
            })
            .await;
        engine.close();
        zg_cli::write_path_lookup_with_terminals(
            zg_cli::stdout(),
            io::stderr().lock(),
            &result?,
            output,
            zg_cli::stdout_supports_ansi(),
            zg_cli::stderr_supports_ansi(),
        )?;
        return Ok(());
    }
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
        Err(error) if is_engine_error(error.as_ref(), EngineError::NOT_FOUND) => {
            // Probing first would block searches that can borrow an active writer.
            if !ensure_query_index(&mut request, server, home, output).await? {
                return Err(error);
            }
            authorize_query(&mut request, server, home).await?;
            execute_context(request, server, home, output).await
        }
        result => result,
    }
}

fn is_engine_error(error: &(dyn Error + 'static), code: &str) -> bool {
    if let Some(error) = error.downcast_ref::<EngineError>() {
        return error.code() == code;
    }
    match error.downcast_ref::<zg_daemon::DaemonError>() {
        Some(zg_daemon::DaemonError::Remote { report, .. }) => report.code == code,
        Some(zg_daemon::DaemonError::Engine(error)) => error.code() == code,
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
            zg_cli::stdout(),
            &result,
            output,
            zg_cli::stdout_supports_ansi(),
        )?;
        zg_cli::write_context_status_with_options(
            io::stderr().lock(),
            &result,
            output,
            zg_cli::stderr_supports_ansi(),
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
    request: &mut ContextOptions,
    server: bool,
    home: Option<&Path>,
    output: zg_cli::OutputOptions,
) -> Result<bool, Box<dyn Error>> {
    use zg_engine::api::info::{InfoOptions, result::WorkspaceIndexPolicy};

    let server_home = if server {
        Some(zg_daemon::resolve_home(home.map(Path::to_owned))?)
    } else {
        None
    };
    let info_request = InfoOptions {
        root: request.root.clone(),
        ..InfoOptions::default()
    };
    let info = read_workspace_info(server, home, info_request).await?;
    if info.indexed || info.index_policy == WorkspaceIndexPolicy::Disabled {
        return Ok(false);
    }
    info.compatibility.ensure_compatible()?;
    if matches!(
        info.compatibility,
        zg_engine::api::info::result::IndexCompatibility::Compatible { .. }
    ) {
        return Err(EngineError::storage_failure(
            "Index storage is missing; rebuild it with `zg --index --rebuild`",
        )
        .into());
    }
    if !io::stdin().is_terminal() || !zg_cli::stderr_supports_ansi() {
        return Err(EngineError::not_found(format!(
            "No index found in {}. Run `zg --index --embedding <model>` to build it, then retry the query. Run `zg --help models` to list supported models.",
            zg_cli::output_path(&info.root).display()
        )).into());
    }
    let options = zg_engine::api::index::setup::IndexSetupOptions {
        model_cache: request.model_cache.clone(),
        preferred_model: std::env::var("ZVEC_GREP_EMBEDDING")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
        credential_available: request
            .api_key
            .as_ref()
            .is_some_and(|key| !key.trim().is_empty()),
    };
    let engine = ZvecGrep::new();
    let setup = if let Some(home) = &server_home {
        let DaemonReply::IndexSetup(setup) =
            zg_daemon::execute_command(home, DaemonCommand::IndexSetup(options)).await?
        else {
            return Err(protocol_mismatch("index_setup"));
        };
        setup
    } else {
        engine.index_setup(options).await?
    };
    let mut index_request = select_query_index(
        request,
        &info.root,
        &setup,
        server_home.as_deref(),
        output.color,
    )
    .await?;
    zg_cli::write_message(
        io::stderr().lock(),
        zg_cli::MessageKind::Info,
        "Building index",
        &[
            &zg_cli::output_path(&info.root).display().to_string(),
            index_request
                .embedding
                .as_ref()
                .map_or("", |model| model.reference.as_str()),
            "Esc Cancel · Ctrl+C Cancel",
        ],
        output.color,
        zg_cli::stderr_supports_ansi(),
    )?;
    let cancellation = zg_cli::IndexCancellation::new()?;
    index_request.signal = Some(cancellation.signal());
    let ansi = zg_cli::stderr_supports_ansi();
    let progress =
        zg_cli::IndexProgressDisplay::new(io::stderr(), animated_progress(output), output.color);
    let reporter = progress.reporter();
    let result: Result<_, Box<dyn Error>> = if let Some(home) = &server_home {
        zg_daemon::index_with_progress(home, index_request, &reporter)
            .await
            .map_err(Into::into)
    } else {
        index_request.on_progress = Some(reporter.prioritize_model_progress());
        engine.index(index_request).await.map_err(Into::into)
    };
    engine.close();
    progress.finish();
    drop(cancellation);
    let result = result?;
    zg_cli::write_index_with_options(io::stderr().lock(), &info.root, &result, None, output, ansi)?;
    Ok(true)
}

async fn select_query_index(
    request: &mut ContextOptions,
    root: &Path,
    setup: &zg_engine::api::index::setup::IndexSetup,
    server_home: Option<&Path>,
    color: zg_cli::ColorMode,
) -> Result<zg_engine::api::index::IndexOptions, Box<dyn Error>> {
    use zg_engine::api::index::{IndexOptions, options::EmbeddingModelSpec};
    let mut selection = zg_cli::ModelSelection::new(setup);
    loop {
        let Some(model) = selection.select(root, color)? else {
            return Err(EngineError::cancelled("Index creation cancelled").into());
        };
        if !model.available()
            && request
                .api_key
                .as_ref()
                .is_none_or(|key| key.trim().is_empty())
        {
            let Some(api_key) = zg_cli::prompt_setup_credential(&model.reference, color)? else {
                return Err(EngineError::cancelled(
                    "Index creation cancelled; no API Key was saved",
                )
                .into());
            };
            let provider = model.reference.split('/').next().unwrap_or_default();
            zg_engine::config::set_provider(provider, &api_key)?;
            selection.set_provider_credential_available(provider);
            request.api_key = Some(api_key);
        }
        let mut index_request = IndexOptions {
            root: Some(root.to_path_buf()),
            embedding: Some(EmbeddingModelSpec {
                reference: model.reference,
                revision: None,
                cache_dir: None,
                endpoint: None,
                device: zg_engine::api::index::options::Device::Auto,
            }),
            // Query credentials remain temporary during the implicit build.
            runtime_api_key: if model.local {
                None
            } else {
                request.api_key.clone()
            },
            allow_remote: request.allow_remote,
            runtime_device: if model.local { request.device } else { None },
            runtime_model_cache: request.model_cache.clone(),
            embedding_concurrency: request.embedding_concurrency,
            lock_timeout_ms: request.lock_timeout_ms,
            ..IndexOptions::default()
        };
        let targets = index_authorization_targets(&index_request, server_home).await?;
        let mut back = false;
        for target in &targets {
            match zg_cli::prompt_setup_authorization(target, true, color)? {
                zg_cli::SetupAuthorizationDecision::Back => {
                    back = true;
                    break;
                }
                zg_cli::SetupAuthorizationDecision::Cancel => {
                    return Err(EngineError::cancelled(
                        "Index creation cancelled; no remote data was sent",
                    )
                    .into());
                }
                zg_cli::SetupAuthorizationDecision::Once => {
                    index_request.authorized_remote.push(target.clone());
                }
                zg_cli::SetupAuthorizationDecision::Workspace => {
                    grant_authorization(target, server_home).await?;
                }
            }
        }
        if back {
            continue;
        }
        // Revalidate the disclosed destination before any remote operation.
        if !index_authorization_targets(&index_request, server_home)
            .await?
            .is_empty()
        {
            return Err(EngineError::permission_denied(
                "Remote destination changed while awaiting consent; retry index setup",
            )
            .into());
        }
        request.device = index_request.runtime_device;
        request
            .authorized_remote
            .extend(index_request.authorized_remote.clone());
        return Ok(index_request);
    }
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
    let inspect_status = !request.rg
        && request.refresh == Some(zg_engine::api::context::options::RefreshPolicy::Off);
    let mut result = engine.context(request).await?;
    if inspect_status {
        let info = engine
            .info(zg_engine::api::info::InfoOptions {
                root: Some(result.root.clone()),
                include_status: true,
            })
            .await?;
        result.indexing = info
            .status
            .as_ref()
            .and_then(zg_engine::api::context::result::ContextIndexingStatus::from_stats);
    }
    engine.close();
    let mut stdout = io::BufWriter::new(zg_cli::stdout());
    zg_cli::write_context_with_options(
        &mut stdout,
        &result,
        output,
        zg_cli::stdout_supports_ansi(),
    )?;
    stdout.flush()?;
    zg_cli::write_context_status_with_options(
        io::stderr().lock(),
        &result,
        output,
        zg_cli::stderr_supports_ansi(),
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
) -> Result<ExitCode, Box<dyn Error>> {
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
                animated_progress(output),
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
            // Inspect saved metadata so omitted flags and --reset-paths are
            // reflected in the summary without rescanning workspace files.
            let info = read_workspace_info(
                server,
                home,
                zg_engine::api::info::InfoOptions {
                    root: Some(root.clone()),
                    include_status: false,
                },
            )
            .await?;
            progress.finish();
            zg_cli::write_index_with_options(
                zg_cli::stdout(),
                &info.root,
                &result,
                info.workspace_index.as_ref().map(|index| &index.scan),
                output,
                zg_cli::stdout_supports_ansi(),
            )?;
            if output.debug {
                eprintln!("Index diagnostics: {}", serde_json::to_string(&result)?);
            }
            if result.files_failed > 0 {
                if zg_cli::stdout_supports_ansi() && !output.debug {
                    return Ok(ExitCode::FAILURE);
                }
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
            zg_cli::write_index_drop_with_options(
                zg_cli::stdout(),
                &root,
                removed,
                output.color,
                zg_cli::stdout_supports_ansi(),
            )?;
        }
    }
    Ok(ExitCode::SUCCESS)
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
    let result = read_workspace_info(use_server(mode, home).await?, home, request).await?;
    zg_cli::write_info_with_options(
        zg_cli::stdout(),
        &result,
        output,
        zg_cli::stdout_supports_ansi(),
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

async fn read_workspace_info(
    server: bool,
    home: Option<&Path>,
    request: zg_engine::api::info::InfoOptions,
) -> Result<zg_engine::api::info::InfoResult, Box<dyn Error>> {
    if server {
        let home = zg_daemon::resolve_home(home.map(Path::to_owned))?;
        let reply = zg_daemon::execute_command(&home, DaemonCommand::Info(request)).await?;
        let DaemonReply::Info(result) = reply else {
            return Err(protocol_mismatch("info"));
        };
        Ok(*result)
    } else {
        let engine = ZvecGrep::new();
        let result = engine.info(request).await;
        engine.close();
        Ok(result?)
    }
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
            write_server_status(&status)?;
        }
        ServerPlan::Off(args) => {
            let home = zg_daemon::resolve_home(args.home)?;
            let status = zg_daemon::stop_server_with_token(
                &home,
                zg_daemon::default_stop_timeout(),
                args.token_file.as_deref(),
            )
            .await?;
            write_server_status(&status)?;
        }
        ServerPlan::Status(args) => {
            let home = zg_daemon::resolve_home(args.home)?;
            let status = zg_daemon::server_status(&home).await?;
            write_server_status(&status)?;
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
    let address = match args.listen {
        Some(listen) => listen,
        None => zg_engine::config::server_listen()?,
    };
    let listen = address.parse::<ListenAddress>()?;
    let home = zg_daemon::resolve_home(args.home)?;
    let mut config = ServerConfig::new(listen, home);
    config.token_file = args.token_file;
    config.mcp_toolset = args.mcp_toolset.map(|toolset| match toolset {
        McpToolset::Agent => DaemonMcpToolset::Agent,
        McpToolset::Full => DaemonMcpToolset::Full,
    });
    Ok(config)
}

fn write_server_status(status: &DaemonStatus) -> io::Result<()> {
    let mut output = zg_cli::stdout();
    let label = if status.ready {
        "ready"
    } else if status.running {
        "starting"
    } else {
        "stopped"
    };
    if zg_cli::stdout_supports_ansi() {
        let mut details = Vec::new();
        if let Some(url) = &status.server_url {
            details.push(url.clone());
        }
        let mut metadata = Vec::new();
        if let Some(pid) = status.pid {
            metadata.push(format!("PID {pid}"));
        }
        if let Some(toolset) = &status.mcp_toolset {
            metadata.push(format!("MCP tools: {toolset}"));
        }
        if !metadata.is_empty() {
            details.push(metadata.join(" · "));
        }
        return zg_cli::write_message(
            output,
            if status.ready {
                zg_cli::MessageKind::Success
            } else if status.running {
                zg_cli::MessageKind::Warning
            } else {
                zg_cli::MessageKind::Info
            },
            &format!("Server {label}"),
            &details.iter().map(String::as_str).collect::<Vec<_>>(),
            output_options().color,
            true,
        );
    }
    writeln!(output, "Server: {label}")?;
    if let Some(pid) = status.pid {
        writeln!(output, "PID: {pid}")?;
    }
    if let Some(url) = &status.server_url {
        writeln!(output, "URL: {url}")?;
    }
    if let Some(toolset) = &status.mcp_toolset {
        writeln!(output, "MCP toolset: {toolset}")?;
    }
    Ok(())
}

fn init_tracing(debug: bool) {
    let options = output_options();
    let color = options.color == zg_cli::ColorMode::Always
        || (options.color == zg_cli::ColorMode::Auto
            && zg_cli::stderr_supports_ansi()
            && std::env::var_os("NO_COLOR").is_none());
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(if debug {
            "zg=debug,zg_engine=debug,zg_daemon=debug"
        } else {
            "warn"
        })
    });
    if animated_progress(options) {
        let _ = tracing_subscriber::fmt()
            .with_writer(|| TerminalLogWriter(b"\r\x1b[K".to_vec()))
            .with_ansi(color)
            .with_env_filter(filter)
            .try_init();
    } else {
        let _ = tracing_subscriber::fmt()
            .with_writer(io::stderr)
            .with_ansi(color)
            .with_env_filter(filter)
            .try_init();
    }
}

// tracing constructs one writer per event. Buffer the event until it is complete,
// then replace the live line and print the log under one stderr lock. The next
// progress tick starts on the following line, keeping warnings in scrollback.
struct TerminalLogWriter(Vec<u8>);

impl Write for TerminalLogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for TerminalLogWriter {
    fn drop(&mut self) {
        if self.0.len() > 4 {
            let mut output = io::stderr().lock();
            let _ = output.write_all(&self.0);
            let _ = output.flush();
        }
    }
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
mod error_tests {
    use super::*;

    #[test]
    fn cancellation_is_recognized_in_direct_and_daemon_errors() {
        let error = EngineError::cancelled("Index creation cancelled");
        let report = error.report();
        let errors: [Box<dyn Error>; 3] = [
            Box::new(error),
            Box::new(zg_daemon::DaemonError::Engine(EngineError::from_report(
                report.clone(),
            ))),
            Box::new(zg_daemon::DaemonError::Remote {
                report: Box::new(report),
                retryable: false,
            }),
        ];
        for error in errors {
            assert!(is_engine_error(error.as_ref(), EngineError::CANCELLED));
            assert!(!is_engine_error(error.as_ref(), EngineError::NOT_FOUND));
        }
    }

    #[test]
    fn failures_are_not_treated_as_cancellation() {
        let error = EngineError::not_found("No index found");
        let report = error.report();
        let errors: [Box<dyn Error>; 4] = [
            Box::new(error),
            Box::new(zg_daemon::DaemonError::Engine(EngineError::from_report(
                report.clone(),
            ))),
            Box::new(zg_daemon::DaemonError::Remote {
                report: Box::new(report),
                retryable: false,
            }),
            Box::new(io::Error::other("Index creation failed")),
        ];
        for (index, error) in errors.iter().enumerate() {
            assert!(!is_engine_error(error.as_ref(), EngineError::CANCELLED));
            assert_eq!(
                is_engine_error(error.as_ref(), EngineError::NOT_FOUND),
                index < 3
            );
        }
    }
}

#[cfg(test)]
mod query_authorization_tests {
    use super::*;

    #[tokio::test]
    async fn query_choices_apply_to_execution_and_workspace_consent_is_reused() {
        const FIXTURE_ENV: &str = "ZG_QUERY_AUTHORIZATION_TEST_ROOT";
        let Some(root) = std::env::var_os(FIXTURE_ENV) else {
            // Isolate global configuration without mutating process environment.
            let workspace = tempfile::tempdir().expect("workspace");
            let state = tempfile::tempdir().expect("state");
            let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args(["--exact", "query_authorization_tests::query_choices_apply_to_execution_and_workspace_consent_is_reused", "--nocapture"])
                .env(FIXTURE_ENV, workspace.path())
                .env("HOME", state.path()).env("USERPROFILE", state.path())
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
