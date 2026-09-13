use std::{fs, io::Read, path::Path};

use flate2::read::GzDecoder;

use crate::{
    EngineError, EngineResult,
    utils::{atomic_write, create_directories, sha256_hex},
};

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
    create_directories(path)
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
        atomic_write(&destination, &bytes)
            .map_err(|error| io_error("persist dictionary", &destination, &error))?;
    }
    Ok(())
}

fn checksum_matches(bytes: &[u8], checksum: &str) -> bool {
    sha256_hex(bytes) == checksum
}

fn io_error(action: &str, path: &Path, error: &std::io::Error) -> EngineError {
    EngineError::from_io(format!("cannot {action} {}", path.display()), error)
}
