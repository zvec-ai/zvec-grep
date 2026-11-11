use super::*;

#[test]
fn preserves_unknown_zero_and_signed_rows_and_recovers_after_bad_id() {
    let table = Table {
        weights: vec![3, 4, 0, 2, 255, 0],
        scales: vec![2.0, 0.5, 1.0],
        dimension: 2,
    };
    assert_eq!(table.embed(&[0], None).expect("UNK retained"), [0.6, 0.8]);
    assert_eq!(table.embed(&[], None).expect("empty"), [0.0, 0.0]);
    assert!(table.embed(&[3], None).is_err());
    assert_eq!(table.embed(&[1], None).expect("recovered"), [0.0, 1.0]);
    let mixed = table.embed(&[0, 1, 2], None).expect("row-specific scales");
    let norm = 106_f64.sqrt();
    assert!((f64::from(mixed[0]) - 5.0 / norm).abs() < 1e-7);
    assert!((f64::from(mixed[1]) - 9.0 / norm).abs() < 1e-7);
}

#[test]
fn fp32_accumulation_is_not_replaced_by_fp64_or_fused_multiply_add() {
    let table = Table {
        weights: vec![1, 0, 1, 1, 255, 0],
        scales: vec![16_777_216.0, 1.0, 16_777_216.0],
        dimension: 2,
    };
    assert_eq!(
        table.embed(&[0, 1, 2], None).expect("FP32 reduction"),
        [0.0, 1.0]
    );
    // Each Mul must round before ReduceSum: (127 * s) rounds to 1,
    // then cancels -1 exactly. An FMA would retain a residual.
    let table = Table {
        weights: vec![255, 0, 127, 1],
        scales: vec![1.0, 1.0 / 127.0],
        dimension: 2,
    };
    assert_eq!(
        table.embed(&[0, 1], None).expect("separate multiply"),
        [0.0, 1.0]
    );
}

#[test]
fn cancellation_does_not_poison_the_table() {
    let table = Table {
        weights: vec![1, 0],
        scales: vec![1.0],
        dimension: 2,
    };
    let signal = CancellationToken::new();
    signal.cancel();
    for ids in [vec![], vec![0; 32_768]] {
        assert_eq!(
            table
                .embed(&ids, Some(&signal))
                .expect_err("cancelled")
                .code(),
            crate::EngineError::CANCELLED
        );
    }
    assert_eq!(table.embed(&[0], None).expect("recovery"), [1.0, 0.0]);
}
