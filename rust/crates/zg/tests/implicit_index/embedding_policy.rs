use super::*;

const CODE: &str = "local/potion-code-16m-v2";
const TEXT: &str = "local/bekko-256-int8";

fn weighted_workspace(fixture: &Fixture) {
    // Nonempty whitespace participates in discovery but produces no embedding
    // fragments, keeping these full CLI tests independent of model downloads.
    fs::write(fixture.root.path().join("large.rs"), " ".repeat(512 * 1024)).expect("code");
    for path in ["one.md", "two.md"] {
        fs::write(fixture.root.path().join(path), "\n".repeat(48 * 1024)).expect("text");
    }
}

fn assert_no_download(fixture: &Fixture) {
    let cache = fixture.user.path().join("models");
    assert!(!cache.exists() || fs::read_dir(cache).expect("model cache").next().is_none());
}

#[test]
fn capped_weights_and_rebuild_choices_match_in_direct_and_server_modes() {
    for mode in ["direct", "server"] {
        let mut fixture = Fixture::new();
        if mode == "server" {
            fixture.start_server();
        }
        weighted_workspace(&fixture);
        let output = fixture.success(&["--index", "--mode", mode, "--color", "never"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Workspace: text-dominant (code 40%, text 60%"),
            "{mode}: {stderr}"
        );
        assert!(
            !stderr.contains("Basis:") && !stderr.contains("Files:"),
            "{stderr}"
        );
        assert_eq!(
            stderr.matches(&format!("Using recommended {TEXT}")).count(),
            1,
            "{stderr}"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("Recommended:"));
        fixture.assert_model(mode, TEXT);
        fs::remove_file(fixture.root.path().join("one.md")).expect("remove document");
        fs::remove_file(fixture.root.path().join("two.md")).expect("remove document");
        let rebuilt =
            fixture.success(&["--index", "--rebuild", "--mode", mode, "--color", "never"]);
        let stderr = String::from_utf8_lossy(&rebuilt.stderr);
        assert!(
            stderr.contains(&format!("Using: {TEXT} (existing index)")),
            "{stderr}"
        );
        assert!(!stderr.contains("Workspace: code-dominant"));
        fixture.assert_model(mode, TEXT);
        let explicit = fixture.success(&[
            "--index",
            "--rebuild",
            "--mode",
            mode,
            "--embedding",
            CODE,
            "--color",
            "never",
        ]);
        assert!(!String::from_utf8_lossy(&explicit.stderr).contains("Recommended:"));
        fixture.assert_model(mode, CODE);
        assert_no_download(&fixture);
    }
}

#[test]
fn automatic_selection_uses_index_filters_and_explains_configured_overrides() {
    for mode in ["direct", "server"] {
        let mut fixture = Fixture::new();
        if mode == "server" {
            fixture.start_server();
        }
        weighted_workspace(&fixture);
        let output = fixture.success(&[
            "--index", "--mode", mode, "-t", "markdown", "--color", "never",
        ]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Workspace: text-dominant (code 0%, text 100%)"),
            "{stderr}"
        );
        fixture.assert_model(mode, TEXT);
        fixture.success(&["--index", "--drop", "--mode", mode]);
        fixture.success(&["--config", "model", "set", CODE, "--default"]);
        let output = fixture.success(&["--index", "--mode", mode, "--color", "never"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(&format!("Recommended: {TEXT}")), "{stderr}");
        assert!(
            stderr.contains(&format!("Using: {CODE} (global configuration)")),
            "{stderr}"
        );
        fixture.assert_model(mode, CODE);
        assert_no_download(&fixture);
    }
}

#[test]
fn server_uses_the_callers_embedding_environment_including_its_absence() {
    let mut fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").expect("available port");
    let address = listener.local_addr().expect("address").to_string();
    drop(listener);
    fixture.server_started = true;
    success(
        fixture
            .command(&["--server", "on", "--listen", &address])
            .env("ZVEC_GREP_EMBEDDING", TEXT)
            .output()
            .expect("start server with different defaults"),
    );
    for mode in ["direct", "server"] {
        let output = fixture.success(&["--index", "--mode", mode, "--color", "never"]);
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&format!("Using recommended {CODE}"))
        );
        fixture.assert_model(mode, CODE);
        fixture.success(&["--index", "--drop", "--mode", mode]);
        let output = success(
            fixture
                .command(&["--index", "--mode", mode, "--color", "never"])
                .env("ZVEC_GREP_EMBEDDING", TEXT)
                .output()
                .expect("caller override"),
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("Using: {TEXT} (ZVEC_GREP_EMBEDDING)"))
        );
        fixture.assert_model(mode, TEXT);
        fixture.success(&["--index", "--drop", "--mode", mode]);
        let invalid = fixture
            .command(&["--index", "--mode", mode])
            .env("ZVEC_GREP_EMBEDDING", "invalid/model")
            .output()
            .expect("invalid override");
        assert!(!invalid.status.success());
        assert!(String::from_utf8_lossy(&invalid.stderr).contains("Invalid ZVEC_GREP_EMBEDDING"));
    }
    assert_no_download(&fixture);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn interactive_search_shows_analysis_and_builds_with_the_recommended_model() {
    for mode in ["direct", "server"] {
        let mut fixture = Fixture::new();
        if mode == "server" {
            fixture.start_server();
        }
        weighted_workspace(&fixture);
        let output = success(terminal::run(
            fixture.command(&["--mode", mode, "--fts", "needle", "--color", "never"]),
            &[("Enter Build", b"\r")],
        ));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Workspace: text-dominant"),
            "{mode}: {stderr}"
        );
        assert!(
            !stderr.contains("Basis:") && !stderr.contains("Files:"),
            "{stderr}"
        );
        assert!(stderr.contains(&format!("Recommended: {TEXT}")), "{stderr}");
        assert!(stderr.contains("Recommended · Download"), "{stderr}");
        fixture.assert_model(mode, TEXT);
        assert_no_download(&fixture);
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn cancelling_after_analysis_creates_neither_an_index_nor_a_model_cache() {
    for mode in ["direct", "server"] {
        let mut fixture = Fixture::new();
        if mode == "server" {
            fixture.start_server();
        }
        weighted_workspace(&fixture);
        let output = terminal::run(
            fixture.command(&["--mode", mode, "--fts", "needle", "--color", "never"]),
            &[("Enter Build", b"\x1b")],
        );
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Workspace: text-dominant"));
        assert!(!fixture.root.path().join(".zvec-grep").exists());
        assert_no_download(&fixture);
    }
}
