use std::{error::Error, fs, process::Command};
use tempfile::TempDir;

#[test]
fn unindexed_workspace_searches_pdf_raw_bytes_and_ordinary_text() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let home = TempDir::new()?;
    fs::write(root.path().join("README.md"), "recovery calls missing.go\n")?;
    fs::write(root.path().join("raw.pdf"), "%PDF-1.7\nrecovery missing.go")?;
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_zg"))
            .current_dir(root.path())
            .env("ZVEC_GREP_HOME", home.path())
            .args(["--mode", "direct", "--color", "never"])
            .args(args)
            .output()
    };
    for args in [vec!["--rg", "-F", "recovery", "."], vec!["missing.go"]] {
        let output = run(&args)?;
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout)?;
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stdout.contains("README.md"), "{stdout}");
        assert!(stdout.contains("raw.pdf"), "{stdout}");
        assert!(!stdout.to_lowercase().contains("pages:"), "{stdout}");
        assert!(!stderr.contains("extraction index"), "{stderr}");
        assert!(!stderr.contains("zg --index"), "{stderr}");
        assert!(!root.path().join(".zvec-grep").exists());
    }
    let output = run(&["--rg", "-F", "recovery", "raw.pdf"])?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stdout.contains("raw.pdf") && stdout.contains("recovery"),
        "{stdout}"
    );
    assert!(!stderr.contains("Warning:"), "{stderr}");
    fs::write(
        root.path().join("binary.pdf"),
        b"%PDF-1.7\n\0binary-needle\n",
    )?;
    let output = run(&["--rg", "-F", "binary-needle", "binary.pdf"])?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout)?.trim(), "No matches.");
    let output = run(&["--rg", "-a", "-F", "binary-needle", "binary.pdf"])?;
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8(output.stdout)?.contains("binary-needle"));
    assert!(!root.path().join(".zvec-grep").exists());
    Ok(())
}

#[test]
fn unreadable_index_warns_without_blocking_raw_pdf_search() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let home = TempDir::new()?;
    fs::create_dir(root.path().join(".zvec-grep"))?;
    fs::write(root.path().join(".zvec-grep/manifest.json"), "not json")?;
    fs::write(root.path().join("raw.pdf"), "%PDF-1.7\nrecovery")?;
    let output = Command::new(env!("CARGO_BIN_EXE_zg"))
        .current_dir(root.path())
        .env("ZVEC_GREP_HOME", home.path())
        .args([
            "--mode", "direct", "--color", "never", "--rg", "-F", "recovery",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stdout.contains("raw.pdf") && stdout.contains("recovery"),
        "{stdout}"
    );
    assert!(!stdout.to_lowercase().contains("pages:"), "{stdout}");
    assert!(
        stderr.contains("Warning: Cannot read extraction index"),
        "{stderr}"
    );
    assert!(stderr.contains("manifest"), "{stderr}");
    assert!(!stderr.contains("possibly_stale"), "{stderr}");
    Ok(())
}
