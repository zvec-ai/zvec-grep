use std::{
    collections::BTreeSet,
    error::Error,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::UNIX_EPOCH,
};

use tempfile::tempdir;
use zg_host_native::{
    AllowAllPaths, DiscoveredFile, HostError, NativeScanner, PathPolicy, ReadBatchRequest,
    RootSpec, ScanRequest, ScanSnapshot, SkippedFileReason, TaskControl, WorkspaceScannerPort,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[derive(Debug)]
struct TestPolicy {
    blocked_directory: PathBuf,
    excluded_file: PathBuf,
    inspected_files: Mutex<Vec<PathBuf>>,
}

impl PathPolicy for TestPolicy {
    fn includes_file(&self, path: &Path) -> Result<bool, HostError> {
        self.inspected_files
            .lock()
            .expect("record file")
            .push(path.to_path_buf());
        Ok(path != self.excluded_file)
    }

    fn can_descend(&self, path: &Path) -> Result<bool, HostError> {
        Ok(!path.starts_with(&self.blocked_directory))
    }
}

#[tokio::test]
async fn scanner_delegates_selection_and_prunes_directories_before_visiting_files() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    fs::create_dir_all(root.join("blocked/deep"))?;
    fs::create_dir_all(root.join("nested/.git"))?;
    fs::write(root.join("tracked.rs"), "fn tracked() {}\n")?;
    fs::write(root.join("excluded.txt"), "excluded\n")?;
    fs::write(root.join("blocked/deep/file.rs"), "blocked\n")?;
    fs::write(root.join("nested/child.rs"), "nested\n")?;
    let policy = Arc::new(TestPolicy {
        blocked_directory: root.join("blocked"),
        excluded_file: root.join("excluded.txt"),
        inspected_files: Mutex::new(Vec::new()),
    });
    let scanner = NativeScanner::default();
    let snapshot = discover(&scanner, RootSpec::new(root.to_path_buf(), policy.clone())).await?;
    assert_eq!(
        relative_paths(&snapshot.files),
        BTreeSet::from([
            PathBuf::from("nested/child.rs"),
            PathBuf::from("tracked.rs"),
        ])
    );
    assert!(
        !policy
            .inspected_files
            .lock()
            .expect("inspected files")
            .iter()
            .any(|path| path.starts_with(root.join("blocked")))
    );
    let tracked = snapshot
        .files
        .iter()
        .find(|file| file.relative_path == Path::new("tracked.rs"))
        .expect("tracked file");
    assert_eq!(
        tracked.source_fingerprint,
        metadata_fingerprint(&root.join("tracked.rs"))?
    );
    let sources = scanner
        .read_batch(
            &ReadBatchRequest {
                files: snapshot.files,
            },
            &TaskControl::default(),
        )
        .await?;
    assert!(
        sources
            .iter()
            .any(|file| file.bytes == b"fn tracked() {}\n")
    );
    Ok(())
}

#[tokio::test]
async fn scanner_limits_discovery_to_requested_scope_paths() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    fs::create_dir_all(root.join("unrelated/deep"))?;
    fs::write(root.join("changed.txt"), "changed\n")?;
    fs::write(root.join("unrelated/deep/untouched.txt"), "untouched\n")?;
    let snapshot = NativeScanner::default()
        .discover(
            &ScanRequest {
                roots: vec![root_spec(root)],
                scope_paths: vec![root.join("changed.txt")],
            },
            &TaskControl::default(),
        )
        .await?;
    assert_eq!(
        relative_paths(&snapshot.files),
        BTreeSet::from([PathBuf::from("changed.txt")])
    );
    Ok(())
}

#[tokio::test]
async fn scanner_applies_depth_and_size_limits_without_classifying_contents() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    fs::create_dir_all(root.join("src/deep"))?;
    fs::write(root.join("root.ts"), "export const root = 1;\n")?;
    fs::write(root.join("src/child.ts"), "export const child = 1;\n")?;
    fs::write(root.join("src/deep/grand.ts"), "export const grand = 1;\n")?;
    fs::write(root.join("binary.md"), [0_u8, 1, 2, 0, 3])?;
    fs::write(root.join("empty.txt"), [])?;
    fs::write(root.join("large.ts"), vec![b'x'; 1025])?;
    let scanner = NativeScanner::default();
    let mut root_spec = root_spec(root);
    root_spec.max_depth = Some(2);
    root_spec.max_file_size_bytes = Some(1024);
    let filtered = discover(&scanner, root_spec).await?;
    assert_eq!(
        relative_paths(&filtered.files),
        BTreeSet::from([
            PathBuf::from("binary.md"),
            PathBuf::from("root.ts"),
            PathBuf::from("src/child.ts"),
        ])
    );
    assert_eq!(filtered.diagnostics.skipped_by_reason.too_large, 1);
    assert_eq!(filtered.diagnostics.skipped_by_reason.empty, 1);
    assert!(
        filtered
            .diagnostics
            .skipped_samples
            .iter()
            .any(|sample| sample.reason == SkippedFileReason::Empty)
    );
    let binary = filtered
        .files
        .iter()
        .find(|file| file.relative_path == Path::new("binary.md"))
        .expect("binary content is discovered without probing");
    let sources = scanner
        .read_batch(
            &ReadBatchRequest {
                files: vec![binary.clone()],
            },
            &TaskControl::default(),
        )
        .await?;
    assert_eq!(sources[0].bytes, [0_u8, 1, 2, 0, 3]);
    Ok(())
}

#[tokio::test]
async fn scanner_rejects_overlapping_roots() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    fs::create_dir_all(root.join("child"))?;
    let error = NativeScanner::default()
        .discover(
            &ScanRequest {
                roots: vec![root_spec(root), root_spec(&root.join("child"))],
                scope_paths: Vec::new(),
            },
            &TaskControl::default(),
        )
        .await
        .expect_err("recursive nested roots must overlap");
    assert!(error.to_string().contains("overlap"));
    Ok(())
}

#[derive(Debug)]
struct MoveRootDuringScan {
    root: PathBuf,
    destination: PathBuf,
    replace: bool,
}

impl PathPolicy for MoveRootDuringScan {
    fn includes_file(&self, _: &Path) -> Result<bool, HostError> {
        Ok(true)
    }

    fn can_descend(&self, path: &Path) -> Result<bool, HostError> {
        if path == self.root {
            fs::rename(&self.root, &self.destination).expect("move root during traversal");
            if self.replace {
                fs::create_dir(&self.root).expect("replace root with empty directory");
            }
        }
        Ok(true)
    }
}

#[tokio::test]
async fn scanner_rejects_roots_removed_or_replaced_after_initial_validation() -> TestResult {
    for replace in [false, true] {
        let temporary = tempdir()?;
        let root = temporary.path().join("workspace");
        fs::create_dir(&root)?;
        fs::write(root.join("source.rs"), "fn source() {}")?;
        let policy = Arc::new(MoveRootDuringScan {
            root: root.clone(),
            destination: temporary.path().join("moved"),
            replace,
        });
        let error = discover(
            &NativeScanner::default(),
            RootSpec::new(root.clone(), policy),
        )
        .await
        .expect_err("a missing or replaced root must not become an empty snapshot");
        assert!(error.to_string().contains(&root.display().to_string()));
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn scanner_reports_unreadable_subdirectories_instead_of_a_partial_snapshot() -> TestResult {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempdir()?;
    let root = temporary.path();
    let child = root.join("restricted");
    fs::create_dir(&child)?;
    fs::write(child.join("source.txt"), "keep indexed content")?;
    let permissions = fs::metadata(&child)?.permissions();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o000))?;
    if fs::read_dir(&child).is_ok() {
        fs::set_permissions(&child, permissions)?;
        eprintln!(
            "Skipping permission failure injection: this user can read chmod 000 directories."
        );
        return Ok(());
    }
    let result = discover(&NativeScanner::default(), root_spec(root)).await;
    fs::set_permissions(&child, permissions)?;
    let error = result.expect_err("unreadable source directories must fail discovery");
    assert!(error.to_string().contains("read directory"));
    assert!(error.to_string().contains("restricted"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn scanner_follows_links_without_visiting_the_same_directory_twice() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    fs::create_dir(root.join("child"))?;
    fs::write(root.join("child/source.rs"), "fn source() {}")?;
    std::os::unix::fs::symlink(root, root.join("child/cycle"))?;
    std::os::unix::fs::symlink(root.join("child"), root.join("linked"))?;
    std::os::unix::fs::symlink(root.join("removed"), root.join("broken"))?;
    let mut spec = root_spec(root);
    spec.follow = true;
    let snapshot = discover(&NativeScanner::default(), spec).await?;
    assert_eq!(
        relative_paths(&snapshot.files),
        BTreeSet::from([PathBuf::from("child/source.rs")])
    );
    Ok(())
}

async fn discover(scanner: &NativeScanner, root: RootSpec) -> TestResult<ScanSnapshot> {
    Ok(scanner
        .discover(
            &ScanRequest {
                roots: vec![root],
                scope_paths: Vec::new(),
            },
            &TaskControl::default(),
        )
        .await?)
}

fn root_spec(root: &Path) -> RootSpec {
    RootSpec::new(root.to_path_buf(), Arc::new(AllowAllPaths))
}

fn relative_paths(files: &[DiscoveredFile]) -> BTreeSet<PathBuf> {
    files
        .iter()
        .map(|file| file.relative_path.clone())
        .collect()
}

fn metadata_fingerprint(path: &Path) -> TestResult<String> {
    let metadata = fs::metadata(path)?;
    let modified = u64::try_from(metadata.modified()?.duration_since(UNIX_EPOCH)?.as_millis())?;
    Ok(format!("metadata-v1:{}:{modified}", metadata.len()))
}
