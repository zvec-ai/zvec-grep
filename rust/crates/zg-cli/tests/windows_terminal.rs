#![cfg(windows)]

use std::{
    env,
    fs::OpenOptions,
    io,
    os::windows::process::CommandExt,
    process::{Command, Stdio},
};

use winapi_util::{HandleRef, console};

const TEST_ROLE: &str = "ZG_WINDOWS_TERMINAL_TEST_ROLE";
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

#[test]
fn windows_ansi_initialization_uses_each_output_stream() -> io::Result<()> {
    match env::var(TEST_ROLE).as_deref() {
        Ok("console") => run_console_workers(),
        Ok("stdout") => check_console_stream(true),
        Ok("stderr") => check_console_stream(false),
        _ => {
            // Own a separate console even when the test runner has redirected stdio.
            let status = test_command()?
                .env(TEST_ROLE, "console")
                .creation_flags(CREATE_NEW_CONSOLE)
                .status()?;
            assert!(status.success(), "isolated console child failed: {status}");
            Ok(())
        }
    }
}

fn test_command() -> io::Result<Command> {
    let mut command = Command::new(env::current_exe()?);
    command.args([
        "--exact",
        "windows_ansi_initialization_uses_each_output_stream",
        "--nocapture",
        "--test-threads=1",
    ]);
    Ok(command)
}

fn run_console_workers() -> io::Result<()> {
    let console = OpenOptions::new().read(true).write(true).open("CONOUT$")?;
    for role in ["stdout", "stderr"] {
        let mut command = test_command()?;
        command
            .env(TEST_ROLE, role)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Explicit console handles are needed: CREATE_NEW_CONSOLE can retain pipes.
        let stream = Stdio::from(console.try_clone()?);
        if role == "stdout" {
            command.stdout(stream);
        } else {
            command.stderr(stream);
        }
        let output = command.output()?;
        assert!(
            output.status.success(),
            "{role} worker failed: {}\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn check_console_stream(stdout: bool) -> io::Result<()> {
    let handle = if stdout {
        HandleRef::stdout()
    } else {
        HandleRef::stderr()
    };
    let console_supports_ansi = || {
        if stdout {
            zg_cli::stdout_supports_ansi()
        } else {
            zg_cli::stderr_supports_ansi()
        }
    };
    let original = console::mode(&handle)?;
    let disabled = original & !ENABLE_VIRTUAL_TERMINAL_PROCESSING;
    console::set_mode(&handle, disabled)?;
    let pipe_supports_ansi = if stdout {
        zg_cli::stderr_supports_ansi()
    } else {
        zg_cli::stdout_supports_ansi()
    };
    assert!(!pipe_supports_ansi, "a pipe must not enable automatic ANSI");
    assert_eq!(console::mode(&handle)?, disabled);
    assert!(console_supports_ansi(), "console VT initialization failed");
    let enabled = disabled | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
    assert_eq!(console::mode(&handle)?, enabled, "other flags changed");
    assert!(console_supports_ansi());
    assert_eq!(console::mode(&handle)?, enabled);
    console::set_mode(&handle, original)
}
