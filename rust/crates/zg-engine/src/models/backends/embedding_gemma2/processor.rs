use image::{ImageDecoder, ImageReader, imageops::FilterType};
use std::io::Cursor;

use crate::models::spi::ModelError;

pub(super) const PATCH_SIZE: usize = 16;
pub(super) const PATCH_DIM: usize = PATCH_SIZE * PATCH_SIZE * 3;
pub(super) const MAX_PATCHES: usize = 280 * 9;
const SIDE_MULTIPLE: u32 = 48;
const MAX_DECODED_PIXELS: u64 = 40_000_000;

pub(super) struct ImagePatches {
    pub(super) pixels: Vec<f32>,
    pub(super) positions: Vec<i64>,
    pub(super) soft_tokens: usize,
}

/// The pinned Gemma4 processor uses 280 soft tokens, RGB bicubic resizing,
/// 16x16 HWC patches, 3x3 pooling, [column, row] positions and -1 padding.
pub(super) fn image_patches(encoded: &[u8]) -> Result<ImagePatches, ModelError> {
    let reader = ImageReader::new(Cursor::new(encoded))
        .with_guessed_format()
        .map_err(image_error)?;
    let mut decoder = reader.into_decoder().map_err(image_error)?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_DECODED_PIXELS {
        return Err(ModelError::invalid_argument(
            "EmbeddingGemma 2 images must contain 1 to 40 million pixels",
        ));
    }
    let orientation = decoder.orientation().map_err(image_error)?;
    let mut image = image::DynamicImage::from_decoder(decoder).map_err(image_error)?;
    image.apply_orientation(orientation);
    let image = image.to_rgb8();
    let (target_width, target_height) = target_size(image.width(), image.height());
    let resized =
        image::imageops::resize(&image, target_width, target_height, FilterType::CatmullRom);
    Ok(patchify(&resized))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn target_size(width: u32, height: u32) -> (u32, u32) {
    // Inputs are positive and the maximum area is fixed by the model, so these
    // casts cannot overflow. Extreme aspect ratios get one full pooling cell.
    let factor = (645_120.0 / (f64::from(width) * f64::from(height))).sqrt();
    let multiple = f64::from(SIDE_MULTIPLE);
    let mut target_width = (factor * f64::from(width) / multiple).floor() as u32 * SIDE_MULTIPLE;
    let mut target_height = (factor * f64::from(height) / multiple).floor() as u32 * SIDE_MULTIPLE;
    let maximum_side = 280 * SIDE_MULTIPLE;
    if target_height == 0 {
        target_height = SIDE_MULTIPLE;
        target_width = (width / height)
            .saturating_mul(SIDE_MULTIPLE)
            .min(maximum_side);
    } else if target_width == 0 {
        target_width = SIDE_MULTIPLE;
        target_height = (height / width)
            .saturating_mul(SIDE_MULTIPLE)
            .min(maximum_side);
    }
    (target_width, target_height)
}

pub(super) fn patchify(image: &image::RgbImage) -> ImagePatches {
    let width = image.width() as usize;
    let height = image.height() as usize;
    let columns = width / PATCH_SIZE;
    let rows = height / PATCH_SIZE;
    let mut pixels = vec![0.0; MAX_PATCHES * PATCH_DIM];
    let mut positions = vec![-1; MAX_PATCHES * 2];
    let data = image.as_raw();
    let mut offset = 0;
    for row in 0..rows {
        for column in 0..columns {
            let index = row * columns + column;
            positions[2 * index] = i64::try_from(column).expect("bounded image width");
            positions[2 * index + 1] = i64::try_from(row).expect("bounded image height");
            for y in 0..PATCH_SIZE {
                let start = ((row * PATCH_SIZE + y) * width + column * PATCH_SIZE) * 3;
                for value in &data[start..start + PATCH_SIZE * 3] {
                    pixels[offset] = f32::from(*value) / 255.0;
                    offset += 1;
                }
            }
        }
    }
    ImagePatches {
        pixels,
        positions,
        soft_tokens: rows * columns / 9,
    }
}

fn image_error(error: impl std::fmt::Display) -> ModelError {
    ModelError::invalid_argument("Unable to decode EmbeddingGemma 2 image").with_cause(error)
}
