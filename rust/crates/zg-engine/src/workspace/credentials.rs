use std::{fs, io::Write};

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};

use crate::{
    EngineError, EngineResult,
    domain::{WorkspaceLayout, model::EncryptedApiKey},
    utils::{file_exists, reject_symlink, sync_directory},
};

pub(crate) fn encrypt_api_key(
    layout: &WorkspaceLayout,
    model_ref: &str,
    value: &str,
) -> EngineResult<EncryptedApiKey> {
    if value.trim().is_empty() {
        return Err(EngineError::invalid_argument(format!(
            "API key for {model_ref} must be non-empty"
        )));
    }
    let key = workspace_key(layout, true)?;
    let mut nonce = [0_u8; 24];
    random_bytes(&mut nonce)?;
    let ciphertext = XChaCha20Poly1305::new((&key).into())
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: value.as_bytes(),
                aad: model_ref.as_bytes(),
            },
        )
        .map_err(|_| EngineError::internal(format!("could not encrypt API key for {model_ref}")))?;
    Ok(EncryptedApiKey {
        nonce: nonce.to_vec(),
        ciphertext,
    })
}

pub(crate) fn decrypt_api_key(
    encrypted: &EncryptedApiKey,
    layout: &WorkspaceLayout,
    model_ref: &str,
) -> EngineResult<String> {
    let invalid = || {
        EngineError::invalid_argument(format!(
            "cannot decrypt saved API key for {model_ref}; restore the matching {} or replace the key with `zg --index --api-key`",
            layout.credentials_key_path().display()
        ))
    };
    if encrypted.nonce.len() != 24 {
        return Err(invalid());
    }
    let key = workspace_key(layout, false)?;
    let plaintext = XChaCha20Poly1305::new((&key).into())
        .decrypt(
            XNonce::from_slice(&encrypted.nonce),
            Payload {
                msg: &encrypted.ciphertext,
                aad: model_ref.as_bytes(),
            },
        )
        .map_err(|_| invalid())?;
    String::from_utf8(plaintext).map_err(|_| invalid())
}

fn random_bytes(bytes: &mut [u8]) -> EngineResult<()> {
    getrandom::fill(bytes)
        .map_err(|_| EngineError::internal("could not obtain randomness for workspace credentials"))
}

pub(crate) fn delete_workspace_key(layout: &WorkspaceLayout) -> EngineResult<()> {
    let key_path = layout.credentials_key_path();
    reject_symlink(key_path)?;
    match fs::remove_file(key_path) {
        Ok(()) => sync_directory(layout.storage_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(EngineError::from_io(
            "remove workspace credentials key",
            &error,
        )),
    }
}

fn workspace_key(layout: &WorkspaceLayout, create: bool) -> EngineResult<[u8; 32]> {
    let key_path = layout.credentials_key_path();
    let storage_dir = layout.storage_dir();
    let exists = file_exists(key_path)?;
    if create && !exists {
        let mut key = [0_u8; 32];
        random_bytes(&mut key)?;
        // Temporary files are private; publish without replacing a concurrent creator's key.
        let mut temporary = tempfile::NamedTempFile::new_in(storage_dir)
            .map_err(|error| EngineError::from_io("create workspace credentials key", &error))?;
        temporary
            .write_all(&key)
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|error| EngineError::from_io("write workspace credentials key", &error))?;
        if let Err(error) = temporary.persist_noclobber(key_path)
            && error.error.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(EngineError::from_io(
                "publish workspace credentials key",
                &error.error,
            ));
        }
        sync_directory(storage_dir)?;
    }
    let key = fs::read(key_path).map_err(|error| {
        EngineError::from_io(
            format!(
                "read workspace credentials key {}; copy this file with the workspace",
                key_path.display()
            ),
            &error,
        )
    })?;
    key.try_into().map_err(|_| {
        EngineError::invalid_argument(format!(
            "invalid workspace credentials key {}; expected 32 bytes",
            key_path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupted_or_missing_keys_and_modified_ciphertexts_are_rejected() {
        let directory = tempfile::tempdir().expect("directory");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        fs::create_dir(layout.storage_dir()).expect("storage directory");
        let sealed = encrypt_api_key(&layout, "remote/model", "secret").expect("seal");
        let mut modified = sealed.clone();
        modified.ciphertext[0] ^= 1;
        assert!(decrypt_api_key(&modified, &layout, "remote/model").is_err());
        modified = sealed.clone();
        modified.nonce.clear();
        assert!(decrypt_api_key(&modified, &layout, "remote/model").is_err());
        fs::write(layout.credentials_key_path(), b"invalid").expect("replace key");
        assert!(decrypt_api_key(&sealed, &layout, "remote/model").is_err());
        fs::remove_file(layout.credentials_key_path()).expect("remove key");
        assert!(decrypt_api_key(&sealed, &layout, "remote/model").is_err());
        assert!(!layout.credentials_key_path().exists());
    }

    #[cfg(unix)]
    #[test]
    fn workspace_keys_reject_links_for_both_access_modes() {
        let directory = tempfile::tempdir().expect("directory");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        fs::create_dir(layout.storage_dir()).expect("storage directory");
        let target = directory.path().join("external.key");
        fs::write(&target, [7; 32]).expect("external key");
        std::os::unix::fs::symlink(&target, layout.credentials_key_path()).expect("key alias");

        for create in [false, true] {
            assert!(workspace_key(&layout, create).is_err());
        }
        assert_eq!(fs::read(target).expect("preserved key"), [7; 32]);
    }

    #[cfg(unix)]
    #[test]
    fn new_keys_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("directory");
        let layout = WorkspaceLayout::new(directory.path().to_path_buf()).expect("layout");
        fs::create_dir(layout.storage_dir()).expect("storage directory");
        encrypt_api_key(&layout, "remote/model", "secret").expect("seal");
        assert_eq!(
            fs::metadata(layout.credentials_key_path())
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
