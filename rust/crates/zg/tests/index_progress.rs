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
    fn start_server(&self) {
        let socket = TcpListener::bind("127.0.0.1:0").expect("port");
        let address = socket.local_addr().expect("address").to_string();
        drop(socket);
        let started = self.run(&["--server", "on", "--listen", &address]);
        assert!(
            started.status.success(),
            "{}",
            String::from_utf8_lossy(&started.stderr)
        );
    }

    fn success(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("UTF-8 output")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_zg"))
            .current_dir(self.root.path())
            .env("HOME", self.state.path())
            .env("USERPROFILE", self.state.path())
            .env("ZVEC_GREP_HOME", self.state.path().join("runtime"))
            .env("ZVEC_GREP_CONFIG", self.state.path().join("config.json"))
            .env(
                "ZVEC_GREP_WORKSPACE_REGISTRY",
                self.state.path().join("workspaces.json"),
            )
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
        self.run(&["--server", "off"]);
    }
}

#[test]
fn direct_and_server_report_index_progress_on_stderr() {
    let fixture = Fixture {
        root: TempDir::new().expect("workspace"),
        state: TempDir::new().expect("state"),
    };
    fixture.start_server();
    for mode in ["direct", "server"] {
        let output = fixture.run(&[
            "--index",
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
        assert!(stdout.starts_with("Workspace index\nfiles\t"));
        assert!(stdout.contains(" retried, "));
        assert!(stdout.contains("\nentities\t"));
        assert!(stdout.contains("\nduration\t"));
        assert!(stdout.contains("\nroots\t"));
        assert!(!stdout.contains("Scanning") && !stdout.contains("Downloading"));
    }
    let failed = fixture.run(&[
        "--index",
        "--mode",
        "server",
        "--rebuild",
        "--embedding",
        "unsupported/model",
    ]);
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stdout).contains("Workspace index\n"));
}

fn roots_line(output: &str) -> &str {
    output
        .lines()
        .find(|line| line.starts_with("roots\t"))
        .expect("roots summary")
}

#[test]
fn index_summary_uses_saved_scope_in_direct_and_server_modes() {
    let fixture = Fixture {
        root: TempDir::new().expect("workspace"),
        state: TempDir::new().expect("state"),
    };
    fixture.start_server();
    for mode in ["direct", "server"] {
        // Empty directories avoid model downloads while exercising saved scope.
        let initial = fixture.success(&[
            "--index",
            "--mode",
            mode,
            "--no-color",
            "--embedding",
            "local/potion-code-16m-v2",
            "--device",
            "cpu",
            "--glob",
            "*.rs",
            "--iglob",
            "!vendor/**",
            "--max-depth",
            "2",
            "--max-filesize",
            "4096",
            "--hidden",
            "--no-ignore",
            "--follow",
            "--nested-git=false",
        ]);
        let initial_roots = roots_line(&initial);
        assert!(initial_roots.ends_with(" (glob=*.rs iglob=!vendor/** hidden no-ignore max-depth=2 max-filesize=4096 follow nested-git=false)"), "{mode}: {initial}");

        let retained = fixture.success(&["--index", "--mode", mode, "--no-color"]);
        assert_eq!(
            roots_line(&retained),
            initial_roots,
            "{mode}: omitted options preserve the saved scope and owning root"
        );

        let updated =
            fixture.success(&["--index", "--mode", mode, "--no-color", "--max-depth", "1"]);
        assert_eq!(
            roots_line(&updated),
            initial_roots.replace("max-depth=2", "max-depth=1"),
            "{mode}: partial updates retain other rules"
        );

        let reset = fixture.success(&["--index", "--mode", mode, "--no-color", "--reset-paths"]);
        let base_root = initial_roots.split_once(" (").expect("filtered roots").0;
        assert_eq!(
            roots_line(&reset),
            base_root,
            "{mode}: reset removes saved filters"
        );
    }
}

fn manifest_without_update_time(path: &std::path::Path) -> serde_json::Value {
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).expect("parent manifest"))
            .expect("manifest JSON");
    // A resident parent watcher can refresh its timestamp after child filesystem events.
    manifest
        .as_object_mut()
        .expect("manifest object")
        .remove("updatedTime");
    manifest
}

#[test]
fn explicit_nested_index_owns_its_scope_in_direct_and_server_modes() {
    for mode in ["direct", "server"] {
        let fixture = Fixture {
            root: TempDir::new().expect("workspace"),
            state: TempDir::new().expect("state"),
        };
        let nested = fixture.root.path().join("nested");
        std::fs::create_dir(&nested).expect("nested directory");
        if mode == "server" {
            fixture.start_server();
        }
        fixture.success(&[
            "--index",
            "--mode",
            mode,
            "--no-color",
            "--embedding",
            "local/potion-code-16m-v2",
            "--device",
            "cpu",
            "--glob",
            "*.rs",
        ]);
        let parent_manifest = fixture.root.path().join(".zvec-grep/manifest.json");
        let parent_before = manifest_without_update_time(&parent_manifest);
        let child = fixture.success(&[
            "--index",
            nested.to_str().expect("nested path"),
            "--mode",
            mode,
            "--no-color",
            "--embedding",
            "local/potion-retrieval-32m",
            "--device",
            "cpu",
        ]);
        let child_root = std::fs::canonicalize(&nested).expect("child root");
        assert_eq!(
            roots_line(&child),
            format!("roots\t{}", child_root.display()),
            "{mode}: child uses its own root and scan scope"
        );
        assert!(nested.join(".zvec-grep/manifest.json").exists());
        assert_eq!(
            manifest_without_update_time(&parent_manifest),
            parent_before,
            "{mode}: child indexing preserves the parent manifest"
        );
    }
}
