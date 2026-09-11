use std::{
    net::TcpListener,
    process::{Command, Output},
};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    state: TempDir,
}
impl Fixture {
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_zg"))
            .current_dir(self.root.path())
            .env("HOME", self.state.path())
            .env("USERPROFILE", self.state.path())
            .env("ZVEC_GREP_HOME", self.state.path().join("runtime"))
            .env_remove("ZVEC_GREP_SERVER_TOKEN")
            .env_remove("ZVEC_GREP_SERVER_TOKEN_FILE")
            .env_remove("ZVEC_GREP_EMBEDDING")
            .env_remove("ZVEC_GREP_DEVICE")
            .args(args)
            .output()
            .expect("run command")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.run(&["server", "off"]);
    }
}

#[test]
fn direct_and_server_report_index_progress_on_stderr() {
    let fixture = Fixture {
        root: TempDir::new().expect("workspace"),
        state: TempDir::new().expect("state"),
    };
    let socket = TcpListener::bind("127.0.0.1:0").expect("port");
    let address = socket.local_addr().expect("address").to_string();
    drop(socket);
    let started = fixture.run(&["server", "on", "--listen", &address]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    for mode in ["direct", "server"] {
        let output = fixture.run(&[
            "index",
            "--mode",
            mode,
            "--embedding",
            "local/potion-code-16m-v2",
            "--device",
            "cpu",
            "--no-color",
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("Index complete"), "{mode}: {stderr}");
        assert!(!stderr.contains('\r') && !stderr.contains("\x1b["));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Workspace index: ready"));
        assert!(!stdout.contains("Scanning") && !stdout.contains("Downloading"));
    }
    let failed = fixture.run(&[
        "index",
        "--mode",
        "server",
        "--rebuild",
        "--embedding",
        "unsupported/model",
    ]);
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stdout).contains("Workspace index: ready"));
}
