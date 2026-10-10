use std::{io::Cursor, path::PathBuf};

use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use tokio_util::sync::CancellationToken;

use super::{
    model::{EmbeddingGemma2Model, format_text, image_placeholder},
    processor::{MAX_PATCHES, PATCH_DIM, image_patches, patchify, target_size},
};
use crate::{
    domain::{
        Content, ContentKind, FileFormat, ImageContent,
        model::{Device, EmbeddingPurpose, ModelConfig},
    },
    models::{
        catalog::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry},
        runtime::ModelComputeRuntime,
        spi::{EmbeddingModel, EmbeddingOptions, EmbeddingPrepareOptions},
    },
};

fn model(cache_dir: PathBuf) -> EmbeddingGemma2Model {
    let EmbeddingCatalogEntry::EmbeddingGemma2(entry) =
        get_embedding_model_catalog_entry("local/embeddinggemma-2").expect("catalog entry")
    else {
        panic!("incorrect backend");
    };
    EmbeddingGemma2Model::new(
        entry,
        ModelConfig {
            cache_dir: Some(cache_dir),
            device: Some(Device::Cpu),
            ..ModelConfig::default()
        },
        ModelComputeRuntime::shared(),
    )
    .expect("construct without downloads")
}

fn encoded_image(color: [u8; 3], format: ImageFormat) -> Vec<u8> {
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(RgbImage::from_pixel(48, 48, Rgb(color)))
        .write_to(&mut output, format)
        .expect("encode image");
    output.into_inner()
}

#[test]
fn catalog_pins_native_multimodal_model() {
    let model = model(PathBuf::from("unused-test-cache"));
    let info = model.info();
    assert_eq!(info.dimension, 768);
    assert_eq!(info.max_input_tokens, Some(8192));
    assert_eq!(info.max_batch_size, 1);
    assert_eq!(
        info.model.content_kinds(),
        &[ContentKind::Text, ContentKind::Code, ContentKind::Image]
    );
    info.validate().expect("valid metadata");
}

#[test]
fn prompts_apply_only_to_text_and_preserve_media_order() {
    assert_eq!(
        format_text("sample", EmbeddingPurpose::Document, false),
        "title: none | text: sample"
    );
    assert_eq!(
        format_text("sample", EmbeddingPurpose::Query, false),
        "task: search result | query: sample"
    );
    assert_eq!(
        format_text("sample", EmbeddingPurpose::Query, true),
        "task: code retrieval | query: sample"
    );
    assert_eq!(image_placeholder(2), "<|image><|image|><|image|><image|>");
}

#[test]
fn resize_respects_aspect_ratio_pooling_and_budget() {
    assert_eq!(target_size(48, 48), (768, 768));
    assert_eq!(target_size(1, 40_000_000), (48, 13_440));
    for (width, height) in [(800, 600), (480, 1920), (5000, 12), (12, 5000)] {
        let (w, h) = target_size(width, height);
        assert_eq!(w % 48, 0);
        assert_eq!(h % 48, 0);
        assert!(u64::from(w) * u64::from(h) <= 645_120);
    }
}

#[test]
fn patches_use_hwc_rgb_positions_and_padding() {
    let image = RgbImage::from_fn(48, 48, |x, y| {
        Rgb([
            u8::try_from(x).expect("bounded x"),
            u8::try_from(y).expect("bounded y"),
            255,
        ])
    });
    let result = patchify(&image);
    assert_eq!(result.soft_tokens, 1);
    assert_eq!(result.pixels.len(), MAX_PATCHES * PATCH_DIM);
    assert_eq!(&result.pixels[..6], &[0.0, 0.0, 1.0, 1.0 / 255.0, 0.0, 1.0]);
    assert_eq!(
        result.pixels[PATCH_DIM].to_bits(),
        (16.0_f32 / 255.0).to_bits()
    );
    assert_eq!(&result.positions[..8], &[0, 0, 1, 0, 2, 0, 0, 1]);
    assert!(result.positions[18..].iter().all(|&value| value == -1));
    assert!(
        result.pixels[9 * PATCH_DIM..]
            .iter()
            .all(|&value| value == 0.0)
    );
}

#[test]
fn all_image_formats_decode_and_corrupt_input_fails() {
    for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
        let result =
            image_patches(&encoded_image([255, 0, 0], format)).expect("decode supported image");
        assert_eq!(result.soft_tokens, 256);
    }
    assert!(image_patches(b"not an image").is_err());
}

#[test]
fn exif_orientation_matches_the_displayed_pixels() {
    let image = RgbImage::from_fn(48, 96, |_, y| {
        if y < 48 {
            Rgb([255, 0, 0])
        } else {
            Rgb([0, 0, 255])
        }
    });
    let mut jpeg = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image)
        .write_to(&mut jpeg, ImageFormat::Jpeg)
        .expect("JPEG");
    let jpeg = jpeg.into_inner();
    // Exif little-endian TIFF: Orientation (0x0112), SHORT, value 6 (90° CW).
    let exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
    let mut oriented = jpeg[..2].to_vec();
    oriented.extend_from_slice(&[0xff, 0xe1]);
    oriented.extend_from_slice(
        &u16::try_from(exif.len() + 2)
            .expect("Exif length")
            .to_be_bytes(),
    );
    oriented.extend_from_slice(exif);
    oriented.extend_from_slice(&jpeg[2..]);
    let mut expected = Cursor::new(Vec::new());
    image::load_from_memory(&jpeg)
        .expect("decode JPEG")
        .rotate90()
        .write_to(&mut expected, ImageFormat::Png)
        .expect("PNG rotated pixels");
    let actual = image_patches(&oriented).expect("apply orientation");
    let expected = image_patches(expected.get_ref()).expect("already rotated pixels");
    assert_eq!(actual.soft_tokens, expected.soft_tokens);
    assert_eq!(actual.positions, expected.positions);
    assert_eq!(actual.pixels, expected.pixels);
}

#[tokio::test]
async fn cancellation_prevents_preparation_and_downloads() {
    let cache = tempfile::tempdir().expect("temp cache");
    let model = model(cache.path().join("models"));
    let signal = CancellationToken::new();
    signal.cancel();
    let error = model
        .prepare(EmbeddingPrepareOptions {
            signal: Some(signal.clone()),
            ..EmbeddingPrepareOptions::default()
        })
        .await
        .expect_err("cancelled preparation");
    assert_eq!(error.code(), crate::EngineError::CANCELLED);
    let error = model
        .embed(
            &[vec![Content::Text("sample".into())]],
            EmbeddingOptions {
                signal: Some(signal),
                ..EmbeddingOptions::default()
            },
        )
        .await
        .expect_err("cancelled inference");
    assert_eq!(error.code(), crate::EngineError::CANCELLED);
    assert!(!cache.path().join("models").exists());
}

#[tokio::test]
#[ignore = "downloads 315 MB pinned public artifacts; set ZVEC_GREP_MODEL_CACHE to an isolated writable directory"]
async fn real_embeddinggemma2_text_image_fusion_and_recovery() {
    let cache = std::env::var_os("ZVEC_GREP_MODEL_CACHE")
        .map(PathBuf::from)
        .expect("isolated model cache");
    let model = model(cache);
    model
        .prepare(EmbeddingPrepareOptions::default())
        .await
        .expect("download and prepare");
    let image = Content::Image(
        ImageContent::new(
            encoded_image([255, 0, 0], ImageFormat::Png),
            FileFormat::Png,
        )
        .expect("image content"),
    );
    let text = Content::Text("a red square".into());
    let mut vectors = Vec::new();
    for input in [
        vec![text.clone()],
        vec![image.clone()],
        vec![text, image.clone()],
    ] {
        let output = model
            .embed(&[input], EmbeddingOptions::default())
            .await
            .expect("real inference");
        let vector = &output.vectors[0];
        assert_eq!(vector.len(), 768);
        assert!(vector.iter().all(|value| value.is_finite()));
        let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 0.001,
            "normalized graph output: {norm}"
        );
        vectors.push(vector.clone());
    }
    assert_ne!(vectors[0], vectors[1]);
    assert_ne!(vectors[1], vectors[2]);
    let blue = Content::Image(
        ImageContent::new(
            encoded_image([0, 0, 255], ImageFormat::WebP),
            FileFormat::Webp,
        )
        .expect("blue image"),
    );
    let blue_vector = model
        .embed(&[vec![blue]], EmbeddingOptions::default())
        .await
        .expect("different image")
        .vectors
        .remove(0);
    let similarity = blue_vector
        .iter()
        .zip(&vectors[1])
        .map(|(left, right)| left * right)
        .sum::<f32>();
    assert!(
        similarity < 0.999,
        "different pixels must change the vector: {similarity}"
    );
    let invalid = Content::Image(
        ImageContent::new(vec![1, 2, 3], FileFormat::Png).expect("invalid encoded image"),
    );
    model
        .embed(&[vec![invalid]], EmbeddingOptions::default())
        .await
        .expect_err("bad image rejected");
    let recovered = model
        .embed(&[vec![image]], EmbeddingOptions::default())
        .await
        .expect("healthy sessions survive bad input");
    assert_eq!(vectors[1], recovered.vectors[0]);
    let query = [vec![Content::Text(
        "find a function that adds two numbers".into(),
    )]];
    let mut query_vectors = Vec::new();
    for target in [ContentKind::Text, ContentKind::Code] {
        query_vectors.push(
            model
                .embed(
                    &query,
                    EmbeddingOptions {
                        purpose: EmbeddingPurpose::Query,
                        query_target: Some(target),
                        ..EmbeddingOptions::default()
                    },
                )
                .await
                .expect("target-specific text query")
                .vectors
                .remove(0),
        );
    }
    assert_ne!(
        query_vectors[0], query_vectors[1],
        "the code task must change encoding without changing input kind"
    );
}
