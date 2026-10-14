use std::{fs, io::Write, path::Path};

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use serde::{Deserialize, Serialize};

use crate::{EngineError, EngineResult, utils::sync_directory};

const KEY_FILE: &str = "credentials.key";

/// Persisted settings never contain a plaintext API key.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoredModelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<EncryptedApiKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EncryptedApiKey {
    #[serde(with = "crate::utils::base64_bytes")]
    nonce: Vec<u8>,
    #[serde(with = "crate::utils::base64_bytes")]
    ciphertext: Vec<u8>,
}

impl EncryptedApiKey {
    pub(crate) fn seal(
        home: &Path,
        model_ref: &str,
        value: &str,
        create_key: bool,
    ) -> EngineResult<Self> {
        if value.trim().is_empty() {
            return Err(EngineError::invalid_argument(format!(
                "API key for {model_ref} must be non-empty"
            )));
        }
        let key = workspace_key(home, create_key)?;
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
            .map_err(|_| {
                EngineError::internal(format!("could not encrypt API key for {model_ref}"))
            })?;
        Ok(Self {
            nonce: nonce.to_vec(),
            ciphertext,
        })
    }

    pub(crate) fn open(&self, home: &Path, model_ref: &str) -> EngineResult<String> {
        let invalid = || {
            EngineError::invalid_argument(format!(
                "cannot decrypt saved API key for {model_ref}; restore the matching {} or replace the key with `zg --index --api-key`",
                home.join(KEY_FILE).display()
            ))
        };
        if self.nonce.len() != 24 {
            return Err(invalid());
        }
        let key = workspace_key(home, false)?;
        let plaintext = XChaCha20Poly1305::new((&key).into())
            .decrypt(
                XNonce::from_slice(&self.nonce),
                Payload {
                    msg: &self.ciphertext,
                    aad: model_ref.as_bytes(),
                },
            )
            .map_err(|_| invalid())?;
        String::from_utf8(plaintext).map_err(|_| invalid())
    }
}

fn random_bytes(bytes: &mut [u8]) -> EngineResult<()> {
    getrandom::fill(bytes)
        .map_err(|_| EngineError::internal("could not obtain randomness for workspace credentials"))
}

fn workspace_key(home: &Path, create: bool) -> EngineResult<[u8; 32]> {
    let path = home.join(KEY_FILE);
    if create
        && !path
            .try_exists()
            .map_err(|error| EngineError::from_io("inspect workspace credentials key", &error))?
    {
        let mut key = [0_u8; 32];
        random_bytes(&mut key)?;
        // Temporary files are private; publish without replacing a concurrent creator's key.
        let mut temporary = tempfile::NamedTempFile::new_in(home)
            .map_err(|error| EngineError::from_io("create workspace credentials key", &error))?;
        temporary
            .write_all(&key)
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|error| EngineError::from_io("write workspace credentials key", &error))?;
        if let Err(error) = temporary.persist_noclobber(&path)
            && error.error.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(EngineError::from_io(
                "publish workspace credentials key",
                &error.error,
            ));
        }
        sync_directory(home)?;
    }
    let key = fs::read(&path).map_err(|error| {
        EngineError::from_io(
            format!(
                "read workspace credentials key {}; copy this file with the workspace",
                path.display()
            ),
            &error,
        )
    })?;
    key.try_into().map_err(|_| {
        EngineError::invalid_argument(format!(
            "invalid workspace credentials key {}; expected 32 bytes",
            path.display()
        ))
    })
}
