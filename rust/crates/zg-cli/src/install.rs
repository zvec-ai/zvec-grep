use std::{
    collections::{BTreeSet, HashSet},
    env, fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use serde_json::{Map, Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::{InstallArgs, McpInstallTransport, McpToolset, UninstallArgs};

const CONFIG_START: &str = "# ZVEC_GREP_START";
const CONFIG_END: &str = "# ZVEC_GREP_END";
const GUIDANCE_START: &str = "<!-- ZVEC_GREP_START -->";
const GUIDANCE_END: &str = "<!-- ZVEC_GREP_END -->";
const CLAUDE_PERMISSION: &str = "mcp__zvec_grep__*";
const SEARCH_PERMISSION: &str = "mcp__zvec_grep__zvec_grep_search";
const RG_PERMISSION: &str = "mcp__zvec_grep__zvec_grep_rg";
const QODER_DESCRIPTION: &str = "Managed by zg install";
const QODER_OWNERSHIP_PREFIX: &str = "Managed by zg install; managed permissions=";

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("{0}")]
    Message(String),
    #[error("installer I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("installer JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug)]
pub struct InstallOutcome {
    pub agent_labels: Vec<&'static str>,
    pub transport: McpInstallTransport,
    pub mcp_toolset: Option<McpToolset>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Agent {
    Claude,
    Codex,
    OpenCode,
    Cursor,
    Qwen,
    Qoder,
}

impl Agent {
    const ALL: [Self; 6] = [
        Self::Claude,
        Self::Codex,
        Self::OpenCode,
        Self::Cursor,
        Self::Qwen,
        Self::Qoder,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
            Self::Cursor => "Cursor",
            Self::Qwen => "Qwen Code",
            Self::Qoder => "Qoder",
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Cursor => "cursor",
            Self::Qwen => "qwen",
            Self::Qoder => "qoder",
        }
    }
}

#[derive(Clone, Debug)]
struct AgentOptions {
    force: bool,
    transport: McpInstallTransport,
    toolset: Option<McpToolset>,
    timeout_seconds: u64,
    token_env: Option<String>,
}

/// Installs the selected agent integrations and returns daemon startup choices.
///
/// # Errors
///
/// Returns an error when target selection, configuration validation, or an
/// atomic file update fails.
pub fn execute_install(args: &InstallArgs) -> Result<InstallOutcome, InstallError> {
    print_header();
    let detected = detect_agents();
    let agents = resolve_agents(
        args.targets.iter().chain(&args.positional_targets),
        args.yes,
        &detected,
        "install",
    )?;
    if agents.is_empty() {
        println!("\nNo agent integrations selected.");
        return Ok(InstallOutcome {
            agent_labels: Vec::new(),
            transport: args.transport.unwrap_or_default(),
            mcp_toolset: args.mcp_toolset,
        });
    }
    let transport = resolve_transport(args.transport, args.yes)?;
    let options = AgentOptions {
        force: args.force,
        transport,
        toolset: args.mcp_toolset,
        timeout_seconds: args.mcp_tool_timeout_seconds,
        token_env: args.mcp_token_env.clone(),
    };

    println!("\nInstalling integrations\n");
    for agent in &agents {
        install_agent(*agent, &options)?;
        println!("  ✓ {}", agent.label());
        println!("    MCP       configured\n");
    }

    Ok(InstallOutcome {
        agent_labels: agents.iter().map(|agent| agent.label()).collect(),
        transport,
        mcp_toolset: args.mcp_toolset,
    })
}

/// Removes only zvec-grep-managed agent configuration.
///
/// # Errors
///
/// Returns an error when an existing configuration is invalid or a file update
/// fails.
pub fn execute_uninstall(args: &UninstallArgs) -> Result<(), InstallError> {
    print_header();
    let detected = detect_agents();
    let agents = resolve_agents(
        args.targets.iter().chain(&args.positional_targets),
        args.yes,
        &detected,
        "uninstall",
    )?;
    if agents.is_empty() {
        println!("\nNo agent integrations selected.");
        return Ok(());
    }

    println!("\nRemoving integrations\n");
    for agent in agents {
        uninstall_agent(agent)?;
        println!("  ✓ {}", agent.label());
        println!("    integration removed");
    }
    println!("\nRestart the selected agents or start a new session to apply the change.");
    Ok(())
}

fn print_header() {
    println!("zvec-grep setup");
    println!("{}", "─".repeat(40));
}

fn resolve_transport(
    explicit: Option<McpInstallTransport>,
    yes: bool,
) -> Result<McpInstallTransport, InstallError> {
    if let Some(transport) = explicit {
        return Ok(transport);
    }
    if yes || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(McpInstallTransport::Stdio);
    }
    print!("MCP transport [stdio] (stdio/http): ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "" | "stdio" => Ok(McpInstallTransport::Stdio),
        "http" => Ok(McpInstallTransport::Http),
        _ => Err(InstallError::Message(
            "MCP transport must be stdio or http".to_owned(),
        )),
    }
}

fn resolve_agents<'a>(
    tokens: impl Iterator<Item = &'a String>,
    yes: bool,
    detected: &BTreeSet<Agent>,
    action: &str,
) -> Result<Vec<Agent>, InstallError> {
    let tokens = tokens
        .flat_map(|token| {
            token.split(|character: char| character == ',' || character.is_whitespace())
        })
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !tokens.is_empty() {
        return agents_from_tokens(&tokens, detected);
    }
    if yes || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(Agent::ALL
            .into_iter()
            .filter(|agent| detected.contains(agent))
            .collect());
    }

    println!(
        "\n{}\n",
        if action == "install" {
            "Choose agent integrations"
        } else {
            "Choose integrations to remove"
        }
    );
    for (index, agent) in Agent::ALL.iter().enumerate() {
        println!(
            "  {}. {} ({})",
            index + 1,
            agent.label(),
            if detected.contains(agent) {
                "detected"
            } else {
                "not found"
            }
        );
    }
    let defaults = if detected.is_empty() {
        "none".to_owned()
    } else {
        detected
            .iter()
            .map(|agent| agent.id())
            .collect::<Vec<_>>()
            .join(",")
    };
    print!("Agents [{defaults}]: ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = if answer.trim().is_empty() {
        defaults
    } else {
        answer.trim().to_owned()
    };
    agents_from_tokens(&[answer], detected)
}

fn agents_from_tokens(
    tokens: &[String],
    detected: &BTreeSet<Agent>,
) -> Result<Vec<Agent>, InstallError> {
    let mut selected = BTreeSet::new();
    for token in tokens.iter().flat_map(|token| {
        token.split(|character: char| character == ',' || character.is_whitespace())
    }) {
        if token.is_empty() {
            continue;
        }
        let lower = token.to_ascii_lowercase();
        match lower.as_str() {
            "none" => return Ok(Vec::new()),
            "auto" => selected.extend(detected.iter().copied()),
            "all" => selected.extend(Agent::ALL),
            "1" | "claude" | "cc" | "claude-code" => {
                selected.insert(Agent::Claude);
            }
            "2" | "codex" => {
                selected.insert(Agent::Codex);
            }
            "3" | "opencode" => {
                selected.insert(Agent::OpenCode);
            }
            "4" | "cursor" => {
                selected.insert(Agent::Cursor);
            }
            "5" | "qwen" | "qwen-code" | "qwencode" => {
                selected.insert(Agent::Qwen);
            }
            "6" | "qoder" => {
                selected.insert(Agent::Qoder);
            }
            _ => {
                return Err(InstallError::Message(format!(
                    "Unknown install target: {token}"
                )));
            }
        }
    }
    Ok(Agent::ALL
        .into_iter()
        .filter(|agent| selected.contains(agent))
        .collect())
}

fn detect_agents() -> BTreeSet<Agent> {
    Agent::ALL
        .into_iter()
        .filter(|agent| match agent {
            Agent::Claude => executable_available("claude"),
            Agent::Codex => executable_available("codex"),
            Agent::OpenCode => executable_available("opencode"),
            Agent::Cursor => executable_available("cursor"),
            Agent::Qwen => executable_available("qwen"),
            Agent::Qoder => {
                executable_available("qoder")
                    || executable_available("qodercli")
                    || executable_available("qoder-ide")
                    || qoder_ide_available()
            }
        })
        .collect()
}

fn executable_available(name: &str) -> bool {
    let path = env::var_os("PATH").unwrap_or_default();
    env::split_paths(&path).any(|directory| {
        #[cfg(windows)]
        {
            let extensions =
                env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_owned());
            extensions.split(';').any(|extension| {
                let candidate = directory.join(format!("{name}{}", extension.to_ascii_lowercase()));
                candidate.is_file()
            })
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(directory.join(name)).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        }
    })
}

fn qoder_ide_available() -> bool {
    if let Some(configured) = non_empty_env("QODER_IDE_EXECUTABLE") {
        return executable_path(&absolute_path(&configured));
    }
    qoder_ide_candidates()
        .iter()
        .any(|path| executable_path(path))
}

fn executable_path(path: &Path) -> bool {
    #[cfg(windows)]
    {
        path.is_file()
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
}

fn qoder_ide_candidates() -> Vec<PathBuf> {
    #[cfg(any(target_os = "macos", windows))]
    let home = home_dir();
    #[cfg(target_os = "macos")]
    return vec![
        home.join("Applications/Qoder IDE.app/Contents/MacOS/Qoder"),
        PathBuf::from("/Applications/Qoder IDE.app/Contents/MacOS/Qoder"),
        home.join("Applications/Qoder.app/Contents/MacOS/Qoder"),
        PathBuf::from("/Applications/Qoder.app/Contents/MacOS/Qoder"),
    ];
    #[cfg(windows)]
    return vec![
        PathBuf::from(
            env::var_os("LOCALAPPDATA")
                .unwrap_or_else(|| home.join("AppData/Local").into_os_string()),
        )
        .join("Programs/Qoder IDE/Qoder IDE.exe"),
        PathBuf::from(
            env::var_os("LOCALAPPDATA")
                .unwrap_or_else(|| home.join("AppData/Local").into_os_string()),
        )
        .join("Programs/Qoder/Qoder.exe"),
    ];
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    vec![
        PathBuf::from("/usr/share/qoder-ide/qoder-ide"),
        PathBuf::from("/usr/share/qoder-ide/bin/qoder-ide"),
        PathBuf::from("/usr/bin/qoder-ide"),
        PathBuf::from("/usr/share/qoder/bin/qoder"),
    ]
}

fn install_agent(agent: Agent, options: &AgentOptions) -> Result<(), InstallError> {
    match agent {
        Agent::Claude => install_claude(options),
        Agent::Codex => install_codex(options),
        Agent::OpenCode => install_opencode(options),
        Agent::Cursor => install_cursor(options),
        Agent::Qwen => install_qwen(options),
        Agent::Qoder => install_qoder(options),
    }
}

fn uninstall_agent(agent: Agent) -> Result<(), InstallError> {
    match agent {
        Agent::Claude => uninstall_claude(),
        Agent::Codex => uninstall_codex(),
        Agent::OpenCode => uninstall_opencode(),
        Agent::Cursor => uninstall_cursor(),
        Agent::Qwen => uninstall_qwen(),
        Agent::Qoder => uninstall_qoder(),
    }
}

fn install_codex(options: &AgentOptions) -> Result<(), InstallError> {
    let home = env_path("CODEX_HOME").unwrap_or_else(|| home_dir().join(".codex"));
    let config = home.join("config.toml");
    let guidance = home.join("AGENTS.md");
    write_marked_file(
        &config,
        CONFIG_START,
        CONFIG_END,
        &codex_block(options),
        options.force,
        Some(codex_conflict),
        Some(remove_codex_conflict),
    )?;
    write_marked_file(
        &guidance,
        GUIDANCE_START,
        GUIDANCE_END,
        &guidance_block("zvec_grep_search", "zvec_grep_rg", false),
        true,
        None,
        None,
    )
}

fn uninstall_codex() -> Result<(), InstallError> {
    let home = env_path("CODEX_HOME").unwrap_or_else(|| home_dir().join(".codex"));
    remove_marked_file(&home.join("config.toml"), CONFIG_START, CONFIG_END)?;
    remove_marked_file(&home.join("AGENTS.md"), GUIDANCE_START, GUIDANCE_END)
}

fn install_claude(options: &AgentOptions) -> Result<(), InstallError> {
    let directory = env_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home_dir().join(".claude"));
    let mcp_path = if env::var_os("CLAUDE_CONFIG_DIR").is_some() {
        directory.join(".claude.json")
    } else {
        home_dir().join(".claude.json")
    };
    let mut root = read_json_object(&mcp_path)?;
    let servers = object_field_mut(&mut root, "mcpServers", options.force, &mcp_path)?;
    reject_unmanaged(
        servers.get("zvec_grep"),
        options.force,
        &mcp_path,
        "Claude Code",
    )?;
    let retained = servers
        .get("zvec_grep")
        .and_then(Value::as_object)
        .map(|server| {
            server
                .iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "type" | "url" | "command" | "args" | "headers"
                    )
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Map<_, _>>()
        })
        .unwrap_or_default();
    let mut server = retained;
    match options.transport {
        McpInstallTransport::Stdio => {
            server.insert("type".to_owned(), json!("stdio"));
            server.insert("command".to_owned(), json!("zg"));
            server.insert("args".to_owned(), json!(stdio_args(options.toolset)));
        }
        McpInstallTransport::Http => {
            server.insert("type".to_owned(), json!("http"));
            server.insert("url".to_owned(), json!(resolve_server_url()?));
            if let Some(token) = &options.token_env {
                server.insert(
                    "headers".to_owned(),
                    json!({"Authorization": format!("Bearer ${{{token}}}")}),
                );
            }
        }
    }
    servers.insert("zvec_grep".to_owned(), Value::Object(server));
    write_json_object(&mcp_path, &root)?;

    let settings_path = directory.join("settings.json");
    update_claude_permission(&settings_path, true)?;
    write_marked_file(
        &directory.join("CLAUDE.md"),
        GUIDANCE_START,
        GUIDANCE_END,
        &guidance_block("zvec_grep_search", "zvec_grep_rg", false),
        true,
        None,
        None,
    )
}

fn uninstall_claude() -> Result<(), InstallError> {
    let directory = env_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home_dir().join(".claude"));
    let mcp_path = if env::var_os("CLAUDE_CONFIG_DIR").is_some() {
        directory.join(".claude.json")
    } else {
        home_dir().join(".claude.json")
    };
    remove_strict_json_server(&mcp_path, "mcpServers")?;
    update_claude_permission(&directory.join("settings.json"), false)?;
    remove_marked_file(&directory.join("CLAUDE.md"), GUIDANCE_START, GUIDANCE_END)
}

fn install_opencode(options: &AgentOptions) -> Result<(), InstallError> {
    let path = env_path("OPENCODE_CONFIG")
        .unwrap_or_else(|| home_dir().join(".config/opencode/opencode.json"));
    let server = match options.transport {
        McpInstallTransport::Stdio => json!({
            "type": "local", "command": stdio_command(options.toolset), "enabled": true,
            "timeout": options.timeout_seconds.saturating_mul(1000)
        }),
        McpInstallTransport::Http => {
            let mut server = json!({
                "type": "remote", "url": resolve_server_url()?, "enabled": true,
                "timeout": options.timeout_seconds.saturating_mul(1000), "oauth": false
            });
            if let Some(token) = &options.token_env {
                server["headers"] = json!({"Authorization": format!("Bearer {{env:{token}}}")});
            }
            server
        }
    };
    install_strict_json_server(&path, "mcp", server, options.force, "OpenCode")?;
    write_marked_file(
        &path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("AGENTS.md"),
        GUIDANCE_START,
        GUIDANCE_END,
        &guidance_block(
            "zvec_grep_zvec_grep_search",
            "zvec_grep_zvec_grep_rg",
            false,
        ),
        true,
        None,
        None,
    )
}

fn uninstall_opencode() -> Result<(), InstallError> {
    let path = env_path("OPENCODE_CONFIG")
        .unwrap_or_else(|| home_dir().join(".config/opencode/opencode.json"));
    remove_strict_json_server(&path, "mcp")?;
    remove_marked_file(
        &path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("AGENTS.md"),
        GUIDANCE_START,
        GUIDANCE_END,
    )
}

fn install_cursor(options: &AgentOptions) -> Result<(), InstallError> {
    let path = env_path("CURSOR_CONFIG_DIR")
        .unwrap_or_else(|| home_dir().join(".cursor"))
        .join("mcp.json");
    let server = match options.transport {
        McpInstallTransport::Stdio => json!({"command": "zg", "args": stdio_args(options.toolset)}),
        McpInstallTransport::Http => {
            let mut server = json!({"url": resolve_server_url()?});
            if let Some(token) = &options.token_env {
                server["headers"] = json!({"Authorization": format!("Bearer ${{{token}}}")});
            }
            server
        }
    };
    install_strict_json_server(&path, "mcpServers", server, options.force, "Cursor")
}

fn uninstall_cursor() -> Result<(), InstallError> {
    let path = env_path("CURSOR_CONFIG_DIR")
        .unwrap_or_else(|| home_dir().join(".cursor"))
        .join("mcp.json");
    remove_strict_json_server(&path, "mcpServers")
}

fn install_qwen(options: &AgentOptions) -> Result<(), InstallError> {
    let home = resolve_qwen_home()?;
    let path = home.join("settings.json");
    let server = qwen_server(options)?;
    update_jsonc_server(&path, &server, options.force, "Qwen Code", is_managed_qwen)?;
    if let Some(warning) = context_warning(&path, "QWEN.md")? {
        eprintln!("    warning   {warning}");
    }
    write_marked_file(
        &home.join("QWEN.md"),
        GUIDANCE_START,
        GUIDANCE_END,
        &guidance_block(SEARCH_PERMISSION, RG_PERMISSION, false),
        true,
        None,
        None,
    )
}

fn uninstall_qwen() -> Result<(), InstallError> {
    let home = resolve_qwen_home()?;
    remove_jsonc_server(&home.join("settings.json"), "Qwen Code", is_managed_qwen)?;
    remove_marked_file(&home.join("QWEN.md"), GUIDANCE_START, GUIDANCE_END)
}

fn install_qoder(options: &AgentOptions) -> Result<(), InstallError> {
    let home = env_path_non_empty("QODER_CONFIG_DIR").unwrap_or_else(|| home_dir().join(".qoder"));
    let settings = home.join("settings.json");
    let ide = qoder_ide_path();
    preflight_jsonc_server(
        &settings,
        options.force,
        "Qoder CLI",
        is_managed_json_server,
    )?;
    validate_qoder_permissions(&settings)?;
    preflight_jsonc_server(&ide, options.force, "Qoder IDE", is_managed_qoder_ide)?;

    update_qoder_cli(&settings, options)?;
    update_jsonc_server(
        &ide,
        &qoder_ide_server(options)?,
        options.force,
        "Qoder IDE",
        is_managed_qoder_ide,
    )?;
    if let Some(warning) = context_warning(&settings, "AGENTS.md")? {
        eprintln!("    warning   {warning}");
    }
    write_marked_file(
        &home.join("AGENTS.md"),
        GUIDANCE_START,
        GUIDANCE_END,
        &guidance_block(SEARCH_PERMISSION, RG_PERMISSION, true),
        true,
        None,
        None,
    )
}

fn uninstall_qoder() -> Result<(), InstallError> {
    let home = env_path_non_empty("QODER_CONFIG_DIR").unwrap_or_else(|| home_dir().join(".qoder"));
    remove_qoder_cli(&home.join("settings.json"))?;
    remove_jsonc_server(&qoder_ide_path(), "Qoder IDE", is_managed_qoder_ide)?;
    remove_marked_file(&home.join("AGENTS.md"), GUIDANCE_START, GUIDANCE_END)
}

fn qwen_server(options: &AgentOptions) -> Result<Value, InstallError> {
    let timeout = options.timeout_seconds.saturating_mul(1000);
    Ok(match options.transport {
        McpInstallTransport::Stdio => json!({
            "command": "zg", "args": stdio_args(options.toolset), "timeout": timeout,
            "alwaysLoadTools": true, "trust": true
        }),
        McpInstallTransport::Http => {
            let mut server = json!({
                "httpUrl": resolve_server_url()?, "timeout": timeout,
                "alwaysLoadTools": true, "trust": true
            });
            if let Some(token) = &options.token_env {
                server["headers"] = json!({"Authorization": format!("Bearer ${{{token}}}")});
            }
            server
        }
    })
}

fn qoder_ide_server(options: &AgentOptions) -> Result<Value, InstallError> {
    let timeout = options.timeout_seconds.saturating_mul(1000);
    Ok(match options.transport {
        McpInstallTransport::Stdio => json!({
            "command": env::current_exe()?.to_string_lossy(),
            "args": stdio_args(options.toolset),
            "timeout": timeout,
            "description": QODER_DESCRIPTION
        }),
        McpInstallTransport::Http => {
            let mut server = json!({
                "type": "sse", "url": resolve_server_url()?, "timeout": timeout,
                "description": QODER_DESCRIPTION
            });
            if let Some(token) = &options.token_env {
                server["headers"] = json!({"Authorization": format!("Bearer ${{{token}}}")});
            }
            server
        }
    })
}

fn stdio_args(toolset: Option<McpToolset>) -> Vec<&'static str> {
    let mut args = vec!["server", "--stdio"];
    if let Some(toolset) = toolset {
        args.extend([
            "--mcp-toolset",
            match toolset {
                McpToolset::Agent => "agent",
                McpToolset::Full => "full",
            },
        ]);
    }
    args
}

fn stdio_command(toolset: Option<McpToolset>) -> Vec<&'static str> {
    let mut command = vec!["zg"];
    command.extend(stdio_args(toolset));
    command
}

fn codex_block(options: &AgentOptions) -> String {
    let connection = match options.transport {
        McpInstallTransport::Stdio => format!(
            "command = \"zg\"\nargs = {}",
            toml_string_array(&stdio_args(options.toolset))
        ),
        McpInstallTransport::Http => format!(
            "url = \"{}\"",
            resolve_server_url().unwrap_or_else(|_| "http://127.0.0.1:7999/mcp".to_owned())
        ),
    };
    let token = options
        .token_env
        .as_ref()
        .map_or_else(String::new, |token| {
            format!("bearer_token_env_var = \"{token}\"\n")
        });
    format!(
        "{CONFIG_START}\n[mcp_servers.zvec_grep]\n{connection}\n{token}tool_timeout_sec = {}\ndefault_tools_approval_mode = \"approve\"\n{CONFIG_END}",
        options.timeout_seconds
    )
}

fn toml_string_array(values: &[&str]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|value| format!("\"{value}\""))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn guidance_block(search: &str, rg: &str, qoder_recovery: bool) -> String {
    let exact =
        format!("`{rg}` when it is listed by the current host; otherwise native Grep or `rg`");
    let recovery = if qoder_recovery {
        format!(
            "\n\n### Qoder Remote Embedding authorization recovery\n- When `{search}` needs `remote_embedding_authorization` and the current Qoder host returns `code = 51500 message = method not found: No request handler configured` without showing an authorization form, returns `The connected MCP host does not support the Remote Embedding authorization interaction required by elicitation/create. The agent should use the current host's built-in user-question tool; for Qoder, the exact name is ask_user_question in Qoder IDE or AskUserQuestion in Qoder CLI/SDK. Ask the user to choose: allow Remote Embedding for this workspace, use local FTS only, or cancel. No user decision was received, and no remote data was sent.`, or reports that authorization was declined or cancelled without showing the user an authorization form, treat it as a Qoder client interaction limitation: the host lacks the server-to-client `elicitation/create` request handler. The outer MCP `tools/call` has already reached the registered MCP server and tool, so do not diagnose this error as a disconnected or missing MCP server or as a missing tool. Apply this recovery only to the Remote Embedding authorization path, not to arbitrary 51500 failures. Do not immediately fall back to broad file reads, do not treat it as a missing API credential, and do not grant access silently. If the user actually declined a displayed authorization form, respect that decision and do not ask again.\n- Use the current Qoder host's built-in user-question tool (`ask_user_question` in Qoder IDE or `AskUserQuestion` in Qoder CLI/SDK) to offer exactly these choices: allow Remote Embedding for this workspace, use local FTS only, or cancel. Explain that workspace approval may send query text and selected workspace content to the configured provider and endpoint and may incur provider charges.\n- Only after the user explicitly chooses workspace approval, run `zg auth grant \"<absolute-root>\" --capability embedding --scope workspace`, substituting the same absolute root used by the failed search, and then retry the original search call once. Do not use `--allow-remote`; it applies only to one CLI command and does not authorize the MCP retry.\n- If the user chooses local FTS, retry `{search}` once with the original search text in `fts`, omit `query`, `queries`, and `vector`, set `autoUpdate` to `false` and `freshness` to `eventual`, and preserve `root`, filters, and limits. This route is lexical-only, does not refresh the remote-embedding index, and sends no query text or workspace content to a remote Embedding provider.\n- If the user cancels, the grant command fails, or interactive user input is unavailable, stop and report that no remote data was sent. Provider credentials and Remote Embedding data authorization are separate; never request or modify an API key merely to resolve this interaction error."
        )
    } else {
        String::new()
    };
    format!(
        "{GUIDANCE_START}\n## zvec-grep\n\nChoose the evidence source before the retrieval mode.\n\n### Workspace evidence\n- Use the current workspace as the evidence source when the user asks about local material, prior context establishes it as relevant, or the question concerns how the current project works—even if the workspace is not mentioned explicitly.\n- A workspace may contain any mix of code, documents, configuration, and data.\n- Do not use workspace retrieval for unrelated open-world questions, current external facts, or web content that does not depend on local evidence.\n\n### Retrieval routing\n- When an exact word, phrase, name, date, identifier, filename, path, configuration key, error message, source fragment, literal, or regex is known and locating its occurrences is sufficient, use {exact}.\n- Use `{search}` when wording or location is unknown, or when the answer requires semantic, conceptual, fuzzy, or paraphrase discovery; relationships, chronology, causality, architecture, or data or control flow; or comparison or synthesis across files, sections, or documents.\n- For a mixed task with exact anchors that still requires relationships or cross-file synthesis, call `{search}` with the concept and anchors, then use {exact} for focused follow-up.\n- When no sufficient exact anchor is available and the user asks whether conceptually related material exists locally, make at most one focused `{search}` probe using the question plus distinctive names, dates, or terms. This probe does not apply to exact quotations, configuration keys, filenames, regexes, or exhaustive occurrence requests. Continue only when results are relevant; otherwise stop and report that the indexed workspace did not establish the answer.\n- Before broad file reads or delegating workspace discovery, use the appropriate search route. Do not delegate solely to locate material, and stop when the evidence is sufficient.\n\n### Search evidence\n- Search results include bounded source snippets. Treat a sufficient snippet as already-read evidence, and read a cited file only when a required detail falls outside the snippet.\n\n### Freshness and index lifecycle\n- Pass a daemon-visible absolute `root` on every zvec-grep workspace call.\n- Read `freshness` and `background_refresh` from search results without a status preflight.\n- When results are `served_from_current_index`, use them when sufficient instead of waiting for the background refresh.\n- If the index is missing but exact or regex lookup can answer the task, use {exact}.\n- Creating, rebuilding, or dropping a persistent index requires an explicit user request or authorization; never do so silently.{recovery}\n{GUIDANCE_END}"
    )
}

fn install_strict_json_server(
    path: &Path,
    container_key: &str,
    server: Value,
    force: bool,
    label: &str,
) -> Result<(), InstallError> {
    let mut root = read_json_object(path)?;
    let container = object_field_mut(&mut root, container_key, false, path)?;
    reject_unmanaged(container.get("zvec_grep"), force, path, label)?;
    container.insert("zvec_grep".to_owned(), server);
    write_json_object(path, &root)
}

fn remove_strict_json_server(path: &Path, container_key: &str) -> Result<(), InstallError> {
    let source = read_if_exists(path)?;
    if source.trim().is_empty() {
        return Ok(());
    }
    let mut root = parse_json_object(path, &source)?;
    let Some(container) = root.get_mut(container_key).and_then(Value::as_object_mut) else {
        return Ok(());
    };
    if !container
        .get("zvec_grep")
        .is_some_and(is_managed_json_server)
    {
        return Ok(());
    }
    container.remove("zvec_grep");
    if container.is_empty() {
        root.remove(container_key);
    }
    write_json_object(path, &root)
}

fn reject_unmanaged(
    current: Option<&Value>,
    force: bool,
    path: &Path,
    label: &str,
) -> Result<(), InstallError> {
    if current.is_some_and(|value| !is_managed_json_server(value)) && !force {
        return Err(InstallError::Message(format!(
            "Existing unmanaged zvec_grep MCP server found in {}. Re-run with --force to replace it for {label}.",
            path.display()
        )));
    }
    Ok(())
}

fn object_field_mut<'a>(
    root: &'a mut Map<String, Value>,
    key: &str,
    replace_invalid: bool,
    path: &Path,
) -> Result<&'a mut Map<String, Value>, InstallError> {
    if root.get(key).is_some_and(|value| !value.is_object()) {
        if !replace_invalid {
            return Err(InstallError::Message(format!(
                "Expected {key} in {} to be a JSON object",
                path.display()
            )));
        }
        root.remove(key);
    }
    root.entry(key.to_owned())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            InstallError::Message(format!(
                "Expected {key} in {} to be a JSON object",
                path.display()
            ))
        })
}

fn update_claude_permission(path: &Path, grant: bool) -> Result<(), InstallError> {
    let source = read_if_exists(path)?;
    if source.trim().is_empty() && !grant {
        return Ok(());
    }
    let mut root = if source.trim().is_empty() {
        Map::new()
    } else {
        parse_json_object(path, &source)?
    };
    let permissions = object_field_mut(&mut root, "permissions", false, path)?;
    let allow = permissions
        .entry("allow".to_owned())
        .or_insert_with(|| json!([]));
    let Some(allow) = allow.as_array_mut() else {
        return Err(InstallError::Message(format!(
            "Invalid permissions.allow configuration in {}.",
            path.display()
        )));
    };
    if allow.iter().any(|rule| !rule.is_string()) {
        return Err(InstallError::Message(format!(
            "Invalid permissions.allow rule in {}.",
            path.display()
        )));
    }
    if grant {
        if !allow.iter().any(|rule| rule == CLAUDE_PERMISSION) {
            allow.push(json!(CLAUDE_PERMISSION));
        }
    } else {
        allow.retain(|rule| rule != CLAUDE_PERMISSION);
        if allow.is_empty() {
            permissions.remove("allow");
        }
        if permissions.is_empty() {
            root.remove("permissions");
        }
    }
    write_json_object(path, &root)
}

fn is_managed_json_server(value: &Value) -> bool {
    let Some(server) = value.as_object() else {
        return false;
    };
    if server
        .get("url")
        .and_then(Value::as_str)
        .is_some_and(|url| resolve_server_url().is_ok_and(|managed| url == managed))
    {
        return true;
    }
    if server.get("command").and_then(Value::as_str) == Some("zg")
        && server.get("args").is_some_and(is_stdio_args)
    {
        return true;
    }
    server.get("type").and_then(Value::as_str) == Some("local")
        && server
            .get("command")
            .and_then(Value::as_array)
            .is_some_and(|command| {
                (command.len() == 3 || command.len() == 5)
                    && command.first().and_then(Value::as_str) == Some("zg")
                    && is_stdio_args(&Value::Array(command[1..].to_vec()))
            })
}

fn is_managed_qwen(value: &Value) -> bool {
    let Some(server) = value.as_object() else {
        return false;
    };
    server
        .get("httpUrl")
        .and_then(Value::as_str)
        .is_some_and(|url| resolve_server_url().is_ok_and(|managed| url == managed))
        || (server.get("command").and_then(Value::as_str) == Some("zg")
            && server.get("args").is_some_and(is_stdio_args))
}

fn is_managed_qoder_ide(value: &Value) -> bool {
    let Some(server) = value.as_object() else {
        return false;
    };
    server.get("description").and_then(Value::as_str) == Some(QODER_DESCRIPTION)
        && ((server.get("command").and_then(Value::as_str).is_some()
            && server.get("args").is_some_and(is_qoder_ide_stdio_args))
            || (server.get("type").and_then(Value::as_str) == Some("sse")
                && server.get("url").and_then(Value::as_str).is_some()))
}

fn is_qoder_ide_stdio_args(value: &Value) -> bool {
    if is_stdio_args(value) {
        return true;
    }
    let Some(args) = value.as_array() else {
        return false;
    };
    (args.len() == 3 || args.len() == 5)
        && args.first().and_then(Value::as_str).is_some()
        && args.get(1).and_then(Value::as_str) == Some("server")
        && args.get(2).and_then(Value::as_str) == Some("--stdio")
        && (args.len() == 3
            || (args.get(3).and_then(Value::as_str) == Some("--mcp-toolset")
                && matches!(args.get(4).and_then(Value::as_str), Some("agent" | "full"))))
}

fn is_stdio_args(value: &Value) -> bool {
    let Some(args) = value.as_array() else {
        return false;
    };
    (args.len() == 2 || args.len() == 4)
        && args.first().and_then(Value::as_str) == Some("server")
        && args.get(1).and_then(Value::as_str) == Some("--stdio")
        && (args.len() == 2
            || (args.get(2).and_then(Value::as_str) == Some("--mcp-toolset")
                && matches!(args.get(3).and_then(Value::as_str), Some("agent" | "full"))))
}

fn read_json_object(path: &Path) -> Result<Map<String, Value>, InstallError> {
    let source = read_if_exists(path)?;
    if source.trim().is_empty() {
        Ok(Map::new())
    } else {
        parse_json_object(path, &source)
    }
}

fn parse_json_object(path: &Path, source: &str) -> Result<Map<String, Value>, InstallError> {
    let value: Value = serde_json::from_str(source).map_err(|error| {
        InstallError::Message(format!("Invalid JSON in {}: {error}", path.display()))
    })?;
    value.as_object().cloned().ok_or_else(|| {
        InstallError::Message(format!("Expected a JSON object in {}.", path.display()))
    })
}

fn write_json_object(path: &Path, value: &Map<String, Value>) -> Result<(), InstallError> {
    let mut content = serde_json::to_string_pretty(value)?;
    content.push('\n');
    atomic_write(path, &content)
}

fn read_if_exists(path: &Path) -> Result<String, io::Error> {
    match fs::read_to_string(path) {
        Ok(source) => Ok(source),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error),
    }
}

fn atomic_write(path: &Path, content: &str) -> Result<(), InstallError> {
    let target = resolve_atomic_target(path, &mut HashSet::new())?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = target.with_file_name(format!(
        "{}.{}.{}.tmp",
        target.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        Uuid::new_v4()
    ));
    let result = (|| -> Result<(), io::Error> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        if let Ok(metadata) = fs::metadata(&target) {
            fs::set_permissions(&temporary, metadata.permissions())?;
        }
        fs::rename(&temporary, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(InstallError::Io)
}

fn resolve_atomic_target(
    path: &Path,
    visited: &mut HashSet<PathBuf>,
) -> Result<PathBuf, InstallError> {
    let absolute = absolute_path(path);
    if !visited.insert(absolute.clone()) {
        return Err(InstallError::Message(format!(
            "Cannot atomically write through circular symbolic link: {}",
            path.display()
        )));
    }
    match fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let link = fs::read_link(&absolute)?;
            let next = if link.is_absolute() {
                link
            } else {
                absolute
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(link)
            };
            resolve_atomic_target(&next, visited)
        }
        Ok(_) => Ok(absolute),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(absolute),
        Err(error) => Err(error.into()),
    }
}

fn absolute_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_owned()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn write_marked_file(
    path: &Path,
    start: &str,
    end: &str,
    block: &str,
    force: bool,
    conflict: Option<fn(&str) -> bool>,
    remove_conflict: Option<fn(&str) -> String>,
) -> Result<(), InstallError> {
    let mut existing = read_if_exists(path)?;
    let replaced = replace_marked_block(&existing, start, end, block);
    if replaced.is_none() {
        existing = remove_orphaned_markers(&existing, start, end);
    }
    if replaced.is_none() && conflict.is_some_and(|check| check(&existing)) {
        if !force {
            return Err(InstallError::Message(format!(
                "Existing [mcp_servers.zvec_grep] found in {}. Re-run with --force after removing or moving that table into the zvec-grep managed block.",
                path.display()
            )));
        }
        if let Some(remove) = remove_conflict {
            existing = remove(&existing);
        }
    }
    atomic_write(
        path,
        &replaced.unwrap_or_else(|| append_marked_block(&existing, block)),
    )
}

fn remove_marked_file(path: &Path, start: &str, end: &str) -> Result<(), InstallError> {
    let existing = read_if_exists(path)?;
    if existing.is_empty() {
        return Ok(());
    }
    let next = replace_marked_block(&existing, start, end, "")
        .unwrap_or_else(|| remove_orphaned_markers(&existing, start, end));
    if next != existing {
        atomic_write(path, &next)?;
    }
    Ok(())
}

fn replace_marked_block(existing: &str, start: &str, end: &str, block: &str) -> Option<String> {
    let lines = existing.lines().collect::<Vec<_>>();
    let mut marker_lines = HashSet::new();
    let mut ranges = Vec::new();
    let mut pending = None;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed == start {
            marker_lines.insert(index);
            pending = Some(index);
        } else if trimmed == end {
            marker_lines.insert(index);
            if let Some(begin) = pending.take() {
                ranges.push((begin, index));
            }
        }
    }
    let first = ranges.first().copied()?;
    let in_removed_range = |index: usize| {
        ranges
            .iter()
            .skip(1)
            .any(|(begin, finish)| index >= *begin && index <= *finish)
    };
    let before = lines[..first.0]
        .iter()
        .enumerate()
        .filter(|(index, _)| !marker_lines.contains(index))
        .map(|(_, line)| *line)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_owned();
    let after = lines[first.1 + 1..]
        .iter()
        .enumerate()
        .filter(|(offset, _)| {
            let index = first.1 + 1 + offset;
            !marker_lines.contains(&index) && !in_removed_range(index)
        })
        .map(|(_, line)| *line)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();
    Some(
        [before.as_str(), block.trim(), after.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
            + "\n",
    )
}

fn remove_orphaned_markers(existing: &str, start: &str, end: &str) -> String {
    existing
        .lines()
        .filter(|line| !matches!(line.trim(), value if value == start || value == end))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_owned()
        + "\n"
}

fn append_marked_block(existing: &str, block: &str) -> String {
    let prefix = existing.trim_end();
    format!(
        "{}{block}\n",
        if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}\n\n")
        }
    )
}

fn codex_conflict(existing: &str) -> bool {
    existing
        .lines()
        .any(|line| toml_table_name(line).as_deref().is_some_and(is_codex_table))
}

fn remove_codex_conflict(existing: &str) -> String {
    let mut kept = Vec::new();
    let mut skipping = false;
    for line in existing.lines() {
        if let Some(table) = toml_table_name(line) {
            skipping = is_codex_table(&table);
        }
        if !skipping {
            kept.push(line);
        }
    }
    kept.join("\n").trim_end().to_owned() + "\n"
}

fn toml_table_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !trimmed.starts_with('[') {
        return None;
    }
    let close = trimmed.find(']')?;
    if !trimmed[close + 1..].trim().is_empty() && !trimmed[close + 1..].trim().starts_with('#') {
        return None;
    }
    Some(trimmed[1..close].trim().to_owned())
}

fn is_codex_table(table: &str) -> bool {
    let normalized = table.replace(['"', '\'', ' '], "");
    normalized == "mcp_servers.zvec_grep" || normalized.starts_with("mcp_servers.zvec_grep.")
}

fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let home = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"));
    home.map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name).map(PathBuf::from).map(absolute_path)
}
fn env_path_non_empty(name: &str) -> Option<PathBuf> {
    non_empty_env(name).map(absolute_path)
}
fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn qoder_ide_path() -> PathBuf {
    env_path_non_empty("QODER_IDE_MCP_PATH").unwrap_or_else(|| home_dir().join(".qoder/mcp.json"))
}

fn resolve_qwen_home() -> Result<PathBuf, InstallError> {
    let default = home_dir().join(".qwen");
    if let Ok(value) = env::var("QWEN_HOME") {
        return Ok(if value.is_empty() {
            default
        } else {
            expand_home(&value)
        });
    }
    for path in [default.join(".env"), home_dir().join(".env")] {
        let source = read_if_exists(&path)?;
        if let Some(value) = parse_dotenv_value(&source, "QWEN_HOME") {
            return Ok(expand_home(&value));
        }
    }
    Ok(default)
}

fn parse_dotenv_value(source: &str, key: &str) -> Option<String> {
    source.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with('#') {
            return None;
        }
        let (name, value) = line.split_once('=')?;
        (name.trim() == key)
            .then(|| value.trim().trim_matches(['"', '\'']).to_owned())
            .filter(|value| !value.is_empty())
    })
}

fn expand_home(value: &str) -> PathBuf {
    if value == "~" {
        home_dir()
    } else if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        home_dir().join(rest)
    } else {
        absolute_path(value)
    }
}

/// Resolves the MCP URL with the TypeScript installer's precedence.
///
/// # Errors
///
/// Returns an error when the global configuration cannot be read or parsed.
pub fn resolve_server_url() -> Result<String, InstallError> {
    if let Some(url) = non_empty_env("ZVEC_GREP_SERVER_URL") {
        return Ok(url);
    }
    let path = home_dir().join(".zvec-grep/config.json");
    let source = read_if_exists(&path)?;
    if !source.trim().is_empty() {
        let root = parse_json_object(&path, &source)?;
        if let Some(url) = root
            .get("client")
            .and_then(Value::as_object)
            .and_then(|client| client.get("serverUrl"))
            .and_then(Value::as_str)
        {
            return Ok(url.to_owned());
        }
        let server = root.get("server").and_then(Value::as_object);
        let host = server
            .and_then(|value| value.get("host"))
            .and_then(Value::as_str)
            .unwrap_or("127.0.0.1");
        let port = server
            .and_then(|value| value.get("port"))
            .and_then(Value::as_u64)
            .unwrap_or(7999);
        return Ok(format!(
            "http://{}:{port}/mcp",
            if host.contains(':') {
                format!("[{host}]")
            } else {
                host.to_owned()
            }
        ));
    }
    Ok("http://127.0.0.1:7999/mcp".to_owned())
}

/// Resolves the loopback listen address used when install starts the daemon.
///
/// # Errors
///
/// Returns an error when the global configuration cannot be read or parsed.
pub fn resolve_server_listen() -> Result<String, InstallError> {
    let path = home_dir().join(".zvec-grep/config.json");
    let source = read_if_exists(&path)?;
    if source.trim().is_empty() {
        return Ok("127.0.0.1:7999".to_owned());
    }
    let root = parse_json_object(&path, &source)?;
    let server = root.get("server").and_then(Value::as_object);
    let host = server
        .and_then(|value| value.get("host"))
        .and_then(Value::as_str)
        .unwrap_or("127.0.0.1");
    let port = server
        .and_then(|value| value.get("port"))
        .and_then(Value::as_u64)
        .unwrap_or(7999);
    Ok(format!(
        "{}:{port}",
        if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_owned()
        }
    ))
}

fn context_warning(path: &Path, file_name: &str) -> Result<Option<String>, InstallError> {
    let source = read_if_exists(path)?;
    if source.trim().is_empty() {
        return Ok(None);
    }
    let root = parse_jsonc_object(path, &source, "agent")?;
    let configured = root
        .get("context")
        .and_then(Value::as_object)
        .and_then(|context| context.get("fileName"));
    let included = match configured {
        Some(Value::String(value)) => value == file_name,
        Some(Value::Array(values)) if values.iter().all(Value::is_string) => {
            values.iter().any(|value| value == file_name)
        }
        _ => true,
    };
    Ok((!included).then(|| format!("context.fileName does not include {file_name}; the installed zvec-grep guidance may not be loaded.")))
}

// JSONC mutation is implemented below with a small range-preserving parser.
fn update_jsonc_server(
    path: &Path,
    server: &Value,
    force: bool,
    label: &str,
    managed: fn(&Value) -> bool,
) -> Result<(), InstallError> {
    let existing = read_if_exists(path)?;
    let source = if existing.trim().is_empty() {
        "{}\n".to_owned()
    } else {
        existing
    };
    let root = parse_jsonc_object(path, &source, label)?;
    if root
        .get("mcpServers")
        .is_some_and(|value| !value.is_object())
    {
        return Err(InstallError::Message(format!(
            "Invalid mcpServers configuration in {}.",
            path.display()
        )));
    }
    let current = root
        .get("mcpServers")
        .and_then(Value::as_object)
        .and_then(|servers| servers.get("zvec_grep"));
    if current.is_some_and(|value| !managed(value)) && !force {
        return Err(InstallError::Message(format!(
            "Existing unmanaged zvec_grep MCP server found in {}. Re-run with --force to replace it for {label}.",
            path.display()
        )));
    }
    if current == Some(server) {
        return Ok(());
    }
    let next = jsonc_set_path(&source, &["mcpServers", "zvec_grep"], server)?;
    atomic_write(path, &ensure_newline(next))
}

fn remove_jsonc_server(
    path: &Path,
    label: &str,
    managed: fn(&Value) -> bool,
) -> Result<(), InstallError> {
    let source = read_if_exists(path)?;
    if source.trim().is_empty() {
        return Ok(());
    }
    let root = parse_jsonc_object(path, &source, label)?;
    let Some(servers) = root.get("mcpServers").and_then(Value::as_object) else {
        return Ok(());
    };
    if !servers.get("zvec_grep").is_some_and(managed) {
        return Ok(());
    }
    let path_to_remove: &[&str] = if servers.len() == 1 && !has_jsonc_comments(&source) {
        &["mcpServers"]
    } else {
        &["mcpServers", "zvec_grep"]
    };
    let next = jsonc_remove_path(&source, path_to_remove)?;
    if next != source {
        atomic_write(path, &ensure_newline(next))?;
    }
    Ok(())
}

fn preflight_jsonc_server(
    path: &Path,
    force: bool,
    label: &str,
    managed: fn(&Value) -> bool,
) -> Result<(), InstallError> {
    let source = read_if_exists(path)?;
    let root = parse_jsonc_object(
        path,
        if source.trim().is_empty() {
            "{}\n"
        } else {
            &source
        },
        label,
    )?;
    if root
        .get("mcpServers")
        .is_some_and(|value| !value.is_object())
    {
        return Err(InstallError::Message(format!(
            "Invalid mcpServers configuration in {}.",
            path.display()
        )));
    }
    let current = root
        .get("mcpServers")
        .and_then(Value::as_object)
        .and_then(|servers| servers.get("zvec_grep"));
    if current.is_some_and(|value| !managed(value)) && !force {
        return Err(InstallError::Message(format!(
            "Existing unmanaged zvec_grep MCP server found in {}. Re-run with --force to replace it for {label}.",
            path.display()
        )));
    }
    Ok(())
}

fn parse_jsonc_object(
    path: &Path,
    source: &str,
    label: &str,
) -> Result<Map<String, Value>, InstallError> {
    let stripped = strip_jsonc_comments(source)?;
    let value: Value = serde_json::from_str(&stripped).map_err(|_| {
        InstallError::Message(format!(
            "Invalid {label} configuration in {}.",
            path.display()
        ))
    })?;
    value.as_object().cloned().ok_or_else(|| {
        InstallError::Message(format!(
            "Invalid {label} configuration in {}.",
            path.display()
        ))
    })
}

fn strip_jsonc_comments(source: &str) -> Result<String, InstallError> {
    let bytes = source.as_bytes();
    let mut output = bytes.to_vec();
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            index += 1;
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'/') {
            output[index] = b' ';
            output[index + 1] = b' ';
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' && bytes[index] != b'\r' {
                output[index] = b' ';
                index += 1;
            }
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            output[index] = b' ';
            output[index + 1] = b' ';
            index += 2;
            let mut closed = false;
            while index < bytes.len() {
                if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    output[index] = b' ';
                    output[index + 1] = b' ';
                    index += 2;
                    closed = true;
                    break;
                }
                if bytes[index] != b'\n' && bytes[index] != b'\r' {
                    output[index] = b' ';
                }
                index += 1;
            }
            if !closed {
                return Err(InstallError::Message(
                    "Unterminated JSONC block comment".to_owned(),
                ));
            }
            continue;
        }
        index += 1;
    }
    String::from_utf8(output).map_err(|_| InstallError::Message("JSONC must be UTF-8".to_owned()))
}

fn has_jsonc_comments(source: &str) -> bool {
    strip_jsonc_comments(source).is_ok_and(|stripped| stripped != source)
}

fn ensure_newline(mut source: String) -> String {
    if !source.ends_with('\n') {
        source.push('\n');
    }
    source
}

fn jsonc_set_path(source: &str, path: &[&str], value: &Value) -> Result<String, InstallError> {
    crate::jsonc::set_path(source, path, value)
        .map_err(|error| InstallError::Message(error.to_string()))
}

fn jsonc_remove_path(source: &str, path: &[&str]) -> Result<String, InstallError> {
    crate::jsonc::remove_path(source, path)
        .map_err(|error| InstallError::Message(error.to_string()))
}

fn validate_qoder_permissions(path: &Path) -> Result<(), InstallError> {
    let source = read_if_exists(path)?;
    if source.trim().is_empty() {
        return Ok(());
    }
    let root = parse_jsonc_object(path, &source, "Qoder")?;
    qoder_allow_rules(path, &root).map(|_| ())
}

fn qoder_allow_rules(path: &Path, root: &Map<String, Value>) -> Result<Vec<String>, InstallError> {
    let permissions = match root.get("permissions") {
        None => return Ok(Vec::new()),
        Some(Value::Object(value)) => value,
        Some(_) => {
            return Err(InstallError::Message(format!(
                "Invalid permissions configuration in {}.",
                path.display()
            )));
        }
    };
    match permissions.get("allow") {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) if values.iter().all(Value::is_string) => Ok(values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()),
        Some(Value::Array(_)) => Err(InstallError::Message(format!(
            "Invalid permissions.allow rule in {}.",
            path.display()
        ))),
        Some(_) => Err(InstallError::Message(format!(
            "Invalid permissions.allow configuration in {}.",
            path.display()
        ))),
    }
}

fn update_qoder_cli(path: &Path, options: &AgentOptions) -> Result<(), InstallError> {
    let source = read_if_exists(path)?;
    let source = if source.trim().is_empty() {
        "{}\n".to_owned()
    } else {
        source
    };
    let root = parse_jsonc_object(path, &source, "Qoder")?;
    let allow = qoder_allow_rules(path, &root)?;
    let current = root
        .get("mcpServers")
        .and_then(Value::as_object)
        .and_then(|servers| servers.get("zvec_grep"));
    let current_managed = current.is_some_and(is_managed_json_server);
    let mut owned = BTreeSet::new();
    if current_managed {
        owned.extend(qoder_owned_permissions(current.unwrap_or(&Value::Null)));
    }
    for permission in [SEARCH_PERMISSION, RG_PERMISSION] {
        if !allow.iter().any(|value| value == permission) {
            owned.insert(permission.to_owned());
        }
    }
    let mut always = current
        .and_then(|server| server.get("alwaysAllow"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for tool in ["zvec_grep_search", "zvec_grep_rg"] {
        if !always.iter().any(|value| value == tool) {
            always.push(tool.to_owned());
        }
    }
    let timeout = options.timeout_seconds.saturating_mul(1000);
    let description = qoder_description(&owned);
    let mut server = match options.transport {
        McpInstallTransport::Stdio => {
            json!({"command":"zg", "args":stdio_args(options.toolset), "timeout":timeout, "trust":true, "description":description, "alwaysAllow":always})
        }
        McpInstallTransport::Http => {
            json!({"type":"http", "url":resolve_server_url()?, "timeout":timeout, "trust":true, "description":description, "alwaysAllow":always})
        }
    };
    if options.transport == McpInstallTransport::Http
        && let Some(token) = &options.token_env
    {
        server["headers"] = json!({"Authorization":format!("Bearer ${{{token}}}")});
    }
    let mut next = jsonc_set_path(&source, &["mcpServers", "zvec_grep"], &server)?;
    let missing = [SEARCH_PERMISSION, RG_PERMISSION]
        .into_iter()
        .filter(|permission| !allow.iter().any(|value| value == permission))
        .collect::<Vec<_>>();
    let has_allow_array = root
        .get("permissions")
        .and_then(Value::as_object)
        .and_then(|permissions| permissions.get("allow"))
        .is_some_and(Value::is_array);
    if has_allow_array {
        next =
            crate::jsonc::insert_array_strings_at_start(&next, &["permissions", "allow"], &missing)
                .map_err(|error| InstallError::Message(error.to_string()))?;
    } else if !missing.is_empty() {
        let mut updated = allow;
        updated.extend(missing.iter().map(|permission| (*permission).to_owned()));
        next = jsonc_set_path(&next, &["permissions", "allow"], &json!(updated))?;
    }
    atomic_write(path, &ensure_newline(next))
}

fn remove_qoder_cli(path: &Path) -> Result<(), InstallError> {
    let source = read_if_exists(path)?;
    if source.trim().is_empty() {
        return Ok(());
    }
    let root = parse_jsonc_object(path, &source, "Qoder")?;
    qoder_allow_rules(path, &root)?;
    let Some(current) = root
        .get("mcpServers")
        .and_then(Value::as_object)
        .and_then(|servers| servers.get("zvec_grep"))
    else {
        return Ok(());
    };
    if !is_managed_json_server(current) {
        return Ok(());
    }
    let owned = qoder_owned_permissions(current);
    let managed_path: &[&str] = if root
        .get("mcpServers")
        .and_then(Value::as_object)
        .is_some_and(|servers| servers.len() == 1)
        && !has_jsonc_comments(&source)
    {
        &["mcpServers"]
    } else {
        &["mcpServers", "zvec_grep"]
    };
    let mut next = jsonc_remove_path(&source, managed_path)?;
    let mut removed = 0;
    for permission in &owned {
        let (updated, did_remove) =
            crate::jsonc::remove_first_array_string(&next, &["permissions", "allow"], permission)
                .map_err(|error| InstallError::Message(error.to_string()))?;
        next = updated;
        removed += usize::from(did_remove);
    }
    let after = parse_jsonc_object(path, &next, "Qoder")?;
    let retained = qoder_allow_rules(path, &after)?;
    if removed > 0 && retained.is_empty() && !has_jsonc_comments(&next) {
        let root_after = parse_jsonc_object(path, &next, "Qoder")?;
        if root_after
            .get("permissions")
            .and_then(Value::as_object)
            .is_some_and(|permissions| permissions.len() == 1)
        {
            next = jsonc_remove_path(&next, &["permissions"])?;
        } else {
            next = jsonc_remove_path(&next, &["permissions", "allow"])?;
        }
    }
    atomic_write(path, &ensure_newline(next))
}

fn qoder_owned_permissions(server: &Value) -> BTreeSet<String> {
    let Some(description) = server.get("description").and_then(Value::as_str) else {
        return BTreeSet::new();
    };
    let Some(tools) = description.strip_prefix(QODER_OWNERSHIP_PREFIX) else {
        return BTreeSet::new();
    };
    tools
        .split(',')
        .filter_map(|tool| match tool {
            "zvec_grep_search" => Some(SEARCH_PERMISSION.to_owned()),
            "zvec_grep_rg" => Some(RG_PERMISSION.to_owned()),
            _ => None,
        })
        .collect()
}

fn qoder_description(owned: &BTreeSet<String>) -> String {
    let tools = [
        (SEARCH_PERMISSION, "zvec_grep_search"),
        (RG_PERMISSION, "zvec_grep_rg"),
    ]
    .into_iter()
    .filter(|(permission, _)| owned.contains(*permission))
    .map(|(_, tool)| tool)
    .collect::<Vec<_>>();
    if tools.is_empty() {
        QODER_DESCRIPTION.to_owned()
    } else {
        format!("{QODER_OWNERSHIP_PREFIX}{}", tools.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_aliases_match_typescript_order() {
        let agents = agents_from_tokens(&["cc,5,2".to_owned()], &BTreeSet::new()).expect("targets");
        assert_eq!(agents, vec![Agent::Claude, Agent::Codex, Agent::Qwen]);
        assert!(agents_from_tokens(&["qoder-ide".to_owned()], &BTreeSet::new()).is_err());
    }

    #[test]
    fn managed_stdio_shapes_are_recognized() {
        assert!(is_managed_json_server(
            &json!({"command":"zg","args":["server","--stdio"]})
        ));
        assert!(is_managed_json_server(
            &json!({"type":"local","command":["zg","server","--stdio"]})
        ));
        assert!(!is_managed_json_server(
            &json!({"command":"other","args":["server","--stdio"]})
        ));
        assert!(is_managed_qoder_ide(&json!({
            "command": "/usr/bin/node",
            "args": ["/package/dist/cli/index.js", "server", "--stdio"],
            "description": "Managed by zg install"
        })));
    }
}
