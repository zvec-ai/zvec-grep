use std::{
    fs::File,
    io::{Cursor, Read},
    path::Path,
};

use image::{ImageDecoder, ImageFormat, ImageReader};

use crate::{
    EngineError, EngineResult,
    domain::{Content, FileFormat, ImageContent, Range},
};

use super::{ExtractedEntity, ExtractedEntityFragment, ImageSource};

pub(crate) const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u64 = 40_000_000;

pub(crate) fn read_image(path: &Path) -> EngineResult<ImageContent> {
    let file =
        File::open(path).map_err(|error| EngineError::from_io("open query image", &error))?;
    let mut data = Vec::new();
    file.take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|error| EngineError::from_io("read query image", &error))?;
    let format = detected_format(&data)?;
    prepare_image(data, format)
}

fn detected_format(data: &[u8]) -> EngineResult<FileFormat> {
    match image::guess_format(data).map_err(image_error)? {
        ImageFormat::Png => Ok(FileFormat::Png),
        ImageFormat::Jpeg => Ok(FileFormat::Jpeg),
        ImageFormat::WebP => Ok(FileFormat::Webp),
        _ => Err(EngineError::unsupported(
            "images must be PNG, JPEG or static WebP",
        )),
    }
}

/// Validate bounded encoded content while preserving the original bytes.
pub(crate) fn prepare_image(data: Vec<u8>, format: FileFormat) -> EngineResult<ImageContent> {
    if data.len() as u64 > MAX_IMAGE_BYTES {
        return Err(EngineError::invalid_argument(
            "image exceeds the 10 MiB encoded input limit",
        ));
    }
    if detected_format(&data)? != format {
        return Err(EngineError::invalid_argument(
            "image bytes do not match their declared format",
        ));
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    let image_format = match format {
        FileFormat::Png => {
            let decoder =
                image::codecs::png::PngDecoder::with_limits(Cursor::new(&data), limits.clone())
                    .map_err(image_error)?;
            if decoder.is_apng().map_err(image_error)? {
                return Err(EngineError::unsupported(
                    "animated PNG images are not supported",
                ));
            }
            ImageFormat::Png
        }
        FileFormat::Jpeg => ImageFormat::Jpeg,
        FileFormat::Webp => {
            let decoder =
                image::codecs::webp::WebPDecoder::new(Cursor::new(&data)).map_err(image_error)?;
            if decoder.has_animation() {
                return Err(EngineError::unsupported(
                    "animated WebP images are not supported",
                ));
            }
            ImageFormat::WebP
        }
        _ => {
            return Err(EngineError::unsupported(
                "images must be PNG, JPEG or static WebP",
            ));
        }
    };
    let mut reader = ImageReader::with_format(Cursor::new(&data), image_format);
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(image_error)?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err(EngineError::invalid_argument(
            "image exceeds the 40 megapixel limit",
        ));
    }
    // A recognizable header alone does not establish a usable image.
    image::DynamicImage::from_decoder(decoder).map_err(image_error)?;
    ImageContent::new(data, format)
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "Result::map_err adapter consumes the decoder error"
)]
fn image_error(error: image::ImageError) -> EngineError {
    EngineError::invalid_argument(format!("invalid image: {error}"))
}

pub(super) fn extract(source: &ImageSource) -> Vec<ExtractedEntity> {
    vec![ExtractedEntity {
        index: 0,
        source_range: Range::Full,
        content: Content::Image(source.content.clone()),
        metadata: None,
        fragments: vec![ExtractedEntityFragment { range: Range::Full }],
    }]
}

#[cfg(test)]
mod tests {
    use crate::domain::{Content, FileFormat, ImageContent, Range};

    use super::super::{
        ChunkOptions, ImageSource, extract as extract_source, extract_for_indexing, test_content,
    };
    use super::extract;
    use crate::extraction::test_metadata;

    fn image_source(data: Vec<u8>) -> ImageSource {
        ImageSource {
            content: ImageContent::new(data, FileFormat::Png).expect("image content"),
        }
    }

    fn encoded(format: image::ImageFormat) -> Vec<u8> {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([1, 2, 3]),
        ));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, format).expect("encode fixture");
        bytes.into_inner()
    }

    #[test]
    fn validates_supported_formats_without_reencoding() {
        for (format, expected) in [
            (image::ImageFormat::Png, FileFormat::Png),
            (image::ImageFormat::Jpeg, FileFormat::Jpeg),
            (image::ImageFormat::WebP, FileFormat::Webp),
        ] {
            let bytes = encoded(format);
            let prepared = super::prepare_image(bytes.clone(), expected).expect("valid image");
            assert_eq!(prepared.data(), bytes);
            assert_eq!(prepared.format(), expected);
        }
    }

    #[test]
    fn rejects_corrupt_mismatched_and_oversized_images() {
        let png = encoded(image::ImageFormat::Png);
        assert!(super::prepare_image(png.clone(), FileFormat::Jpeg).is_err());
        assert!(super::prepare_image(png[..png.len() / 2].to_vec(), FileFormat::Png).is_err());
        assert!(
            super::prepare_image(
                vec![0; usize::try_from(super::MAX_IMAGE_BYTES).expect("image limit") + 1],
                FileFormat::Png
            )
            .is_err()
        );
        assert!(super::prepare_image(b"GIF89a".to_vec(), FileFormat::Gif).is_err());
    }

    fn png_chunk(kind: [u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = u32::try_from(data.len())
            .expect("length")
            .to_be_bytes()
            .to_vec();
        chunk.extend_from_slice(&kind);
        chunk.extend_from_slice(data);
        let mut crc = u32::MAX;
        for byte in &chunk[4..] {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        chunk.extend_from_slice(&(!crc).to_be_bytes());
        chunk
    }

    #[test]
    fn rejects_animation_and_excessive_dimensions_before_decoding_pixels() {
        let png = encoded(image::ImageFormat::Png);
        let mut animated = png[..33].to_vec();
        animated.extend(png_chunk(*b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0]));
        let mut frame = vec![0; 26];
        frame[4..8].copy_from_slice(&8u32.to_be_bytes());
        frame[8..12].copy_from_slice(&8u32.to_be_bytes());
        frame[20..24].copy_from_slice(&[0, 1, 0, 10]);
        animated.extend(png_chunk(*b"fcTL", &frame));
        animated.extend_from_slice(&png[33..]);
        let error = super::prepare_image(animated, FileFormat::Png).expect_err("APNG");
        assert_eq!(error.code(), crate::EngineError::UNSUPPORTED);
        let mut header = png[16..29].to_vec();
        header[..4].copy_from_slice(&10_000u32.to_be_bytes());
        header[4..8].copy_from_slice(&5_000u32.to_be_bytes());
        let mut enormous = png[..8].to_vec();
        enormous.extend(png_chunk(*b"IHDR", &header));
        enormous.extend_from_slice(&png[33..]);
        assert!(super::prepare_image(enormous, FileFormat::Png).is_err());
    }

    #[test]
    fn preserves_image_bytes_format_and_file_range() {
        let source = image_source(vec![1, 2, 3]);
        let fragments = extract(&source);
        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0].index(), 0);
        assert_eq!(fragments[0].source_range(), &Range::Full);
        assert_eq!(test_metadata(&fragments[0]), None);
        assert_eq!(test_content(&fragments[0]), Content::Image(source.content));
    }

    #[test]
    fn source_router_and_indexing_preserve_the_image_fragment() {
        let source = image_source(vec![4, 5, 6]);
        let direct = extract_source(&source, ChunkOptions::default()).expect("direct extraction");
        let indexing =
            extract_for_indexing(&source, ChunkOptions::default()).expect("indexing extraction");

        assert_eq!(indexing.len(), 1);
        assert_eq!(direct, indexing);
    }
}
