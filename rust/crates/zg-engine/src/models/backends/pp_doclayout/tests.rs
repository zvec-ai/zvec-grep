use super::{
    model::decode,
    preprocess::{SIDE, preprocess},
};
use crate::models::layout::{LayoutImage, LayoutKind};

#[test]
fn pp_s_handles_constant_rgb_and_invalid_dimensions() {
    let image = LayoutImage {
        width: 1,
        height: 1,
        rgb: vec![0, 128, 255].into(),
    };
    let tensor = preprocess(&image).expect("constant image");
    assert_eq!(tensor.len(), 3 * SIDE * SIDE);
    for (channel, (value, mean, std)) in [
        (0.0_f32, 0.485_f32, 0.229_f32),
        (128.0, 0.456, 0.224),
        (255.0, 0.406, 0.225),
    ]
    .into_iter()
    .enumerate()
    {
        let expected = (value * (1.0 / 255.0) - mean) / std;
        assert!(
            tensor[channel * SIDE * SIDE..(channel + 1) * SIDE * SIDE]
                .iter()
                .all(|value| value.to_bits() == expected.to_bits())
        );
    }
    assert!(
        preprocess(&LayoutImage {
            width: usize::MAX,
            height: 2,
            rgb: vec![0; 3].into()
        })
        .is_err()
    );
}

#[test]
fn pp_s_preserves_strict_threshold_and_clips_boxes() {
    let boxes = decode(
        &[
            1.0, 0.5, 0.0, 0.0, 10.0, 10.0, 1.0, 0.5001, -1.0, -2.0, 300.0, 400.0, -1.0, 0.9, 0.0,
            0.0, 10.0, 10.0, 2.0, 0.9, 10.0, 0.0, 1.0, 10.0,
        ],
        100,
        200,
    )
    .expect("decode");
    assert_eq!(boxes.len(), 1);
    assert_eq!(boxes[0].kind, LayoutKind::Image);
    assert_eq!(
        boxes[0].bounds.map(f32::to_bits),
        [0.0_f32, 0.0, 100.0, 200.0].map(f32::to_bits)
    );
    assert!(decode(&[], 100, 200).expect("empty detections").is_empty());
    assert!(decode(&[23.0, 0.9, 0.0, 0.0, 1.0, 1.0], 100, 200).is_err());
    assert!(decode(&[2.0, f32::NAN, 0.0, 0.0, 1.0, 1.0], 100, 200).is_err());
}
