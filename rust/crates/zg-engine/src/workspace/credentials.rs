//! Encrypts saved API keys as a best-effort measure to avoid plaintext storage.
//! Anyone with both the manifest and `credentials.key` can decrypt them, so
//! sharing the complete workspace also shares access to its saved credentials.

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

fn random_bytes(bytes: &mut [u8]) -> EngineResult<()> {
    getrandom::fill(bytes)
        .map_err(|_| EngineError::internal("could not obtain randomness for workspace credentials"))
}

fn load_credentials_key(
    layout: &WorkspaceLayout,
    create_if_missing: bool,
) -> EngineResult<[u8; 32]> {
    let key_path = layout.credentials_key_path();
    let storage_dir = layout.storage_dir();
    let exists = file_exists(key_path)?;
    if create_if_missing && !exists {
        let mut key = [0_u8; 32];
        random_bytes(&mut key)?;
        // Temporary files are private; publish without replacing a concurrent creator's key.
        let mut temporary = tempfile::NamedTempFile::new_in(storage_dir).map_err(|error| {
            EngineError::from_io(
                format!("cannot create credentials key '{}'", key_path.display()),
                &error,
            )
        })?;
        temporary
            .write_all(&key)
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|error| {
                EngineError::from_io(
                    format!("cannot write credentials key '{}'", key_path.display()),
                    &error,
                )
            })?;
        if let Err(error) = temporary.persist_noclobber(key_path)
            && error.error.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(EngineError::from_io(
                format!("cannot save credentials key '{}'", key_path.display()),
                &error.error,
            ));
        }
        sync_directory(storage_dir)?;
    }
    let key = fs::read(key_path).map_err(|error| {
        EngineError::from_io(
            format!("cannot read credentials key '{}'", key_path.display()),
            &error,
        )
    })?;
    let length = key.len();
    key.try_into().map_err(|_| {
        EngineError::invalid_argument(format!(
            "invalid credentials key '{}': expected 32 bytes, found {length}",
            key_path.display()
        ))
    })
}

pub(crate) fn delete_credentials_key(layout: &WorkspaceLayout) -> EngineResult<()> {
    let key_path = layout.credentials_key_path();
    reject_symlink(key_path)?;
    match fs::remove_file(key_path) {
        Ok(()) => sync_directory(layout.storage_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(EngineError::from_io("remove credentials key", &error)),
    }
}

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
    let key = load_credentials_key(layout, true)?;
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
            "cannot decrypt saved API key for model '{model_ref}' using credentials key '{}'",
            layout.credentials_key_path().display()
        ))
    };
    if encrypted.nonce.len() != 24 {
        return Err(invalid());
    }
    let key = load_credentials_key(layout, false)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_round_trip() {
        let directory = tempfile::tempdir().expect("directory");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        fs::create_dir(layout.storage_dir()).expect("storage directory");

        let original = encrypt_api_key(&layout, "remote/model", "secret").expect("encrypt");
        let key = fs::read(layout.credentials_key_path()).expect("credentials key");
        let updated =
            encrypt_api_key(&layout, "remote/model", "new-secret").expect("encrypt again");

        assert_eq!(
            fs::read(layout.credentials_key_path()).expect("same key"),
            key
        );
        assert_eq!(
            decrypt_api_key(&original, &layout, "remote/model").expect("decrypt original"),
            "secret"
        );
        assert_eq!(
            decrypt_api_key(&updated, &layout, "remote/model").expect("decrypt updated"),
            "new-secret"
        );
    }

    #[test]
    fn invalid_credentials_are_rejected() {
        let directory = tempfile::tempdir().expect("directory");
        let layout = WorkspaceLayout::new(directory.path().join(".zvec-grep")).expect("layout");
        fs::create_dir(layout.storage_dir()).expect("storage directory");

        for value in ["", " \t\n"] {
            assert!(encrypt_api_key(&layout, "remote/model", value).is_err());
        }
        assert!(!layout.credentials_key_path().exists());

        let sealed = encrypt_api_key(&layout, "remote/model", "secret").expect("seal");
        assert!(decrypt_api_key(&sealed, &layout, "remote/other").is_err());

        let mut modified = sealed.clone();
        modified.ciphertext[0] ^= 1;
        assert!(decrypt_api_key(&modified, &layout, "remote/model").is_err());
        modified = sealed.clone();
        modified.nonce.clear();
        assert!(decrypt_api_key(&modified, &layout, "remote/model").is_err());
        modified = sealed.clone();
        modified.nonce[0] ^= 1;
        assert!(decrypt_api_key(&modified, &layout, "remote/model").is_err());

        let mut wrong_key = fs::read(layout.credentials_key_path()).expect("credentials key");
        wrong_key[0] ^= 1;
        fs::write(layout.credentials_key_path(), wrong_key).expect("replace key");
        assert!(decrypt_api_key(&sealed, &layout, "remote/model").is_err());

        for invalid_key in [b"".as_slice(), b"invalid".as_slice()] {
            fs::write(layout.credentials_key_path(), invalid_key).expect("invalid key length");
            assert!(decrypt_api_key(&sealed, &layout, "remote/model").is_err());
            assert!(encrypt_api_key(&layout, "remote/model", "secret").is_err());
        }
        fs::remove_file(layout.credentials_key_path()).expect("remove key");
        assert!(decrypt_api_key(&sealed, &layout, "remote/model").is_err());
        assert!(!layout.credentials_key_path().exists());

        fs::create_dir(layout.credentials_key_path()).expect("directory in place of key file");
        for create_if_missing in [false, true] {
            assert!(load_credentials_key(&layout, create_if_missing).is_err());
        }
    }
}
