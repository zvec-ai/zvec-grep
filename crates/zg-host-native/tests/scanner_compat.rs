use std::{
    collections::BTreeSet,
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use zg_host_native::{
    DiscoveredFile, DiscoveryOptions, NativeScanner, ReadBatchRequest, RootSpec, ScanRequest,
    ScanSnapshot, SkippedFileReason, TaskControl, WorkspaceScannerPort,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[tokio::test]
async fn scanner_matches_typescript_ignore_hidden_nested_git_and_path_filters() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    mkdir(root, "src/nested/.git")?;
    mkdir(root, "node_modules/pkg")?;
    mkdir(root, "dist")?;
    mkdir(root, ".hidden")?;
    mkdir(root, "vendor")?;
    write(
        root,
        ".gitignore",
        "ignored.txt\nvendor/\n!vendor/keep.ts\n",
    )?;
    write(root, "src/main.ts", "export const main = 1;\n")?;
    write(root, "src/skip.log", "skip\n")?;
    write(root, "src/nested/child.ts", "nested\n")?;
    write(root, "ignored.txt", "ignored\n")?;
    write(root, "vendor/keep.ts", "export {};\n")?;
    write(root, "node_modules/pkg/index.js", "ignored\n")?;
    write(root, "dist/output.js", "ignored\n")?;
    write(root, ".hidden/secret.ts", "hidden\n")?;

    let scanner = NativeScanner::default();
    let snapshot = discover(
        &scanner,
        root,
        DiscoveryOptions {
            include_paths: vec!["src/**".to_owned(), "vendor/keep.ts".to_owned()],
            exclude_paths: vec!["**/*.log".to_owned()],
            ..DiscoveryOptions::default()
        },
    )
    .await?;
    assert_eq!(
        relative_paths(&snapshot.files),
        BTreeSet::from([
            PathBuf::from("src/main.ts"),
            PathBuf::from("src/nested/child.ts"),
            PathBuf::from("vendor/keep.ts"),
        ])
    );
    assert_eq!(snapshot.files[0].size_bytes, 23);
    assert_eq!(
        snapshot.files[0].source_fingerprint,
        metadata_fingerprint(&root.join("src/main.ts"))?,
    );

    let sources = scanner
        .read_batch(
            &ReadBatchRequest {
                files: snapshot.files,
            },
            &control(),
        )
        .await?;
    assert!(
        sources
            .iter()
            .any(|file| file.bytes == b"export const main = 1;\n")
    );
    Ok(())
}

#[tokio::test]
async fn scanner_matches_typescript_defaults_and_explicit_includes() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    mkdir(root, "src")?;
    mkdir(root, "locales")?;
    mkdir(root, ".github/workflows")?;
    write(root, "src/main.ts", "export const main = true;\n")?;
    write(root, "locales/en.json", "{\"hello\":\"Hello\"}\n")?;
    write(root, "package-lock.json", "{\"lockfileVersion\":3}\n")?;
    write(root, "client.pb.go", "package client\n")?;
    write(root, "logo.png", "not-a-real-image")?;
    write(root, ".github/workflows/ci.yml", "name: CI\n")?;
    write(root, ".env.example", "TOKEN=replace-me\n")?;

    let scanner = NativeScanner::default();
    let defaults = discover(&scanner, root, DiscoveryOptions::default()).await?;
    assert_eq!(
        relative_paths(&defaults.files),
        BTreeSet::from([PathBuf::from("src/main.ts")])
    );

    let explicit = discover(
        &scanner,
        root,
        DiscoveryOptions {
            include_paths: vec![
                "locales/en.json".to_owned(),
                "package-lock.json".to_owned(),
                "client.pb.go".to_owned(),
                "logo.png".to_owned(),
            ],
            ..DiscoveryOptions::default()
        },
    )
    .await?;
    assert_eq!(
        relative_paths(&explicit.files),
        BTreeSet::from([
            PathBuf::from("client.pb.go"),
            PathBuf::from("locales/en.json"),
            PathBuf::from("logo.png"),
            PathBuf::from("package-lock.json"),
        ])
    );
    Ok(())
}

#[tokio::test]
async fn scanner_limits_discovery_to_requested_scope_paths() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    mkdir(root, "unrelated/deep")?;
    write(root, "changed.txt", "changed\n")?;
    write(root, "unrelated/deep/untouched.txt", "untouched\n")?;

    let scanner = NativeScanner::default();
    let snapshot = scanner
        .discover(
            &ScanRequest {
                roots: vec![root_spec(root)],
                scope_paths: vec![root.join("changed.txt")],
            },
            &control(),
        )
        .await?;

    assert_eq!(
        relative_paths(&snapshot.files),
        BTreeSet::from([PathBuf::from("changed.txt")])
    );
    Ok(())
}

#[tokio::test]
async fn scanner_applies_path_filters_and_explicit_size_limits_without_classifying_contents()
-> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    mkdir(root, "src/deep")?;
    mkdir(root, ".hidden")?;
    write(root, ".gitignore", "ignored.ts\n")?;
    write(root, "root.ts", "export const root = 1;\n")?;
    write(root, "ignored.ts", "export const ignored = 1;\n")?;
    write(root, "root.py", "root = 1\n")?;
    write(root, "skip.test.ts", "export const skip = 1;\n")?;
    write(root, ".hidden/secret.ts", "export const secret = 1;\n")?;
    write(root, "src/child.ts", "export const child = 1;\n")?;
    write(root, "src/deep/grand.ts", "export const grand = 1;\n")?;
    fs::write(root.join("binary.md"), [0_u8, 1, 2, 0, 3])?;
    fs::write(root.join("archive.zip"), [0_u8, 1, 2, 3])?;
    fs::write(root.join("empty.txt"), [])?;
    fs::write(root.join("large.ts"), vec![b'x'; 1024 * 1024 + 1])?;

    let scanner = NativeScanner::default();
    let filtered = discover(
        &scanner,
        root,
        DiscoveryOptions {
            globs: vec!["**".to_owned(), "!**/*.test.ts".to_owned()],
            file_types: vec!["ts".to_owned()],
            hidden: true,
            no_ignore: true,
            max_depth: Some(2),
            max_file_size_bytes: Some(1024 * 1024),
            ..DiscoveryOptions::default()
        },
    )
    .await?;
    assert_eq!(
        relative_paths(&filtered.files),
        BTreeSet::from([
            PathBuf::from(".hidden/secret.ts"),
            PathBuf::from("ignored.ts"),
            PathBuf::from("root.ts"),
            PathBuf::from("src/child.ts"),
        ])
    );
    assert_eq!(filtered.diagnostics.skipped_by_reason.too_large, 1);

    let unrestricted = discover(
        &scanner,
        root,
        DiscoveryOptions {
            include_paths: vec![
                "binary.md".to_owned(),
                "archive.zip".to_owned(),
                "large.ts".to_owned(),
                "empty.txt".to_owned(),
            ],
            no_ignore: true,
            ..DiscoveryOptions::default()
        },
    )
    .await?;
    assert_eq!(
        relative_paths(&unrestricted.files),
        BTreeSet::from([
            PathBuf::from("archive.zip"),
            PathBuf::from("binary.md"),
            PathBuf::from("large.ts"),
        ])
    );
    assert_eq!(unrestricted.diagnostics.skipped_files, 1);
    assert_eq!(unrestricted.diagnostics.skipped_by_reason.empty, 1);
    assert_eq!(
        unrestricted.diagnostics.skipped_samples[0].reason,
        SkippedFileReason::Empty,
    );
    let binary = unrestricted
        .files
        .iter()
        .find(|file| file.relative_path == Path::new("binary.md"))
        .expect("binary content is discovered without probing");
    assert_eq!(
        binary.source_fingerprint,
        metadata_fingerprint(&root.join("binary.md"))?
    );
    let sources = scanner
        .read_batch(
            &ReadBatchRequest {
                files: vec![binary.clone()],
            },
            &control(),
        )
        .await?;
    assert_eq!(sources[0].bytes, [0_u8, 1, 2, 0, 3]);
    Ok(())
}

#[tokio::test]
async fn scanner_rejects_overlapping_roots() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    mkdir(root, "child")?;
    let scanner = NativeScanner::default();
    let error = scanner
        .discover(
            &ScanRequest {
                roots: vec![root_spec(root), root_spec(&root.join("child"))],
                scope_paths: Vec::new(),
            },
            &control(),
        )
        .await
        .expect_err("recursive nested roots must overlap");
    assert!(error.to_string().contains("overlap"));
    Ok(())
}

async fn discover(
    scanner: &NativeScanner,
    root: &Path,
    discovery: DiscoveryOptions,
) -> TestResult<ScanSnapshot> {
    Ok(scanner
        .discover(
            &ScanRequest {
                roots: vec![RootSpec {
                    path: root.to_path_buf(),
                    recursive: true,
                    discovery,
                }],
                scope_paths: Vec::new(),
            },
            &control(),
        )
        .await?)
}

fn root_spec(root: &Path) -> RootSpec {
    RootSpec {
        path: root.to_path_buf(),
        recursive: true,
        discovery: DiscoveryOptions::default(),
    }
}

fn relative_paths(files: &[DiscoveredFile]) -> BTreeSet<PathBuf> {
    files
        .iter()
        .map(|file| file.relative_path.clone())
        .collect()
}

fn control() -> TaskControl {
    TaskControl::new(CancellationToken::new())
}

fn mkdir(root: &Path, path: &str) -> std::io::Result<()> {
    fs::create_dir_all(root.join(path))
}

fn write(root: &Path, path: &str, content: &str) -> std::io::Result<()> {
    fs::write(root.join(path), content)
}

fn metadata_fingerprint(path: &Path) -> TestResult<String> {
    let metadata = fs::metadata(path)?;
    let modified = u64::try_from(metadata.modified()?.duration_since(UNIX_EPOCH)?.as_millis())?;
    Ok(format!("metadata-v1:{}:{modified}", metadata.len()))
}
