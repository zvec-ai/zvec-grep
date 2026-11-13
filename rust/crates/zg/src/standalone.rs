//! Shared activation transaction for the shell and PowerShell installers.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zg_cli::StandaloneArgs;
use zg_daemon::{InstallationDaemon, ServerConfig};

mod completion;
mod entry;
mod selection;
mod versioned;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const OWNER: &str = "zvec-grep standalone v2\n";
const MANIFEST: &str = "install-manifest.json";
const RECEIPT: &str = "install-receipt.json";

#[derive(Debug, Deserialize)]
struct PayloadFile {
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema_version: u32,
    version: String,
    target: String,
    accelerator: String,
    files: BTreeMap<String, PayloadFile>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Receipt {
    schema_version: u32,
    source: String,
    version: String,
    target: String,
    accelerator_requested: String,
    accelerator_actual: String,
    home: PathBuf,
    modify_path: bool,
    #[serde(default)]
    path_managed: bool,
    #[serde(default)]
    launcher: Option<PathBuf>,
    #[serde(default)]
    bundle: Option<String>,
    #[serde(default)]
    launcher_sha256: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Restart {
    home: PathBuf,
    listen: String,
    toolset: String,
    token_file: Option<PathBuf>,
    #[serde(default)]
    authentication_required: Option<bool>,
}

impl Restart {
    fn from_config(config: &ServerConfig, authentication_required: bool) -> Self {
        Self {
            home: config.home.clone(),
            listen: config.listen.to_string(),
            toolset: config.mcp_toolset.unwrap_or_default().to_string(),
            token_file: config.token_file.clone(),
            authentication_required: Some(authentication_required),
        }
    }

    fn config(&self, token_file: Option<&Path>) -> Result<ServerConfig> {
        let mut config = ServerConfig::new(self.listen.parse()?, self.home.clone());
        config.mcp_toolset = Some(match self.toolset.as_str() {
            "agent" => zg_daemon::McpToolset::Agent,
            "full" => zg_daemon::McpToolset::Full,
            _ => return Err(io::Error::other("invalid saved daemon toolset").into()),
        });
        config.token_file = token_file
            .map(Path::to_owned)
            .or_else(|| self.token_file.clone());
        // Missing authentication metadata does not establish that anonymous restart is safe.
        if self.authentication_required.unwrap_or(true)
            && zg_daemon::resolve_token(config.token_file.as_deref())?.is_none()
        {
            return Err(io::Error::other("recovery requires the daemon authentication token; supply --token-file or the original token environment").into());
        }
        Ok(config)
    }
}

fn binary(root: &Path) -> PathBuf {
    root.join("bin")
        .join(if cfg!(windows) { "zg.exe" } else { "zg" })
}

fn target() -> String {
    let os = if cfg!(windows) {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let abi = if cfg!(windows) {
        "-msvc"
    } else if cfg!(target_os = "linux") {
        "-gnu"
    } else {
        ""
    };
    format!("{os}-{arch}{abi}")
}

fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn regular_path(root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.is_empty()
        || relative.contains('\\')
        || relative.contains(':')
        || !Path::new(relative)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(io::Error::other(format!("invalid payload path: {relative}")).into());
    }
    let mut path = root.to_owned();
    for part in Path::new(relative).components() {
        path.push(part);
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(io::Error::other(format!("payload contains a link: {relative}")).into());
        }
    }
    if !path.is_file() {
        return Err(io::Error::other("payload is not a regular file").into());
    }
    Ok(path)
}

fn verify_bundle(root: &Path) -> Result<Manifest> {
    let manifest: Manifest = read_json(&regular_path(root, MANIFEST)?)?;
    if manifest.schema_version != 1
        || (cfg!(windows) && !manifest.files.contains_key("bin/zg-launcher.exe"))
        || !versioned::valid_version(&manifest.version)
        || manifest.target != target()
        || !["cpu", "cuda"].contains(&manifest.accelerator.as_str())
        || !manifest.files.contains_key(if cfg!(windows) {
            "bin/zg.exe"
        } else {
            "bin/zg"
        })
    {
        return Err(io::Error::other("standalone bundle identity or platform mismatch").into());
    }
    for (relative, expected) in &manifest.files {
        let path = regular_path(root, relative)?;
        let mut file = File::open(path)?;
        let mut hash = Sha256::new();
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        let mut actual = String::with_capacity(64);
        for byte in hash.finalize() {
            write!(actual, "{byte:02x}")?;
        }
        if file.metadata()?.len() != expected.bytes || actual != expected.sha256 {
            return Err(io::Error::other(format!(
                "standalone payload checksum mismatch: {relative}"
            ))
            .into());
        }
    }
    Ok(manifest)
}

fn copy_bundle(source: &Path, destination: &Path, manifest: &Manifest) -> Result<()> {
    fs::create_dir(destination)?;
    for relative in manifest
        .files
        .keys()
        .map(String::as_str)
        .chain(std::iter::once(MANIFEST))
    {
        let source = regular_path(source, relative)?;
        let output = destination.join(relative);
        fs::create_dir_all(
            output
                .parent()
                .ok_or_else(|| io::Error::other("missing payload parent"))?,
        )?;
        fs::copy(source, output)?;
    }
    Ok(())
}

fn remove_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        _ => {
            return Err(io::Error::other(format!(
                "refusing unexpected installation path: {}",
                path.display()
            ))
            .into());
        }
    }
    Ok(())
}

fn installation_lock(root: &Path) -> Result<File> {
    if fs::symlink_metadata(root).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(io::Error::other("installation directory must not be a symlink").into());
    }
    fs::create_dir_all(root)?;
    let lock_path = root.join(".install.lock");
    if fs::symlink_metadata(&lock_path).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(io::Error::other("invalid installation lock file").into());
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock.try_lock()
        .map_err(|_| io::Error::other("another standalone installation is running"))?;
    let marker = root.join(".zg-standalone");
    if marker.exists() {
        if fs::read_to_string(regular_path(root, ".zg-standalone")?)? != OWNER {
            return Err(io::Error::other("unknown installation owner").into());
        }
    } else {
        if fs::read_dir(root)?
            .any(|entry| entry.is_ok_and(|entry| entry.file_name() != ".install.lock"))
        {
            return Err(io::Error::other(
                "installation directory is not empty or belongs to another installer",
            )
            .into());
        }
        let mut owner = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)?;
        owner.write_all(OWNER.as_bytes())?;
        owner.sync_all()?;
    }
    Ok(lock)
}

async fn check_binary(path: &Path, version: &str) -> Result<()> {
    check_binary_with_timeout(path, version, Duration::from_secs(10)).await
}

async fn check_binary_with_timeout(path: &Path, version: &str, timeout: Duration) -> Result<()> {
    let output = tokio::time::timeout(
        timeout,
        tokio::process::Command::new(path)
            .arg("--version")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        io::Error::other(format!(
            "executable version check timed out: {}",
            path.display()
        ))
    })??;
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != version {
        return Err(io::Error::other(format!(
            "installed executable failed its version check: {} ({}); {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    Ok(())
}

fn release_source(repository: &str) -> Result<String> {
    let parts: Vec<_> = repository.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || *part == "."
                || *part == ".."
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
    {
        return Err(io::Error::other("repository must be a GitHub owner/name").into());
    }
    Ok(format!("https://github.com/{repository}/releases"))
}

async fn prepare_candidate(
    args: &StandaloneArgs,
    executable: &Path,
    next: &Path,
    home: &Path,
) -> Result<Option<Receipt>> {
    remove_directory(next)?;
    if args.remove {
        return Ok(None);
    }
    let source_root = executable
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("invalid staging layout"))?;
    let manifest = verify_bundle(source_root)?;
    if manifest.version != env!("CARGO_PKG_VERSION") {
        return Err(io::Error::other("staging executable and bundle version differ").into());
    }
    if (args.accelerator == "cpu" && manifest.accelerator != "cpu")
        || (args.accelerator == "cuda" && manifest.accelerator != "cuda")
    {
        return Err(io::Error::other("requested accelerator does not match the bundle").into());
    }
    let mut candidate = CandidateCleanup(Some(next));
    copy_bundle(source_root, next, &manifest)?;
    verify_bundle(next)?;
    let receipt = Receipt {
        schema_version: 2,
        source: release_source(&args.repository)?,
        version: manifest.version,
        target: manifest.target,
        accelerator_requested: args.accelerator.clone(),
        accelerator_actual: manifest.accelerator,
        home: home.to_owned(),
        modify_path: !args.no_modify_path,
        path_managed: args.path_managed,
        launcher: None,
        bundle: None,
        launcher_sha256: None,
    };
    check_binary(&binary(next), &receipt.version).await?;
    candidate.0 = None;
    Ok(Some(receipt))
}

struct CandidateCleanup<'a>(Option<&'a Path>);

impl Drop for CandidateCleanup<'_> {
    fn drop(&mut self) {
        if let Some(path) = self.0
            && let Err(error) = remove_directory(path)
        {
            eprintln!("Failed to clean candidate {}: {error}", path.display());
        }
    }
}

fn check_data_layout(root: &Path, home: &Path) -> Result<()> {
    // The managed distribution subtree may share the data home; no other data subtree may
    // become an installation root or be included in a program replacement.
    if home.starts_with(root) || (root.starts_with(home) && root != home.join("dist")) {
        return Err(
            io::Error::other("installation and daemon data directories must be separate").into(),
        );
    }
    Ok(())
}

fn installation_home(args: &StandaloneArgs, root: &Path, old: Option<&Receipt>) -> Result<PathBuf> {
    let home = zg_daemon::resolve_home(
        args.home
            .clone()
            .or_else(|| old.map(|receipt| receipt.home.clone())),
    )?;
    let home = std::path::absolute(home)?;
    if home.starts_with(root) {
        return Err(
            io::Error::other("installation and daemon data directories must be separate").into(),
        );
    }
    fs::create_dir_all(&home)?;
    let home = home.canonicalize()?;
    check_data_layout(root, &home)?;
    if let Some(receipt) = old
        && (receipt.schema_version != 2
            || receipt.target != target()
            || receipt.home.canonicalize()? != home)
    {
        return Err(io::Error::other("installation receipt mismatch; preserve the existing platform and daemon home when upgrading").into());
    }
    Ok(home)
}

pub fn installed_launcher() -> Result<Option<PathBuf>> {
    versioned::installed_launcher()
}

pub async fn execute(args: &StandaloneArgs) -> Result<()> {
    versioned::execute(args).await
}

fn complete_installation(
    args: &StandaloneArgs,
    receipt: Option<&Receipt>,
    current: &Path,
    external: Option<&zg_daemon::ExternalDaemon>,
) {
    if let Err(error) =
        completion::complete(&mut zg_cli::stdout(), args, receipt, current, external)
    {
        eprintln!("Installation finished, but its summary could not be written: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn version_probe_has_a_hard_deadline() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("temp");
        let executable = temp.path().join("slow");
        fs::write(&executable, "#!/bin/sh\nexec sleep 30\n").expect("probe");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).expect("permissions");
        let error = check_binary_with_timeout(&executable, "1", Duration::from_millis(100))
            .await
            .expect_err("deadline");
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn failed_candidate_preparation_removes_partial_files() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("bin")).expect("bin");
        let executable = binary(&source);
        fs::write(&executable, "fixture").expect("binary");
        let digest = |bytes: &[u8]| {
            let mut digest = String::new();
            for byte in Sha256::digest(bytes) {
                write!(digest, "{byte:02x}").expect("digest");
            }
            digest
        };
        let name = if cfg!(windows) {
            "bin/zg.exe"
        } else {
            "bin/zg"
        };
        let mut files = serde_json::json!({ (name): { "bytes": 7, "sha256": digest(b"fixture") } });
        if cfg!(windows) {
            fs::write(source.join("bin/zg-launcher.exe"), "fixture").expect("launcher");
            files["bin/zg-launcher.exe"] =
                serde_json::json!({ "bytes": 7, "sha256": digest(b"fixture") });
        }
        fs::write(source.join(MANIFEST), serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1, "version": env!("CARGO_PKG_VERSION"), "target": target(), "accelerator": "cpu",
            "files": files
        })).expect("manifest")).expect("manifest");
        let root = temp.path().join("program");
        fs::create_dir(&root).expect("root");
        let next = root.join("next");
        let args = StandaloneArgs {
            install_dir: root,
            launcher: None,
            activate: None,
            accelerator: "cpu".into(),
            home: None,
            token_file: None,
            skip_agent_upgrade: true,
            no_modify_path: true,
            path_managed: false,
            repository: "zvec-ai/zvec-grep".into(),
            remove: false,
        };
        let _error = prepare_candidate(&args, &executable, &next, temp.path())
            .await
            .err()
            .expect("invalid executable");
        assert!(!next.exists());
    }

    #[test]
    fn repository_overrides_remain_github_only() {
        assert_eq!(
            release_source("egolearner/zvec-grep").expect("fork"),
            "https://github.com/egolearner/zvec-grep/releases"
        );
        for repository in [
            "https://example.com/repo",
            "owner/repo/extra",
            "../repo",
            "owner/repo?x",
            "owner/repo\n",
        ] {
            assert!(release_source(repository).is_err());
        }
    }

    #[test]
    fn only_the_managed_dist_subtree_may_share_the_data_home() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let home = temporary.path().join("data");
        let separate = temporary.path().join("program");
        assert!(check_data_layout(&home.join("dist"), &home).is_ok());
        assert!(check_data_layout(&separate, &home).is_ok());
        for root in [&home, &home.join("models"), &home.join("daemon")] {
            assert!(check_data_layout(root, &home).is_err());
        }
        assert!(check_data_layout(&separate, &separate.join("data")).is_err());
    }

    #[test]
    fn installation_ownership_and_lock_are_enforced() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = temporary.path().join("安装 with spaces");
        let lock = installation_lock(&root).expect("first lock");
        assert!(installation_lock(&root).is_err());
        drop(lock);
        assert!(installation_lock(&root).is_ok());
        fs::write(root.join(".zg-standalone"), "another installer").expect("owner");
        assert!(installation_lock(&root).is_err());
    }

    #[test]
    fn unknown_directories_and_payload_escapes_are_rejected() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        fs::write(temporary.path().join("user-file"), "preserve me").expect("fixture");
        assert!(installation_lock(temporary.path()).is_err());
        for path in ["../user-file", "/user-file", "C:/user-file", "a\\b"] {
            assert!(regular_path(temporary.path(), path).is_err());
        }
        assert_eq!(
            fs::read_to_string(temporary.path().join("user-file")).expect("user data"),
            "preserve me"
        );
    }
}
