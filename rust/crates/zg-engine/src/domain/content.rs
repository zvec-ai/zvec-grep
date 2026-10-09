use crate::{EngineError, EngineResult};

use super::{FileCategory, FileFormat};

#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Text,
    Code,
    Image,
}

impl ContentKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Code => "code",
            Self::Image => "image",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Content {
    Text(String),
    Code(String),
    Image(ImageContent),
}

impl Content {
    #[must_use]
    pub fn kind(&self) -> ContentKind {
        match self {
            Self::Text(_) => ContentKind::Text,
            Self::Code(_) => ContentKind::Code,
            Self::Image(_) => ContentKind::Image,
        }
    }

    /// Returns a platform-independent SHA-256 fingerprint of the content kind and value.
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        match self {
            Self::Text(text) => crate::utils::sha256_parts([b"text", text.as_bytes()]),
            Self::Code(text) => crate::utils::sha256_parts([b"code", text.as_bytes()]),
            Self::Image(image) => image.fingerprint(),
        }
    }
}

// --- Image ---

/// Stores a complete encoded image resource and its format.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ImageContent {
    #[serde(with = "crate::utils::base64_bytes")]
    data: Vec<u8>,
    format: FileFormat,
}

impl ImageContent {
    /// # Errors
    /// Returns an error if data is empty or the format is not an image.
    #[track_caller]
    pub fn new(data: Vec<u8>, format: FileFormat) -> EngineResult<Self> {
        if !format.categories().contains(&FileCategory::Image) {
            return Err(EngineError::invalid_argument(format!(
                "image content requires an image format, got {}",
                format.as_str()
            )));
        }
        if data.is_empty() {
            return Err(EngineError::invalid_argument(
                "image content requires non-empty data",
            ));
        }
        Ok(Self { data, format })
    }

    #[must_use]
    pub fn format(&self) -> FileFormat {
        self.format
    }

    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    fn fingerprint(&self) -> [u8; 32] {
        crate::utils::sha256_parts([b"image", self.data.as_slice()])
    }
}

impl<'de> serde::Deserialize<'de> for ImageContent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ImageData {
            #[serde(with = "crate::utils::base64_bytes")]
            data: Vec<u8>,
            format: FileFormat,
        }

        let image = ImageData::deserialize(deserializer)?;
        Self::new(image.data, image.format).map_err(serde::de::Error::custom)
    }
}
