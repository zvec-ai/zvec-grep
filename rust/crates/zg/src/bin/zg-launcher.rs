//! Stable native entrypoint for installations that cannot use command symlinks.

use std::{
    env, fs, io,
    path::Component,
    process::{Command, ExitCode, Stdio},
};

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let executable = env::current_exe()?;
    let root = executable
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or_else(|| io::Error::other("invalid standalone launcher location"))?;
    if fs::read_to_string(root.join(".zg-standalone"))? != "zvec-grep standalone v2\n" {
        return Err(io::Error::other("unrecognized standalone installation").into());
    }
    let selection = fs::read_to_string(root.join("current"))?;
    let bundle = selection.trim();
    let relative = std::path::Path::new(bundle);
    if relative.components().count() != 2
        || !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        || bundle.contains('\\')
        || bundle.contains(':')
    {
        return Err(io::Error::other("invalid active version path").into());
    }
    let target = root
        .join(relative)
        .join("bin")
        .join(if cfg!(windows) { "zg.exe" } else { "zg" })
        .canonicalize()?;
    if !target.starts_with(root.canonicalize()?) || target == executable.canonicalize()? {
        return Err(io::Error::other("active version escapes the installation").into());
    }
    let status = Command::new(target)
        .args(env::args_os().skip(1))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    // Exit with the child's status rather than exposing an intermediate wrapper status.
    std::process::exit(status.code().unwrap_or(1));
}

fn main() -> ExitCode {
    match run() {
        Ok(status) => status,
        Err(error) => {
            eprintln!("zvec-grep launcher: {error}");
            ExitCode::FAILURE
        }
    }
}
