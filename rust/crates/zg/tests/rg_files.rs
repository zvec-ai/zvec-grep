use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;

fn run(root: &Path, mode: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zg"))
        .current_dir(root)
        .args(["--mode", mode, "--rg", "--files"])
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("run file discovery")
}

fn paths(output: &Output) -> Vec<PathBuf> {
    String::from_utf8(output.stdout.clone())
        .expect("UTF-8 fixture paths")
        .lines()
        .map(PathBuf::from)
        .collect()
}

fn assert_files(output: &Output, expected: &[&str]) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty(), "{output:?}");
    let mut expected = expected.iter().map(PathBuf::from).collect::<Vec<_>>();
    expected.sort();
    assert_eq!(paths(output), expected);
}

#[test]
fn all_modes_list_all_files_without_content_models_index_or_daemon() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let home = TempDir::new()?;
    for index in 0..24 {
        fs::write(root.path().join(format!("file-{index:02}.bin")), b"\0\xff")?;
    }
    fs::create_dir(root.path().join("node_modules"))?;
    fs::create_dir(root.path().join("dist"))?;
    let extra = [
        "Cargo.lock",
        "node_modules/dependency.js",
        "dist/bundle.js",
        "image.png",
    ];
    for file in extra {
        fs::write(root.path().join(file), b"\0\xff")?;
    }
    let mut expected = (0..24)
        .map(|index| PathBuf::from(format!("file-{index:02}.bin")))
        .chain(extra.into_iter().map(PathBuf::from))
        .collect::<Vec<_>>();
    expected.sort();
    for mode in ["direct", "server", "auto"] {
        let output = Command::new(env!("CARGO_BIN_EXE_zg"))
            .current_dir(root.path())
            .args(["--mode", mode, "--rg", "--files"])
            .env("ZVEC_GREP_HOME", home.path())
            .env("ZVEC_GREP_EMBEDDING", "missing/no-model")
            .output()?;
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        assert_eq!(paths(&output), expected);
        assert!(!root.path().join(".zvec-grep").exists());
        assert_eq!(fs::read_dir(home.path())?.count(), 0);
    }
    Ok(())
}

#[test]
fn roots_globs_and_types_select_paths_instead_of_content() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    fs::create_dir(root.path().join("src"))?;
    fs::create_dir(root.path().join("docs"))?;
    for file in [
        "src/auth.ts",
        "src/auth.test.ts",
        "src/other.rs",
        "docs/readme.md",
        "auth.ts",
    ] {
        fs::write(root.path().join(file), "auth.ts\n")?;
    }
    let cases: &[(&[&str], &[&str])] = &[
        (&["-g", "auth.ts"], &["auth.ts", "src/auth.ts"]),
        (&["src", "-g", "*.ts", "-g", "!*.test.ts"], &["src/auth.ts"]),
        (
            &["src", "docs", "--iglob", "*.TS"],
            &["src/auth.test.ts", "src/auth.ts"],
        ),
        (&["-t", "rust"], &["src/other.rs"]),
        (&["src", "-T", "rust"], &["src/auth.test.ts", "src/auth.ts"]),
        (&["auth.ts"], &["auth.ts"]),
        (&["./src", "-g", "auth.ts"], &["./src/auth.ts"]),
        (&[".", "-t", "rust"], &["./src/other.rs"]),
        (
            &["src", "src", "-t", "rust"],
            &["src/other.rs", "src/other.rs"],
        ),
        (&["-"], &["<stdin>"]),
        (
            &["-e", "[", "-f", "missing-patterns", "auth.ts"],
            &["auth.ts"],
        ),
    ];
    for mode in ["direct", "server"] {
        for (args, expected) in cases {
            assert_files(&run(root.path(), mode, args), expected);
        }
        let absolute = root.path().join("src/auth.ts");
        let output = run(
            root.path(),
            mode,
            &[absolute.to_str().expect("UTF-8 temp path")],
        );
        assert_eq!(paths(&output), [absolute]);
        assert!(output.status.success(), "{output:?}");
    }
    Ok(())
}

#[test]
fn discovery_obeys_ignore_controls_depth_size_and_ordered_globs() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    fs::create_dir(root.path().join(".git"))?;
    fs::create_dir(root.path().join(".zvec-grep"))?;
    fs::create_dir(root.path().join("nested"))?;
    for file in [
        "keep.rs",
        "ignored.rs",
        "vcs.rs",
        ".hidden.rs",
        ".git/private.rs",
        ".zvec-grep/private.rs",
        "nested/deep.rs",
    ] {
        fs::write(root.path().join(file), "data")?;
    }
    fs::write(root.path().join("large.rs"), "longer contents")?;
    fs::write(root.path().join(".ignore"), "ignored.rs\n")?;
    fs::write(root.path().join(".gitignore"), "vcs.rs\n")?;
    fs::write(root.path().join(".custom-ignore"), "keep.rs\n")?;
    let cases: &[(&[&str], &[&str])] = &[
        (&[], &["keep.rs", "large.rs", "nested/deep.rs"]),
        (&["--max-depth", "1", "--max-filesize", "4"], &["keep.rs"]),
        (
            &["--no-ignore-dot"],
            &["ignored.rs", "keep.rs", "large.rs", "nested/deep.rs"],
        ),
        (
            &["--no-ignore-vcs"],
            &["keep.rs", "large.rs", "nested/deep.rs", "vcs.rs"],
        ),
        (
            &["-u"],
            &[
                "ignored.rs",
                "keep.rs",
                "large.rs",
                "nested/deep.rs",
                "vcs.rs",
            ],
        ),
        (
            &["-uu", "-t", "rust"],
            &[
                ".hidden.rs",
                "ignored.rs",
                "keep.rs",
                "large.rs",
                "nested/deep.rs",
                "vcs.rs",
            ],
        ),
        (
            &["--ignore-file", ".custom-ignore"],
            &["large.rs", "nested/deep.rs"],
        ),
        (
            &["--no-ignore-files", "--ignore-file", ".custom-ignore"],
            &["keep.rs", "large.rs", "nested/deep.rs"],
        ),
        (
            &["--iglob", "*.RS", "-g", "!keep.rs"],
            &[
                ".hidden.rs",
                "ignored.rs",
                "large.rs",
                "nested/deep.rs",
                "vcs.rs",
            ],
        ),
        (
            &["-g", "!keep.rs", "--iglob", "*.RS"],
            &[
                ".hidden.rs",
                "ignored.rs",
                "keep.rs",
                "large.rs",
                "nested/deep.rs",
                "vcs.rs",
            ],
        ),
    ];
    for mode in ["direct", "server"] {
        for (args, expected) in cases {
            assert_files(&run(root.path(), mode, args), expected);
        }
        assert_files(
            &run(
                root.path(),
                mode,
                &[".git/private.rs", ".zvec-grep/private.rs"],
            ),
            &[".git/private.rs", ".zvec-grep/private.rs"],
        );
    }
    Ok(())
}

#[test]
fn empty_lists_and_partial_traversal_errors_have_rg_exit_codes() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    for mode in ["direct", "server", "auto"] {
        let empty = run(root.path(), mode, &[]);
        assert_eq!(empty.status.code(), Some(1));
        assert!(empty.stdout.is_empty());
        assert!(empty.stderr.is_empty());
        fs::write(root.path().join("auth.ts"), "body")?;
        let no_glob = run(root.path(), mode, &["-g", "missing.ts"]);
        assert_eq!(no_glob.status.code(), Some(1));
        assert!(no_glob.stdout.is_empty());
        assert!(no_glob.stderr.is_empty());
        let partial = run(root.path(), mode, &["missing", "auth.ts"]);
        assert_eq!(partial.status.code(), Some(2));
        assert_eq!(paths(&partial), [PathBuf::from("auth.ts")]);
        assert!(String::from_utf8_lossy(&partial.stderr).contains("missing"));
        let missing = run(root.path(), mode, &["missing"]);
        assert_eq!(missing.status.code(), Some(2));
        assert!(missing.stdout.is_empty());
        let empty_path = run(root.path(), mode, &[""]);
        assert_eq!(empty_path.status.code(), Some(2));
        assert!(empty_path.stdout.is_empty());
        assert!(!empty_path.stderr.is_empty());
        let invalid_type = run(root.path(), mode, &["-t", "missing-type"]);
        assert_eq!(invalid_type.status.code(), Some(2));
        assert!(!invalid_type.stderr.is_empty());
        fs::remove_file(root.path().join("auth.ts"))?;
    }
    Ok(())
}

#[test]
fn ignore_parse_errors_fail_with_partial_or_empty_file_lists() -> Result<(), Box<dyn Error>> {
    for ignore_name in [".ignore", ".gitignore"] {
        let root = TempDir::new()?;
        fs::create_dir(root.path().join(".git"))?;
        let source = root.path().join("src");
        fs::create_dir(&source)?;
        fs::write(source.join("keep.ts"), "body")?;
        fs::write(source.join("ignored.ts"), "body")?;
        for excluded in ["ignored.ts", "*.ts"] {
            fs::write(source.join(ignore_name), format!("{{invalid\n{excluded}\n"))?;
            for mode in ["direct", "server", "auto"] {
                let output = run(root.path(), mode, &[]);
                assert_eq!(
                    output.status.code(),
                    Some(2),
                    "{mode} {ignore_name}: {output:?}"
                );
                assert_eq!(
                    paths(&output),
                    if excluded == "ignored.ts" {
                        vec![PathBuf::from("src/keep.ts")]
                    } else {
                        Vec::new()
                    },
                    "{mode} {ignore_name} {excluded}"
                );
                let stderr = String::from_utf8(output.stderr)?;
                assert!(stderr.contains(ignore_name), "{stderr}");
                assert!(stderr.contains("{invalid"), "{stderr}");
            }
        }
        let disable = if ignore_name == ".gitignore" {
            "--no-ignore-vcs"
        } else {
            "--no-ignore-dot"
        };
        for mode in ["direct", "server", "auto"] {
            assert_files(
                &run(root.path(), mode, &[disable]),
                &["src/ignored.ts", "src/keep.ts"],
            );
        }
    }
    Ok(())
}

#[test]
fn null_output_and_literal_paths_are_safe_for_scripts() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    for file in ["space name.ts", "中文.ts", "--files"] {
        fs::write(root.path().join(file), "data")?;
    }
    for flag in ["-0", "--null"] {
        let output = run(root.path(), "server", &[flag]);
        assert!(output.status.success(), "{output:?}");
        let actual = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| PathBuf::from(std::str::from_utf8(path).expect("UTF-8 path")))
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            [
                PathBuf::from("--files"),
                PathBuf::from("space name.ts"),
                PathBuf::from("中文.ts")
            ]
        );
        assert!(output.stderr.is_empty());
    }
    assert_files(
        &run(root.path(), "direct", &["--", "--files"]),
        &["--files"],
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn lists_unreadable_sources_and_preserves_newlines_in_filenames() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new()?;
    let name = "line\nname.ts";
    let path = root.path().join(name);
    fs::write(&path, "unreadable source")?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o0))?;
    let output = run(root.path(), "direct", &["-0"]);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    assert!(output.status.success(), "{output:?}");
    let mut expected = name.as_bytes().to_vec();
    expected.push(0);
    assert_eq!(output.stdout, expected);
    assert!(output.stderr.is_empty());
    Ok(())
}

#[cfg(unix)]
#[test]
fn follows_symlink_directories_only_when_requested() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;
    let root = TempDir::new()?;
    let target = TempDir::new()?;
    fs::write(target.path().join("auth.ts"), "data")?;
    symlink(target.path(), root.path().join("linked"))?;
    let skipped = run(root.path(), "direct", &[]);
    assert_eq!(skipped.status.code(), Some(1));
    assert_files(&run(root.path(), "server", &["-L"]), &["linked/auth.ts"]);
    symlink(target.path().join("auth.ts"), root.path().join("link.ts"))?;
    assert_files(&run(root.path(), "direct", &["link.ts"]), &["link.ts"]);
    Ok(())
}
