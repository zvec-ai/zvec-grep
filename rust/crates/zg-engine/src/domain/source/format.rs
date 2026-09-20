use std::{
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    path::Path,
};

use crate::{EngineError, EngineResult};

mod catalog;
mod sniff;
#[cfg(test)]
mod tests;

pub use catalog::FileFormat;

const HEADER_BYTES: usize = 1024;

/// Keep numeric IDs stable; do not renumber or reuse existing IDs.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileCategory {
    Unknown = 0,
    Archive = 1,
    Data = 2,
    Document = 3,
    Image = 4,
    Audio = 5,
    Video = 6,
    Code = 7,
    Binary = 8,
}

impl FileCategory {
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "unknown" => Some(Self::Unknown),
            "archive" => Some(Self::Archive),
            "data" => Some(Self::Data),
            "document" => Some(Self::Document),
            "image" => Some(Self::Image),
            "audio" => Some(Self::Audio),
            "video" => Some(Self::Video),
            "code" => Some(Self::Code),
            "binary" => Some(Self::Binary),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Archive => "archive",
            Self::Data => "data",
            Self::Document => "document",
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Code => "code",
            Self::Binary => "binary",
        }
    }
}

impl serde::Serialize for FileFormat {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for FileFormat {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::parse(&name)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown file format {name:?}")))
    }
}

impl FileFormat {
    /// Matches catalog suffixes and basenames exactly, then probes contents if needed.
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
        let needs_sniff = formats.is_empty() || catalog::needs_sniff(extension_formats);
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
        if formats.is_empty() {
            formats.push(Self::Unknown);
        }
        Ok(formats)
    }
}

fn match_longest_extension(file_name: &OsStr) -> &'static [FileFormat] {
    let file_name_bytes = file_name.as_encoded_bytes();
    file_name_bytes
        .iter()
        .enumerate()
        .filter(|(index, byte)| *index > 0 && **byte == b'.')
        .find_map(|(dot_index, _)| {
            let extension_bytes = &file_name_bytes[dot_index + 1..];
            if extension_bytes.len() > catalog::MAX_EXTENSION_LEN {
                return None;
            }
            let extension = std::str::from_utf8(extension_bytes).ok()?;
            let candidates = catalog::lookup_extension(extension);
            (!candidates.is_empty()).then_some(candidates)
        })
        .unwrap_or(&[])
}

fn normalize_formats(formats: &mut Vec<FileFormat>) {
    formats.sort_unstable();
    formats.dedup();
    if formats
        .iter()
        .any(|format| !matches!(format, FileFormat::Text | FileFormat::Unknown))
    {
        formats.retain(|format| !matches!(format, FileFormat::Text | FileFormat::Unknown));
    }
}
