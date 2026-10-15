use std::{error::Error, fs, process::Command};

use tempfile::TempDir;

#[test]
fn code_keyword_search_is_exact_current_and_independent_of_index_and_transport()
-> Result<(), Box<dyn Error>> {
    let workspace = TempDir::new()?;
    let home = TempDir::new()?;
    fs::write(
        workspace.path().join("sample.ts"),
        "verifyToken();\nverifyTokenExtra();\npreverifyToken();\nverifytoken();\nverifyToken_legacy();\nrequest_service();\nRequest::Service();\n",
    )?;
    fs::write(workspace.path().join("excluded.ts"), "verifyToken();\n")?;
    let mut previous = None;
    for mode in ["direct", "server", "auto"] {
        let output = Command::new(env!("CARGO_BIN_EXE_zg"))
            .current_dir(workspace.path())
            .env("ZVEC_GREP_HOME", home.path())
            .env(
                "ZVEC_GREP_WORKSPACE_REGISTRY",
                home.path().join("registry.json"),
            )
            .args([
                "verifyToken",
                "--mode",
                mode,
                "-g",
                "!excluded.ts",
                "--refresh",
                "off",
            ])
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout)?;
        assert_eq!(
            stdout.lines().collect::<Vec<_>>(),
            ["sample.ts", "  1: verifyToken();"]
        );
        if let Some(previous) = &previous {
            assert_eq!(&stdout, previous);
        }
        previous = Some(stdout);
    }
    for (query, line) in [("request_service", 6), ("Request::Service", 7)] {
        let output = Command::new(env!("CARGO_BIN_EXE_zg"))
            .current_dir(workspace.path())
            .env("ZVEC_GREP_HOME", home.path())
            .args([query, "--mode", "direct"])
            .output()?;
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout)?.contains(&format!("  {line}: {query}();")));
    }
    fs::write(workspace.path().join("sample.ts"), "changedToken();\n")?;
    let output = Command::new(env!("CARGO_BIN_EXE_zg"))
        .current_dir(workspace.path())
        .env("ZVEC_GREP_HOME", home.path())
        .args(["verifyToken", "--mode", "direct", "-g", "!excluded.ts"])
        .output()?;
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout)?, "No matches.\n");
    assert!(!workspace.path().join(".zvec-grep").exists());
    assert!(!home.path().join("daemon").exists());
    Ok(())
}
