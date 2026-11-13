use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

use zg_cli::{
    ColorMode, IntegrationLaunch, IntegrationUpgradeEntryKind as Kind, IntegrationUpgradeReport,
    MessageKind, StandaloneArgs,
};
use zg_daemon::ExternalDaemon;

use super::{Receipt, binary};

pub(super) fn complete(
    output: &mut impl Write,
    args: &StandaloneArgs,
    receipt: Option<&Receipt>,
    current: &Path,
    external: Option<&ExternalDaemon>,
) -> io::Result<()> {
    let Some(receipt) = receipt else {
        writeln!(
            output,
            "Removed standalone binaries. Configuration, models and indexes were preserved."
        )?;
        if external.is_some() {
            writeln!(output, "! External daemon is still running.")?;
        }
        return Ok(());
    };
    zg_cli::write_message(
        &mut *output,
        MessageKind::Success,
        &format!("Installed zvec-grep {}", receipt.version),
        &[],
        ColorMode::Auto,
        zg_cli::stdout_supports_ansi(),
    )?;
    if external.is_some() {
        writeln!(
            output,
            "! External daemon is still running. Runtime switch is pending."
        )?;
    }
    let launcher = receipt.launcher.clone().unwrap_or_else(|| binary(current));
    let report = if args.skip_agent_upgrade {
        writeln!(output, "Agent configuration migration skipped.")?;
        None
    } else {
        if external.is_some() {
            writeln!(
                output,
                "! PREVIEW ONLY — no agent configuration files were changed."
            )?;
        }
        match zg_cli::upgrade_installed(Some(&launcher), external.is_some()) {
            Ok(report) => {
                write_migration(output, &report, external.is_some())?;
                Some(report)
            }
            Err(error) => {
                zg_cli::write_integration_upgrade_entry(
                    &mut *output,
                    Kind::Failed,
                    &error.to_string(),
                    ColorMode::Auto,
                    zg_cli::stdout_supports_ansi(),
                )?;
                writeln!(output, "Retry agent migration:")?;
                writeln!(
                    output,
                    "  {}",
                    migration_command(&launcher, external.is_some())
                )?;
                None
            }
        }
    };
    if let Some(external) = external {
        write_manual_switch(output, args, receipt, &launcher, external, report.as_ref())?;
    } else {
        writeln!(output, "Reopen your agents to use the installed version.")?;
    }
    if receipt.accelerator_actual == "cuda" {
        writeln!(
            output,
            "CUDA plugins require compatible NVIDIA drivers and CUDA/cuBLAS/cuDNN host libraries."
        )?;
    }
    Ok(())
}

fn write_migration(
    output: &mut impl Write,
    report: &IntegrationUpgradeReport,
    preview: bool,
) -> io::Result<()> {
    let counts = report.summary;
    writeln!(
        output,
        "  {} entries {} · {} need no edits",
        counts.updated,
        if preview {
            "need migration"
        } else {
            "migrated"
        },
        counts.compatible
    )?;
    for entry in &report.entries {
        if matches!(entry.kind, Kind::Manual | Kind::Failed) {
            zg_cli::write_integration_upgrade_entry(
                &mut *output,
                entry.kind,
                &entry.detail,
                ColorMode::Auto,
                zg_cli::stdout_supports_ansi(),
            )?;
        }
    }
    if counts.manual + counts.failed > 0 {
        writeln!(
            output,
            "! Agent configuration migration needs attention: {} manual · {} failed.",
            counts.manual, counts.failed
        )?;
    }
    Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.is_absolute()
        && right.is_absolute()
        && (left == right
            || left
                .canonicalize()
                .ok()
                .zip(right.canonicalize().ok())
                .is_some_and(|(left, right)| left == right))
}

fn stdio_can_restart(
    report: &IntegrationUpgradeReport,
    daemon_home: &Path,
    required_token: Option<&Path>,
    external: &ExternalDaemon,
) -> bool {
    if report.summary.failed + report.summary.manual > 0 || report.launches.is_empty() {
        return false;
    }
    // Defaults come from the user's persisted configuration, not this installer's --home.
    let default_home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".zvec-grep"));
    let default_listen = zg_cli::resolve_server_listen().ok();
    report.launches.iter().all(|launch| {
        let IntegrationLaunch::Stdio {
            home,
            listen,
            toolset,
            token_file,
            token_environment,
        } = launch
        else {
            return false;
        };
        let address_matches = listen
            .as_ref()
            .or(default_listen.as_ref())
            .and_then(|value| value.parse::<zg_daemon::ListenAddress>().ok())
            .zip(external.listen.parse::<zg_daemon::ListenAddress>().ok())
            .is_some_and(|(left, right)| left == right);
        home.as_ref()
            .or(default_home.as_ref())
            .is_some_and(|candidate| same_path(candidate, daemon_home))
            && address_matches
            && toolset.as_deref().unwrap_or("agent") == external.mcp_toolset
            && required_token.is_none_or(|required| {
                *token_environment
                    || token_file
                        .as_ref()
                        .is_some_and(|path| same_path(path, required))
            })
    })
}

fn write_manual_switch(
    output: &mut impl Write,
    args: &StandaloneArgs,
    receipt: &Receipt,
    launcher: &Path,
    external: &ExternalDaemon,
    report: Option<&IntegrationUpgradeReport>,
) -> io::Result<()> {
    let automatic = report.is_some_and(|report| {
        stdio_can_restart(report, &receipt.home, args.token_file.as_deref(), external)
    });
    let home = receipt.home.to_string_lossy();
    let token_file = args.token_file.as_ref().map(|path| path.to_string_lossy());
    let mut stop = vec!["--server", "off", "--home", home.as_ref()];
    let mut start = vec!["--server", "on", "--home", home.as_ref()];
    if !external.listen.is_empty() {
        start.extend(["--listen", &external.listen]);
    }
    if matches!(external.mcp_toolset.as_str(), "agent" | "full") {
        start.extend(["--mcp-toolset", &external.mcp_toolset]);
    }
    if let Some(token_file) = &token_file {
        stop.extend(["--token-file", token_file]);
        start.extend(["--token-file", token_file]);
    }
    writeln!(output, "Close all agents/MCP clients using zg, then run:")?;
    let mut commands = vec![
        migration_command(launcher, false),
        shell_command(launcher, &stop),
    ];
    if !automatic {
        commands.push(shell_command(launcher, &start));
    }
    write_commands(output, &commands, cfg!(windows))?;
    if automatic {
        writeln!(
            output,
            "Reopen your agents. stdio MCP will start the new daemon automatically."
        )?;
    } else {
        writeln!(
            output,
            "An explicit start preserves the daemon's listen/toolset settings for HTTP clients or incomplete stdio configurations."
        )?;
        writeln!(output, "Reopen your agents after all commands succeed.")?;
    }
    writeln!(
        output,
        "For authenticated daemons, keep the original token environment or --token-file when switching and reopening agents."
    )?;
    writeln!(output, "To inspect every configuration entry, run:")?;
    writeln!(output, "  {}", migration_command(launcher, true))
}

fn write_commands(
    output: &mut impl Write,
    commands: &[String],
    powershell: bool,
) -> io::Result<()> {
    if powershell {
        // Windows PowerShell 5 has no &&; check each native exit code before proceeding.
        writeln!(output, "  {}", commands[0])?;
        for (index, command) in commands[1..].iter().enumerate() {
            writeln!(
                output,
                "{}if ($LASTEXITCODE -eq 0) {{",
                "  ".repeat(index + 1)
            )?;
            writeln!(output, "{}{}", "  ".repeat(index + 2), command)?;
        }
        for index in (1..commands.len()).rev() {
            writeln!(output, "{}}}", "  ".repeat(index))?;
        }
    } else {
        for (index, command) in commands.iter().enumerate() {
            writeln!(
                output,
                "  {command}{}",
                if index + 1 == commands.len() {
                    ""
                } else {
                    " &&"
                }
            )?;
        }
    }
    Ok(())
}

fn quote(word: &str) -> String {
    format!(
        "'{}'",
        if cfg!(windows) {
            word.replace('\'', "''")
        } else {
            word.replace('\'', "'\\''")
        }
    )
}

fn shell_command(executable: &Path, arguments: &[&str]) -> String {
    let mut words = vec![quote(&executable.to_string_lossy())];
    for (index, word) in arguments.iter().enumerate() {
        let path = index > 0
            && matches!(
                arguments[index - 1],
                "--launcher" | "--home" | "--token-file"
            );
        words.push(
            if path
                || !word.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "-_./:[]".contains(character)
                })
            {
                quote(word)
            } else {
                (*word).to_owned()
            },
        );
    }
    format!(
        "{}{}",
        if cfg!(windows) { "& " } else { "" },
        words.join(" ")
    )
}

fn migration_command(launcher: &Path, preview: bool) -> String {
    let launcher_path = launcher.to_string_lossy();
    let mut arguments = vec![
        "--install",
        "--upgrade-installed",
        "--launcher",
        &launcher_path,
    ];
    if preview {
        arguments.push("--dry-run");
    }
    shell_command(launcher, &arguments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zg_cli::{IntegrationUpgradeEntry, IntegrationUpgradeSummary};

    #[test]
    fn automatic_start_requires_matching_stdio_settings_and_no_http_clients() {
        let root = tempfile::tempdir().expect("home");
        let home = root.path();
        let external = ExternalDaemon {
            executable: home.join("external/zg"),
            listen: "127.0.0.1:9123".to_owned(),
            mcp_toolset: "full".to_owned(),
        };
        let mut report = IntegrationUpgradeReport {
            launches: vec![IntegrationLaunch::Stdio {
                home: Some(home.to_owned()),
                listen: Some(external.listen.clone()),
                toolset: Some("full".to_owned()),
                token_file: None,
                token_environment: false,
            }],
            ..Default::default()
        };
        assert!(stdio_can_restart(&report, home, None, &external));
        assert!(!stdio_can_restart(
            &report,
            home,
            Some(&home.join("token")),
            &external
        ));
        report.launches.push(IntegrationLaunch::Http);
        assert!(!stdio_can_restart(&report, home, None, &external));
        report.launches.pop();
        if let IntegrationLaunch::Stdio { toolset, .. } = &mut report.launches[0] {
            *toolset = None;
        }
        assert!(!stdio_can_restart(&report, home, None, &external));
    }

    #[test]
    fn compact_preview_only_shows_counts_and_actionable_details() {
        let report = IntegrationUpgradeReport {
            summary: IntegrationUpgradeSummary {
                updated: 4,
                compatible: 2,
                manual: 1,
                failed: 1,
            },
            entries: vec![
                IntegrationUpgradeEntry {
                    kind: Kind::Preview,
                    detail: "hidden-preview-path".to_owned(),
                },
                IntegrationUpgradeEntry {
                    kind: Kind::Compatible,
                    detail: "hidden-compatible-path".to_owned(),
                },
                IntegrationUpgradeEntry {
                    kind: Kind::Backup,
                    detail: "hidden-backup-path".to_owned(),
                },
                IntegrationUpgradeEntry {
                    kind: Kind::Manual,
                    detail: "manual-path: unsupported wrapper".to_owned(),
                },
                IntegrationUpgradeEntry {
                    kind: Kind::Failed,
                    detail: "failed-path: invalid JSON".to_owned(),
                },
            ],
            launches: vec![],
        };
        let mut output = Vec::new();
        write_migration(&mut output, &report, true).expect("summary");
        let output = String::from_utf8(output).expect("UTF-8");
        assert!(output.contains("4 entries need migration · 2 need no edits"));
        assert!(!output.contains("hidden-"));
        assert!(output.contains("manual-path: unsupported wrapper"));
        assert!(output.contains("failed-path: invalid JSON"));
        assert!(!output.contains("entries migrated"));
    }

    #[test]
    fn commands_quote_paths_and_guard_every_transition() {
        let launcher = Path::new("directory with ' quote/zg");
        let command = migration_command(launcher, false);
        assert!(command.contains(" --install --upgrade-installed --launcher "));
        assert!(!command.contains("'--install'"));
        let mut posix = Vec::new();
        write_commands(
            &mut posix,
            &["migrate".to_owned(), "stop".to_owned()],
            false,
        )
        .expect("commands");
        assert_eq!(
            String::from_utf8(posix).expect("UTF-8"),
            "  migrate &&\n  stop\n"
        );
        let mut powershell = Vec::new();
        write_commands(
            &mut powershell,
            &["migrate".to_owned(), "stop".to_owned(), "start".to_owned()],
            true,
        )
        .expect("commands");
        assert_eq!(
            String::from_utf8(powershell).expect("UTF-8"),
            "  migrate\n  if ($LASTEXITCODE -eq 0) {\n    stop\n    if ($LASTEXITCODE -eq 0) {\n      start\n    }\n  }\n"
        );
    }
}
