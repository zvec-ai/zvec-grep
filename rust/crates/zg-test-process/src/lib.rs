//! Child commands for tests that run alongside native storage handles.

use std::{ffi::OsStr, process::Command};

/// Creates a test command that inherits only its configured standard streams.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    prevent_descriptor_inheritance(&mut command);
    command
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn prevent_descriptor_inheritance(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // Native libraries may open files without CLOEXEC. Set it in the child,
    // after fork, so concurrent opens in the parent cannot race with this scan.
    // SAFETY: close_fds is async-signal-safe on our Unix targets. The callback
    // allocates nothing and takes no locks. Marking descriptors instead of
    // closing them also preserves the standard library's exec-error pipe.
    unsafe {
        command.pre_exec(|| {
            close_fds::set_fds_cloexec(3, &[]);
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn prevent_descriptor_inheritance(_command: &mut Command) {
    // Windows Command restricts inherited handles to the configured stdio.
}

#[cfg(test)]
mod tests {
    #[test]
    fn failed_exec_reports_the_error_to_the_parent() {
        let directory = tempfile::tempdir().expect("empty directory");
        let error = super::command(directory.path().join("missing-executable"))
            .spawn()
            .expect_err("missing executable must not appear to start successfully");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}
