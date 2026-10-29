use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{
    InstallError, atomic_write, copilot_config_path, env_path, env_path_non_empty, grok_home,
    home_dir, jsonc_set_path, parse_json_object, parse_jsonc_object_options, qoder_ide_path,
    resolve_atomic_target, resolve_opencode_config, resolve_pi_home, resolve_qwen_home,
    vscode_config_paths,
};
use crate::InstallArgs;

struct ConfigFile {
    path: PathBuf,
    container: &'static str,
    format: Format,
}

#[derive(Clone, Copy)]
enum Format {
    Json,
    Jsonc,
    JsoncTrailing,
    Toml,
}

#[derive(Default)]
struct Report {
    updated: usize,
    compatible: usize,
    manual: usize,
    failed: usize,
}

enum Launch {
    Ignore,
    Compatible,
    Manual,
    Update {
        array: &'static str,
        action_index: usize,
        script: Option<String>,
        replace_executable: bool,
    },
}

/// Migrates existing launch configurations without installing integrations or starting a server.
///
/// # Errors
///
/// Reports unreadable, invalid or unsupported configurations after processing
/// all independent files. Each changed file is backed up before replacement.
pub fn execute_upgrade_installed(args: &InstallArgs) -> Result<(), InstallError> {
    writeln!(
        crate::stdout(),
        "Upgrading existing integrations{}",
        if args.dry_run { " (dry run)" } else { "" }
    )?;
    let mut report = Report::default();
    let mut visited = HashSet::new();
    for candidate in configuration_files() {
        let result = candidate.and_then(|file| {
            (|| {
                let target = resolve_atomic_target(&file.path, &mut HashSet::new())?;
                if !visited.insert((target.clone(), file.container)) {
                    return Ok(());
                }
                upgrade_file(&file, &target, args.dry_run, &mut report)
            })()
            .map_err(|error: InstallError| {
                InstallError::Message(format!("{}: {error}", file.path.display()))
            })
        });
        if let Err(error) = result {
            report.failed += 1;
            writeln!(crate::stdout(), "  failed: {error}")?;
        }
    }
    writeln!(
        crate::stdout(),
        "{}: {}, compatible: {}, manual: {}, failed: {}",
        if args.dry_run {
            "Would upgrade"
        } else {
            "Upgraded"
        },
        report.updated,
        report.compatible,
        report.manual,
        report.failed
    )?;
    if report.updated + report.compatible + report.manual + report.failed == 0 {
        writeln!(crate::stdout(), "No existing integrations found.")?;
    }
    if report.updated > 0 && !args.dry_run {
        writeln!(
            crate::stdout(),
            "Reconnect agents to use the updated configuration. No server was started."
        )?;
    }
    if report.failed > 0 || report.manual > 0 {
        return Err(InstallError::Message("Some configurations need attention; review the paths above and rerun zg --install --upgrade-installed.".to_owned()));
    }
    Ok(())
}

fn configuration_files() -> Vec<Result<ConfigFile, InstallError>> {
    let json = |path, container, format| {
        Ok(ConfigFile {
            path,
            container,
            format,
        })
    };
    let toml = |path| {
        Ok(ConfigFile {
            path,
            container: "mcp_servers",
            format: Format::Toml,
        })
    };
    let home = home_dir();
    let mut files = vec![
        toml(
            env_path("CODEX_HOME")
                .unwrap_or_else(|| home.join(".codex"))
                .join("config.toml"),
        ),
        toml(grok_home().join("config.toml")),
        json(
            env_path("CLAUDE_CONFIG_DIR")
                .map_or_else(|| home.join(".claude.json"), |dir| dir.join(".claude.json")),
            "mcpServers",
            Format::Json,
        ),
        json(
            env_path("CURSOR_CONFIG_DIR")
                .unwrap_or_else(|| home.join(".cursor"))
                .join("mcp.json"),
            "mcpServers",
            Format::Json,
        ),
        resolve_qwen_home().map(|path| ConfigFile {
            path: path.join("settings.json"),
            container: "mcpServers",
            format: Format::Jsonc,
        }),
        json(
            env_path_non_empty("QODER_CONFIG_DIR")
                .unwrap_or_else(|| home.join(".qoder"))
                .join("settings.json"),
            "mcpServers",
            Format::Jsonc,
        ),
        json(qoder_ide_path(), "mcpServers", Format::Jsonc),
        json(copilot_config_path(), "mcpServers", Format::Json),
        resolve_pi_home().map(|path| ConfigFile {
            path: path.join("mcp.json"),
            container: "mcpServers",
            format: Format::Json,
        }),
    ];
    files.extend(
        resolve_opencode_config()
            .cleanup_paths
            .into_iter()
            .map(|path| json(path, "mcp", Format::JsoncTrailing)),
    );
    files.extend(
        vscode_config_paths()
            .into_iter()
            .map(|path| json(path, "servers", Format::JsoncTrailing)),
    );
    files
}

fn upgrade_file(
    file: &ConfigFile,
    target: &Path,
    dry_run: bool,
    report: &mut Report,
) -> Result<(), InstallError> {
    let source = match fs::read_to_string(target) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if source.trim().is_empty() {
        return Ok(());
    }
    let (next, changes) = if matches!(file.format, Format::Toml) {
        upgrade_toml(&source, &file.path, report)?
    } else {
        upgrade_json(&source, file, report)?
    };
    if next != source {
        if !dry_run {
            write_upgrade(target, &source, &next)?;
        }
        report.updated += changes;
        writeln!(
            crate::stdout(),
            "  {}: {} ({changes} entries)",
            if dry_run { "would upgrade" } else { "upgraded" },
            file.path.display()
        )?;
    }
    Ok(())
}

fn write_upgrade(target: &Path, source: &str, next: &str) -> Result<(), InstallError> {
    if fs::read_to_string(target)? != source {
        return Err(InstallError::Message(
            "Configuration changed during migration; retry.".to_owned(),
        ));
    }
    let backup = target.with_file_name(format!(
        "{}.zg-upgrade-{}.bak",
        target.file_name().unwrap_or_default().to_string_lossy(),
        Uuid::new_v4()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup)?;
    file.set_permissions(fs::metadata(target)?.permissions())?;
    file.write_all(source.as_bytes())?;
    file.sync_all()?;
    if fs::read_to_string(target)? != source {
        return Err(InstallError::Message(
            "Configuration changed during backup; retry.".to_owned(),
        ));
    }
    atomic_write(target, next)?;
    writeln!(crate::stdout(), "  backup: {}", backup.display())?;
    Ok(())
}

fn executable_name(command: &str) -> &str {
    command.rsplit(['/', '\\']).next().unwrap_or(command)
}

fn zg_executable(command: &str) -> bool {
    matches!(
        executable_name(command),
        "zg" | "zg.exe" | "zg.cmd" | "zg.bat"
    )
}

fn known_stdio_args(args: &[Value]) -> bool {
    if !matches!(
        args.first().and_then(Value::as_str),
        Some("server" | "--server")
    ) || args.get(1).and_then(Value::as_str) != Some("--stdio")
    {
        return false;
    }
    let mut rest = &args[2..];
    while let [flag, value, tail @ ..] = rest {
        let Some(value) = value.as_str().filter(|value| !value.is_empty()) else {
            return false;
        };
        match flag.as_str() {
            Some("--mcp-toolset") if matches!(value, "agent" | "full") => {}
            Some("--listen" | "--home" | "--token-file") => {}
            _ => return false,
        }
        rest = tail;
    }
    rest.is_empty()
}

fn launch(name: &str, server: &Value) -> Launch {
    let named = matches!(name, "zvec_grep" | "zvec-grep");
    let Some(server) = server.as_object() else {
        return if named {
            Launch::Manual
        } else {
            Launch::Ignore
        };
    };
    let fallback = if named {
        Launch::Manual
    } else {
        Launch::Ignore
    };
    if server
        .get("url")
        .or_else(|| server.get("httpUrl"))
        .and_then(Value::as_str)
        .is_some()
    {
        return if named && !server.contains_key("command") {
            Launch::Compatible
        } else {
            fallback
        };
    }
    let (array, args, action_index, script, replace_executable) =
        if let Some(command) = server.get("command").and_then(Value::as_array) {
            let Some(executable) = command
                .first()
                .and_then(Value::as_str)
                .filter(|command| zg_executable(command))
            else {
                return fallback;
            };
            (
                "command",
                &command[1..],
                1,
                None,
                executable_name(executable) != executable,
            )
        } else {
            let Some(command) = server.get("command").and_then(Value::as_str) else {
                return fallback;
            };
            let Some(args) = server.get("args").and_then(Value::as_array) else {
                return fallback;
            };
            if zg_executable(command) {
                (
                    "args",
                    args.as_slice(),
                    0,
                    None,
                    executable_name(command) != command,
                )
            } else if matches!(executable_name(command), "node" | "node.exe" | "nodejs") {
                let Some(script) = args.first().and_then(Value::as_str) else {
                    return fallback;
                };
                let normalized = script.replace('\\', "/");
                let owned = matches!(
                    server.get("description").and_then(Value::as_str),
                    Some("Managed by zg install" | "Managed by zg --install")
                );
                let package_entry = normalized.ends_with("/dist/cli/index.js")
                    && (owned || normalized.ends_with("/@zvec/zvec-grep/dist/cli/index.js"));
                // Older installers persisted argv[1], which may be the npm bin symlink.
                let managed_bin = owned && zg_executable(script);
                if !package_entry && !managed_bin {
                    return fallback;
                }
                ("args", &args[1..], 1, Some(script.to_owned()), true)
            } else {
                return fallback;
            }
        };
    if args.first().and_then(Value::as_str) == Some("--server")
        && args.get(1).and_then(Value::as_str) == Some("--stdio")
        && script.is_none()
    {
        return Launch::Compatible;
    }
    if !known_stdio_args(args) {
        return Launch::Manual;
    }
    Launch::Update {
        array,
        action_index,
        script,
        replace_executable,
    }
}

fn note_launch(
    path: &Path,
    name: &str,
    launch: &Launch,
    report: &mut Report,
) -> Result<(), InstallError> {
    match launch {
        Launch::Compatible => {
            report.compatible += 1;
            writeln!(crate::stdout(), "  compatible: {} [{name}]", path.display())?;
        }
        Launch::Manual => {
            report.manual += 1;
            writeln!(
                crate::stdout(),
                "  manual: {} [{name}] has an unsupported launcher; use zg --server --stdio for stdio MCP",
                path.display()
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn upgrade_json(
    source: &str,
    file: &ConfigFile,
    report: &mut Report,
) -> Result<(String, usize), InstallError> {
    let root = read_json_config(source, file)?;
    let Some(container) = root.get(file.container) else {
        return Ok((source.to_owned(), 0));
    };
    let servers = container.as_object().ok_or_else(|| {
        InstallError::Message(format!("Expected {} to be an object", file.container))
    })?;
    let mut next = source.to_owned();
    let mut upgraded = Vec::new();
    for (name, server) in servers {
        let launch = launch(name, server);
        note_launch(&file.path, name, &launch, report)?;
        if let Launch::Update {
            array,
            action_index,
            script,
            replace_executable,
        } = launch
        {
            let path = &[file.container, name.as_str(), array];
            next = crate::jsonc::replace_array_string(&next, path, action_index, "--server")
                .map_err(|error| InstallError::Message(error.to_string()))?;
            if let Some(script) = script {
                next = crate::jsonc::remove_first_array_string(&next, path, &script)
                    .map_err(|error| InstallError::Message(error.to_string()))?
                    .0;
            }
            if replace_executable {
                let executable = env::current_exe()?.to_string_lossy().into_owned();
                next = if array == "command" {
                    crate::jsonc::replace_array_string(&next, path, 0, &executable)
                        .map_err(|error| InstallError::Message(error.to_string()))?
                } else {
                    jsonc_set_path(
                        &next,
                        &[file.container, name, "command"],
                        &json!(executable),
                    )?
                };
            }
            upgraded.push((name, replace_executable));
        }
    }
    if !upgraded.is_empty() {
        let updated = read_json_config(&next, file)?;
        for (name, replace_executable) in &upgraded {
            let server = &updated[file.container][name.as_str()];
            let executable = server.get("command").and_then(|command| {
                command
                    .as_str()
                    .or_else(|| command.as_array()?.first()?.as_str())
            });
            let executable_matches = !replace_executable
                || executable == Some(env::current_exe()?.to_string_lossy().as_ref());
            if !executable_matches || !matches!(launch(name, server), Launch::Compatible) {
                return Err(InstallError::Message(
                    "Ambiguous configuration properties; original file left unchanged.".to_owned(),
                ));
            }
        }
    }
    Ok((next, upgraded.len()))
}

fn read_json_config(source: &str, file: &ConfigFile) -> Result<Map<String, Value>, InstallError> {
    match file.format {
        Format::Json => parse_json_object(&file.path, source),
        _ => parse_jsonc_object_options(
            &file.path,
            source,
            "agent",
            matches!(file.format, Format::JsoncTrailing),
        ),
    }
}

#[derive(Deserialize)]
struct TomlConfig {
    #[serde(default)]
    mcp_servers: BTreeMap<String, TomlServer>,
}

#[derive(Deserialize)]
struct TomlServer {
    command: Option<toml::Spanned<String>>,
    args: Option<Vec<toml::Spanned<String>>>,
    url: Option<String>,
    description: Option<String>,
}

fn upgrade_toml(
    source: &str,
    path: &Path,
    report: &mut Report,
) -> Result<(String, usize), InstallError> {
    let root: TomlConfig = toml::from_str(source)
        .map_err(|_| InstallError::Message("Invalid TOML configuration".to_owned()))?;
    let mut edits = Vec::new();
    let mut changes = 0;
    for (name, server) in root.mcp_servers {
        let mut value = Map::new();
        if let Some(command) = &server.command {
            value.insert("command".to_owned(), json!(command.get_ref()));
        }
        if let Some(args) = &server.args {
            value.insert(
                "args".to_owned(),
                json!(args.iter().map(toml::Spanned::get_ref).collect::<Vec<_>>()),
            );
        }
        if let Some(url) = &server.url {
            value.insert("url".to_owned(), json!(url));
        }
        if let Some(description) = &server.description {
            value.insert("description".to_owned(), json!(description));
        }
        let launch = launch(&name, &Value::Object(value));
        note_launch(path, &name, &launch, report)?;
        if let Launch::Update {
            action_index,
            script,
            replace_executable,
            ..
        } = launch
        {
            changes += 1;
            let args = server.args.as_ref().expect("validated launch arguments");
            edits.push((args[action_index].span(), "\"--server\"".to_owned()));
            if script.is_some() {
                let first = args[0].span();
                let between = first.end..args[1].span().start;
                let mut comment = false;
                let comma = source[between.clone()]
                    .char_indices()
                    .find_map(|(offset, character)| {
                        match character {
                            '\n' => comment = false,
                            '#' => comment = true,
                            ',' if !comment => return Some(between.start + offset),
                            _ => {}
                        }
                        None
                    })
                    .expect("validated array separator");
                edits.push((first, String::new()));
                edits.push((comma..comma + 1, String::new()));
            }
            if replace_executable {
                edits.push((
                    server
                        .command
                        .as_ref()
                        .expect("validated launch command")
                        .span(),
                    toml::Value::String(env::current_exe()?.to_string_lossy().into_owned())
                        .to_string(),
                ));
            }
        }
    }
    edits.sort_by_key(|(span, _)| std::cmp::Reverse(span.start));
    let mut next = source.to_owned();
    for (span, replacement) in edits {
        next.replace_range(span, &replacement);
    }
    Ok((next, changes))
}
