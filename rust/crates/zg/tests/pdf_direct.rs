use std::{error::Error, fs, process::Command};
use tempfile::TempDir;

#[test]
fn unindexed_pdf_does_not_block_text_or_filename_fallback() -> Result<(), Box<dyn Error>> {
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
        assert!(!stdout.contains("raw.pdf"), "{stdout}");
        assert_eq!(stderr.matches("skipped 1").count(), 1, "{stderr}");
        assert!(stderr.contains("zg --index"), "{stderr}");
        assert!(!root.path().join(".zvec-grep").exists());
    }
    let output = run(&["--rg", "-F", "recovery", "raw.pdf"])?;
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("raw.pdf") && stderr.contains("zg --index"),
        "{stderr}"
    );
    Ok(())
}
