//! Fixed PP-S input contract, parity-checked against the frozen Paddle/OpenCV input.
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "Image dimensions are validated below 64 MiB; resize quantization deliberately reproduces the reference arithmetic"
)]
use crate::models::{ModelError, layout::LayoutImage};
use std::collections::HashMap;
pub(super) const SIDE: usize = 480;

// OpenCV resize.cpp INTER_CUBIC uint8 path. Coefficients use A=-0.75,
// 11 fractional bits, ties-to-even rounding; samples replicate borders.
// Explicit FMA mirrors scalar compiler contraction in the ARM64 OpenCV 5.0.0
// baseline; final vertical FMA mirrors its NEON implementation. These choices
// are validated against fixture tensors, not assumed portable across cv2 builds.
fn cubic_weights(x: f32) -> [i32; 4] {
    let a = -0.75_f32;
    let c0 = a
        .mul_add(x + 1., -5. * a)
        .mul_add(x + 1., 8. * a)
        .mul_add(x + 1., -4. * a);
    let c1 = ((a + 2.).mul_add(x, -(a + 3.)) * x).mul_add(x, 1.);
    let c2 = ((a + 2.).mul_add(1. - x, -(a + 3.)) * (1. - x)).mul_add(1. - x, 1.);
    let c3 = 1. - c0 - c1 - c2;
    [c0, c1, c2, c3].map(|c| (c * 2048.).round_ties_even() as i32)
}

fn axis(src_len: usize) -> Vec<(i32, [i32; 4])> {
    // OpenCV computes inverse then reciprocal, with float cast before floor.
    let scale = 1.0 / (SIDE as f64 / src_len as f64);
    (0..SIDE)
        .map(|d| {
            let f = (d as f64 + 0.5).mul_add(scale, -0.5) as f32;
            let s = f.floor() as i32;
            (s, cubic_weights(f - s as f32))
        })
        .collect()
}

fn resize_rgb(rgb: &[u8], width: usize, height: usize) -> Result<Vec<u8>, ModelError> {
    if width == 0 || height == 0 || rgb.len() != width * height * 3 {
        return Err(ModelError::invalid_argument(
            "Layout RGB byte count does not match its dimensions",
        ));
    }
    if width == SIDE && height == SIDE {
        return Ok(rgb.to_vec());
    }
    let xs = axis(width);
    let ys = axis(height);
    let mut dst = vec![0; SIDE * SIDE * 3];
    // Horizontal row cache reduces each source row to 480 RGB pixels once.
    let mut row_cache: HashMap<usize, Vec<i32>> = HashMap::new();
    for (dy, (sy, wy)) in ys.into_iter().enumerate() {
        let source_ys = [0, 1, 2, 3].map(|k| (sy + k - 1).clamp(0, height as i32 - 1) as usize);
        for &y in &source_ys {
            row_cache.entry(y).or_insert_with(|| {
                let mut row = vec![0; SIDE * 3];
                for (dx, &(sx, wx)) in xs.iter().enumerate() {
                    for c in 0..3 {
                        let mut sum = 0;
                        for (k, w) in wx.iter().enumerate() {
                            let x = (sx + k as i32 - 1).clamp(0, width as i32 - 1) as usize;
                            sum += i32::from(rgb[(y * width + x) * 3 + c]) * w;
                        }
                        row[dx * 3 + c] = sum;
                    }
                }
                row
            });
        }
        let source_rows = source_ys.map(|y| &row_cache[&y]);
        let beta = wy.map(|v| v as f32 * (1.0_f32 / (2048. * 2048.)));
        for x in 0..SIDE * 3 {
            // Match VResizeCubicVec_32s8u on the ARM64 OpenCV baseline:
            // vector lanes convert horizontal integers to f32, scale beta,
            // nested FMA from bottom row to top, then nearest-even rounding.
            // 480 RGB columns are divisible by its 8-channel vector width.
            let row = source_rows.map(|r| r[x] as f32);
            let sum = row[0].mul_add(
                beta[0],
                row[1].mul_add(beta[1], row[2].mul_add(beta[2], row[3] * beta[3])),
            );
            dst[dy * SIDE * 3 + x] = sum.round_ties_even().clamp(0., 255.) as u8;
        }
        // At most four cached rows survive each destination scanline.
        row_cache.retain(|y, _| *y >= source_ys[0]);
    }
    Ok(dst)
}

pub(super) fn preprocess(image: &LayoutImage) -> Result<Vec<f32>, ModelError> {
    image.validate()?;
    let (rgb, width, height) = (image.rgb.as_ref(), image.width, image.height);
    let resized = resize_rgb(rgb, width, height)?;
    let mean = [0.485_f32, 0.456, 0.406];
    let std = [0.229_f32, 0.224, 0.225];
    let mut tensor = vec![0.; 3 * SIDE * SIDE];
    for c in 0..3 {
        for p in 0..SIDE * SIDE {
            tensor[c * SIDE * SIDE + p] =
                (f32::from(resized[p * 3 + c]) * (1.0_f32 / 255.) - mean[c]) / std[c];
        }
    }
    Ok(tensor)
}
