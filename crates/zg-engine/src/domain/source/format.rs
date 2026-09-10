use std::{
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    path::Path,
};

use crate::{EngineError, EngineResult};

mod catalog;
mod sniff;

pub(crate) use catalog::FileFormat;

const HEADER_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum FileCategory {
    Unknown,
    Archive,
    Data,
    Document,
    Image,
    Audio,
    Video,
    Code,
    Binary,
}

impl FileFormat {
    /// Infers formats from the file name and, if needed, its contents.
    pub(crate) fn from_path(path: &Path) -> EngineResult<Vec<Self>> {
        let file_name = path.file_name().ok_or_else(|| {
            EngineError::invalid_argument(format!(
                "cannot detect file format: path must include a file name: {}",
                path.display()
            ))
        })?;
        let file_name_formats = file_name.to_str().map_or(&[][..], catalog::lookup_name);
        let extension_formats = match_longest_extension(file_name);

        let mut formats = Vec::with_capacity(file_name_formats.len() + extension_formats.len());
        formats.extend_from_slice(extension_formats);
        formats.extend_from_slice(file_name_formats);
        let needs_sniff = formats.is_empty()
            || (extension_formats.len() > 1 && catalog::needs_sniff(extension_formats));
        if !needs_sniff {
            normalize_formats(&mut formats);
            return Ok(formats);
        }

        let metadata = fs::metadata(path).map_err(|error| {
            EngineError::from_io(
                format!(
                    "cannot detect file format: failed to inspect {}",
                    path.display()
                ),
                &error,
            )
        })?;
        if !metadata.is_file() {
            return Err(EngineError::invalid_argument(format!(
                "cannot detect file format: expected a regular file: {}",
                path.display()
            )));
        }

        let file = File::open(path).map_err(|error| {
            EngineError::from_io(
                format!(
                    "cannot detect file format: failed to open {}",
                    path.display()
                ),
                &error,
            )
        })?;
        // One extra byte distinguishes EOF from a truncated sample.
        let mut header = Vec::with_capacity(HEADER_BYTES + 1);
        file.take((HEADER_BYTES + 1) as u64)
            .read_to_end(&mut header)
            .map_err(|error| {
                EngineError::from_io(
                    format!(
                        "cannot detect file format: failed to read {}",
                        path.display()
                    ),
                    &error,
                )
            })?;
        let complete = header.len() <= HEADER_BYTES;
        header.truncate(HEADER_BYTES);
        // Refine only extension candidates, then restore the exact-name matches.
        formats.truncate(extension_formats.len());
        formats = sniff::refine(formats, &header, complete);
        formats.extend_from_slice(file_name_formats);
        normalize_formats(&mut formats);
        Ok(formats)
    }
}

fn match_longest_extension(file_name: &OsStr) -> &'static [FileFormat] {
    let file_name_bytes = file_name.as_encoded_bytes();
    let mut extension_buffer = [0; catalog::MAX_EXTENSION_LEN];
    file_name_bytes
        .iter()
        .enumerate()
        .filter(|(index, byte)| *index > 0 && **byte == b'.')
        .find_map(|(dot_index, _)| {
            let extension_bytes = &file_name_bytes[dot_index + 1..];
            let lowercase_extension = extension_buffer.get_mut(..extension_bytes.len())?;
            for (target, byte) in lowercase_extension.iter_mut().zip(extension_bytes) {
                *target = byte.to_ascii_lowercase();
            }
            let extension = std::str::from_utf8(lowercase_extension).ok()?;
            let candidates = catalog::lookup_extension(extension);
            (!candidates.is_empty()).then_some(candidates)
        })
        .unwrap_or(&[])
}

fn normalize_formats(formats: &mut Vec<FileFormat>) {
    formats.sort_unstable_by_key(|format| *format as u16);
    formats.dedup();
    if formats
        .iter()
        .any(|format| !matches!(format, FileFormat::Text | FileFormat::Unknown))
    {
        formats.retain(|format| !matches!(format, FileFormat::Text | FileFormat::Unknown));
    }
}

#[cfg(test)]
mod tests;
