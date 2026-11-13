use super::{entry::Entry, selection, *};
use std::sync::atomic::{AtomicU64, Ordering};

const JOURNAL: &str = "activation.json";
const STATE: &str = "install-state.tsv";

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Transaction {
    before: Option<Receipt>,
    after: Option<Receipt>,
    home: PathBuf,
    restart: Option<Restart>,
    entry: Entry,
    bundles: Vec<String>,
    committed: bool,
}

pub(super) fn valid_version(version: &str) -> bool {
    let (core, suffix) = version
        .split_once('-')
        .map_or((version, None), |(core, suffix)| (core, Some(suffix)));
    let parts: Vec<_> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit()))
        && suffix.is_none_or(|suffix| {
            !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
        })
}

pub(super) fn validate_bundle_key(key: &str) -> Result<()> {
    let Some((version, variant)) = key.split_once('/') else {
        return Err(io::Error::other("invalid version bundle path").into());
    };
    if !valid_version(version)
        || ![format!("{}-cpu", target()), format!("{}-cuda", target())]
            .contains(&variant.to_owned())
    {
        return Err(io::Error::other("invalid version bundle path").into());
    }
    Ok(())
}

fn key(receipt: &Receipt) -> Result<&str> {
    let key = receipt
        .bundle
        .as_deref()
        .ok_or_else(|| io::Error::other("receipt is missing its version bundle"))?;
    validate_bundle_key(key)?;
    if receipt.schema_version != 2
        || key
            != format!(
                "{}/{}-{}",
                receipt.version, receipt.target, receipt.accelerator_actual
            )
    {
        return Err(io::Error::other("receipt does not match its version bundle").into());
    }
    Ok(key)
}

fn checked_path(root: &Path, key: &str) -> Result<PathBuf> {
    validate_bundle_key(key)?;
    let mut path = root.to_owned();
    for component in Path::new(key).components() {
        path.push(component);
        if fs::symlink_metadata(&path)
            .is_ok_and(|metadata| !metadata.is_dir() || metadata.file_type().is_symlink())
        {
            return Err(
                io::Error::other("version directories must be ordinary directories").into(),
            );
        }
    }
    Ok(path)
}

pub(super) fn bundle_path(root: &Path, receipt: &Receipt) -> Result<PathBuf> {
    checked_path(root, key(receipt)?)
}

pub(super) fn file_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let mut result = String::with_capacity(64);
    for byte in digest.finalize() {
        write!(result, "{byte:02x}")?;
    }
    Ok(result)
}

pub(super) fn temporary(root: &Path, name: &str) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    root.join(format!(
        ".{name}-{}-{nanos}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn atomic_write(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let path = root.join(name);
    if fs::symlink_metadata(&path)
        .is_ok_and(|metadata| !metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err(io::Error::other(format!("invalid installation metadata: {name}")).into());
    }
    let temporary = temporary(root, name);
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}

fn save_transaction(root: &Path, transaction: &Transaction) -> Result<()> {
    atomic_write(root, JOURNAL, &serde_json::to_vec_pretty(transaction)?)
}

fn unlink(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn save_state(root: &Path, receipt: &Receipt) -> Result<()> {
    let repository = receipt
        .source
        .strip_prefix("https://github.com/")
        .and_then(|source| source.strip_suffix("/releases"))
        .ok_or_else(|| io::Error::other("receipt has an unsupported release source"))?;
    release_source(repository)?;
    atomic_write(root, STATE, format!("bundle\t{}\nversion\t{}\naccelerator\t{}\nmodify_path\t{}\npath_managed\t{}\nrepository\t{}\n",
        key(receipt)?, receipt.version, receipt.accelerator_requested, u8::from(receipt.modify_path), u8::from(receipt.path_managed), repository).as_bytes())
}

fn save_receipt(root: &Path, receipt: Option<&Receipt>) -> Result<()> {
    if let Some(receipt) = receipt {
        atomic_write(root, RECEIPT, &serde_json::to_vec_pretty(receipt)?)?;
        save_state(root, receipt)?;
    } else {
        unlink(&root.join(RECEIPT))?;
        unlink(&root.join(STATE))?;
    }
    Ok(())
}

fn installed_bundles(root: &Path) -> Result<Vec<String>> {
    let mut bundles = Vec::new();
    for version in fs::read_dir(root)? {
        let version = version?;
        let name = version.file_name().to_string_lossy().into_owned();
        if !valid_version(&name) || !version.file_type()?.is_dir() {
            continue;
        }
        for variant in ["cpu", "cuda"] {
            let key = format!("{name}/{}-{variant}", target());
            let path = checked_path(root, &key)?;
            if !path.exists() {
                continue;
            }
            let manifest: Manifest = read_json(&regular_path(&path, MANIFEST)?)?;
            if manifest.version != name
                || manifest.target != target()
                || manifest.accelerator != variant
            {
                return Err(io::Error::other(
                    "installed bundle identity does not match its directory",
                )
                .into());
            }
            bundles.push(key);
        }
    }
    Ok(bundles)
}

async fn claim(
    root: &Path,
    transaction: &Transaction,
    token_file: Option<&Path>,
) -> Result<InstallationDaemon> {
    let executables = transaction
        .bundles
        .iter()
        .map(|key| checked_path(root, key).map(|path| binary(&path)))
        .collect::<Result<Vec<_>>>()?;
    Ok(InstallationDaemon::acquire(&transaction.home, &executables, token_file).await?)
}

async fn rollback(
    root: &Path,
    transaction: &Transaction,
    daemon: &InstallationDaemon,
    token_file: Option<&Path>,
) -> Result<()> {
    let restart = transaction
        .restart
        .as_ref()
        .map(|restart| restart.config(token_file))
        .transpose()?;
    if restart.is_some() {
        daemon.require_owned()?;
    }
    daemon.stop().await?;
    selection::replace(
        root,
        transaction.after.as_ref().map(key).transpose()?,
        transaction.before.as_ref().map(key).transpose()?,
    )?;
    save_receipt(root, transaction.before.as_ref())?;
    transaction.entry.restore()?;
    if let Some(before) = &transaction.before {
        daemon
            .restart(&binary(&bundle_path(root, before)?), restart.as_ref())
            .await?;
    }
    unlink(&root.join(JOURNAL))?;
    Ok(())
}

fn finish_removal(root: &Path, transaction: &Transaction) -> Result<()> {
    // A committed uninstall is resumed, never rolled back after files have been removed.
    transaction.entry.remove()?;
    for key in &transaction.bundles {
        let path = checked_path(root, key)?;
        remove_directory(&path)?;
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
    save_receipt(root, None)?;
    unlink(&root.join(JOURNAL))?;
    Ok(())
}

async fn recover(root: &Path, token_file: &mut Option<PathBuf>, remove: bool) -> Result<bool> {
    if !root.join(JOURNAL).exists() {
        return Ok(false);
    }
    let mut transaction: Transaction = read_json(&regular_path(root, JOURNAL)?)?;
    check_data_layout(root, &transaction.home)?;
    if transaction.committed {
        if transaction.after.is_some() {
            return Err(io::Error::other("invalid committed uninstall journal").into());
        }
        if token_file.is_none() {
            *token_file = transaction
                .restart
                .as_ref()
                .and_then(|restart| restart.token_file.clone());
        }
        let daemon = claim(root, &transaction, token_file.as_deref()).await?;
        daemon.stop().await?;
        finish_removal(root, &transaction)?;
        return Ok(true);
    }
    // Resolve credentials before changing either the selection or a running process.
    // Keep the resolved path for the installation that follows recovery.
    if let Some(restart) = &transaction.restart {
        *token_file = restart.config(token_file.as_deref())?.token_file;
    }
    let daemon = claim(root, &transaction, token_file.as_deref()).await?;
    rollback(root, &transaction, &daemon, token_file.as_deref()).await?;
    if remove && transaction.before.is_none() {
        transaction.before = transaction.after.take();
        transaction.restart = None;
        transaction.committed = true;
        if let Some(before) = &transaction.before {
            save_state(root, before)?;
        }
        save_transaction(root, &transaction)?;
        finish_removal(root, &transaction)?;
        return Ok(true);
    }
    Ok(false)
}

async fn selected_receipt(
    args: &StandaloneArgs,
    root: &Path,
    home: &Path,
    before: Option<&Receipt>,
) -> Result<Receipt> {
    if let Some(version) = &args.activate {
        if !valid_version(version) {
            return Err(io::Error::other("activation requires a complete version number").into());
        }
        let old = before.ok_or_else(|| io::Error::other("no active standalone installation"))?;
        let variant = if args.accelerator == "auto" {
            old.accelerator_actual.as_str()
        } else {
            &args.accelerator
        };
        let bundle = format!("{version}/{}-{variant}", target());
        let path = checked_path(root, &bundle)?;
        let manifest = verify_bundle(&path)?;
        if manifest.version != *version || manifest.accelerator != variant {
            return Err(io::Error::other("retained bundle identity mismatch").into());
        }
        check_binary(&binary(&path), version).await?;
        let mut receipt = old.clone();
        receipt.version.clone_from(version);
        variant.clone_into(&mut receipt.accelerator_actual);
        receipt.accelerator_requested.clone_from(&args.accelerator);
        receipt.bundle = Some(bundle);
        return Ok(receipt);
    }
    let staging = root.join(".staging");
    let mut receipt = prepare_candidate(
        args,
        &std::env::current_exe()?.canonicalize()?,
        &staging,
        home,
    )
    .await?
    .ok_or_else(|| io::Error::other("missing install candidate"))?;
    let _cleanup = CandidateCleanup(Some(&staging));
    receipt.bundle = Some(format!(
        "{}/{}-{}",
        receipt.version, receipt.target, receipt.accelerator_actual
    ));
    let path = bundle_path(root, &receipt)?;
    if path.exists() {
        verify_bundle(&path)?;
        if fs::read(path.join(MANIFEST))? != fs::read(staging.join(MANIFEST))? {
            return Err(io::Error::other("this version and platform already contain a different package; immutable version directories cannot be overwritten").into());
        }
    } else {
        fs::create_dir_all(
            path.parent()
                .ok_or_else(|| io::Error::other("missing version parent"))?,
        )?;
        fs::rename(&staging, path)?;
    }
    Ok(receipt)
}

pub(super) fn installed_launcher() -> Result<Option<PathBuf>> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let Some(root) = executable.ancestors().nth(4) else {
        return Ok(None);
    };
    if fs::read_to_string(root.join(".zg-standalone"))
        .ok()
        .as_deref()
        != Some(OWNER)
    {
        return Ok(None);
    }
    let receipt: Receipt = read_json(&regular_path(root, RECEIPT)?)?;
    if selection::read(root)?.as_deref() != Some(key(&receipt)?) {
        return Err(
            io::Error::other("standalone activation is incomplete; rerun the installer").into(),
        );
    }
    let launcher = receipt
        .launcher
        .as_ref()
        .ok_or_else(|| io::Error::other("standalone command entry is missing"))?;
    let entry = Entry::prepare(
        root,
        launcher,
        Some(&receipt),
        Some(&bundle_path(root, &receipt)?),
    )?;
    if !entry.owned()? {
        return Err(
            io::Error::other("standalone command entry is missing; rerun the installer").into(),
        );
    }
    Ok(Some(entry.path))
}

pub(super) async fn execute(args: &StandaloneArgs) -> Result<()> {
    release_source(&args.repository)?;
    if !["auto", "cpu", "cuda"].contains(&args.accelerator.as_str()) {
        return Err(io::Error::other("invalid accelerator").into());
    }
    let root = std::path::absolute(&args.install_dir)?;
    let _lock = installation_lock(&root)?;
    let root = root.canonicalize()?;
    let mut token_file = args.token_file.clone();
    let removed = recover(&root, &mut token_file, args.remove).await?;
    let token_file = token_file.as_deref();
    let before = if root.join(RECEIPT).exists() {
        Some(read_json::<Receipt>(&regular_path(&root, RECEIPT)?)?)
    } else {
        None
    };
    if args.remove && before.is_none() {
        if removed {
            complete_installation(args, None, &root, None);
            return Ok(());
        }
        return Err(io::Error::other("no standalone installation to remove").into());
    }
    if selection::read(&root)?.as_deref() != before.as_ref().map(key).transpose()? {
        return Err(
            io::Error::other("current selection does not match the installation receipt").into(),
        );
    }
    let home = installation_home(args, &root, before.as_ref())?;
    let mut after = if args.remove {
        None
    } else {
        Some(selected_receipt(args, &root, &home, before.as_ref()).await?)
    };
    let selected = after
        .as_ref()
        .map(|receipt| bundle_path(&root, receipt))
        .transpose()?;
    let launcher = args
        .launcher
        .clone()
        .or_else(|| before.as_ref().and_then(|receipt| receipt.launcher.clone()))
        .unwrap_or_else(|| {
            if cfg!(windows) {
                binary(&root)
            } else {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/bin/zg")
            }
        });
    let entry = Entry::prepare(&root, &launcher, before.as_ref(), selected.as_deref())?;
    if let Some(old) = before
        .as_ref()
        .and_then(|receipt| receipt.launcher.as_ref())
        && entry.path != *old
    {
        return Err(io::Error::other("preserve the existing command entry when upgrading").into());
    }
    if let Some(receipt) = &mut after {
        receipt.launcher = Some(entry.path.clone());
        receipt.launcher_sha256 = entry.digest();
    }
    let mut transaction = Transaction {
        before,
        after,
        home,
        restart: None,
        entry,
        bundles: installed_bundles(&root)?,
        committed: false,
    };
    let daemon = claim(&root, &transaction, token_file).await?;
    let external = daemon.external_daemon().cloned();
    transaction.restart = daemon
        .config()
        .map(|config| Restart::from_config(config, daemon.authentication_required()));
    commit(&root, &mut transaction, &daemon, token_file).await?;
    drop(daemon);
    complete_installation(
        args,
        transaction.after.as_ref(),
        selected.as_deref().unwrap_or(&root),
        external.as_ref(),
    );
    Ok(())
}

async fn commit(
    root: &Path,
    transaction: &mut Transaction,
    daemon: &InstallationDaemon,
    token_file: Option<&Path>,
) -> Result<()> {
    // The bootstrap state also locates a recovery executable after an interrupted first install.
    if transaction.before.is_none()
        && let Some(after) = &transaction.after
    {
        save_state(root, after)?;
    }
    save_transaction(root, transaction)?;
    let result = async {
        daemon.stop().await?;
        selection::replace(
            root,
            transaction.before.as_ref().map(key).transpose()?,
            transaction.after.as_ref().map(key).transpose()?,
        )?;
        if let Some(after) = &transaction.after {
            let path = bundle_path(root, after)?;
            transaction.entry.activate(&path)?;
            save_receipt(root, Some(after))?;
            check_binary(&transaction.entry.path, &after.version).await?;
            daemon.restart(&binary(&path), daemon.config()).await?;
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    if let Err(error) = result {
        if let Err(recovery) = rollback(root, transaction, daemon, token_file).await {
            return Err(io::Error::other(format!(
                "installation failed: {error}; recovery is pending: {recovery}"
            ))
            .into());
        }
        return Err(error);
    }
    if transaction.after.is_none() {
        transaction.committed = true;
        save_transaction(root, transaction)?;
        finish_removal(root, transaction)?;
    } else {
        unlink(&root.join(JOURNAL))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_identity_includes_the_complete_version_and_variant() {
        for version in ["0.3.1", "1.12.345", "0.3.1-rc.2"] {
            assert!(valid_version(version));
        }
        for version in ["0.3", "../1.2.3", "1.2.3/other", "1.2.3-", "1.2.3\n"] {
            assert!(!valid_version(version));
        }
        for variant in ["cpu", "cuda"] {
            assert!(validate_bundle_key(&format!("0.3.1/{}-{variant}", target())).is_ok());
        }
        assert!(validate_bundle_key("0.3.1/../../other").is_err());
    }
}
