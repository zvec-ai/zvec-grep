use std::{fs, io::Read, path::Path};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use crate::{EngineError, EngineResult};

use super::backend::{atomic_write, io_error};

const DICTIONARIES: [(&str, &[u8], &str); 2] = [
    (
        "jieba.dict.utf8",
        include_bytes!("../../resources/jieba/jieba.dict.utf8.gz"),
        "6f7d4350e8861ef4139b2e3a6fad05430c19ae71f4b8378190edecac8aae2e6a",
    ),
    (
        "hmm_model.utf8",
        include_bytes!("../../resources/jieba/hmm_model.utf8.gz"),
        "f17790586ac86dd048c8adffed052c4bd2b28ed0682972c1275e59040c0589a7",
    ),
];

pub(super) fn prepare(path: &Path) -> EngineResult<()> {
    fs::create_dir_all(path)
        .map_err(|error| io_error("create dictionary directory", path, &error))?;
    for (name, compressed, checksum) in DICTIONARIES {
        let destination = path.join(name);
        if fs::read(&destination).is_ok_and(|bytes| checksum_matches(&bytes, checksum)) {
            continue;
        }
        let mut bytes = Vec::new();
        GzDecoder::new(compressed)
            .read_to_end(&mut bytes)
            .map_err(|error| io_error("decode bundled dictionary", &destination, &error))?;
        if !checksum_matches(&bytes, checksum) {
            return Err(EngineError::storage_failure(
                "bundled dictionary checksum mismatch",
            ));
        }
        atomic_write(&destination, &bytes)?;
    }
    Ok(())
}

fn checksum_matches(bytes: &[u8], checksum: &str) -> bool {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut actual = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        actual.push(char::from(HEX[usize::from(byte >> 4)]));
        actual.push(char::from(HEX[usize::from(byte & 15)]));
    }
    actual == checksum
}
