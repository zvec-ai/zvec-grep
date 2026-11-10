use super::{DIMENSION, assert_batched_equivalent, assert_bitwise};

fn unit_vector() -> Vec<f32> {
    let mut vector = vec![0.0; DIMENSION];
    vector[0] = 0.6;
    vector[1] = 0.8;
    vector
}

#[test]
fn batched_reference_accepts_small_rounding_and_exact_zero_vectors() {
    let actual = unit_vector();
    let mut rounded = actual.clone();
    rounded[0] = f32::from_bits(actual[0].to_bits() + 2);
    assert_batched_equivalent(&actual, &rounded, 0);
    assert_batched_equivalent(&vec![0.0; DIMENSION], &vec![0.0; DIMENSION], 0);
    assert_bitwise(&actual, &actual, 0);
}

#[test]
#[should_panic(expected = "row=0 column=0")]
fn canonical_reference_still_rejects_a_single_ulp() {
    let actual = unit_vector();
    let mut rounded = actual.clone();
    rounded[0] = f32::from_bits(actual[0].to_bits() + 1);
    assert_bitwise(&actual, &rounded, 0);
}

#[test]
#[should_panic(expected = "batched ONNX component drift")]
fn batched_reference_rejects_component_errors() {
    let actual = unit_vector();
    let mut changed = actual.clone();
    changed[0] += 2e-5;
    assert_batched_equivalent(&actual, &changed, 0);
}

#[test]
#[should_panic(expected = "batched ONNX direction drift")]
fn batched_reference_rejects_accumulated_direction_errors() {
    let actual = unit_vector();
    let mut changed = actual.clone();
    changed[2..].fill(9e-6);
    assert_batched_equivalent(&actual, &changed, 0);
}

#[test]
#[should_panic(expected = "non-finite embedding")]
fn batched_reference_rejects_non_finite_components() {
    let actual = unit_vector();
    let mut changed = actual.clone();
    changed[0] = f32::NAN;
    assert_batched_equivalent(&actual, &changed, 0);
}

#[test]
#[should_panic(expected = "row=0 column=0")]
fn batched_reference_preserves_exact_zero_outputs() {
    let mut changed = vec![0.0; DIMENSION];
    changed[0] = 1e-7;
    assert_batched_equivalent(&vec![0.0; DIMENSION], &changed, 0);
}
