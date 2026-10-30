use super::*;
use crate::models::backends::llama_cpp::model::LlamaToken;

fn inputs(lengths: &[usize]) -> Vec<PreparedLlamaInput> {
    lengths
        .iter()
        .enumerate()
        .map(|(index, &length)| PreparedLlamaInput {
            index: index * 3 + 1,
            tokens: vec![LlamaToken::new(7); length],
        })
        .collect()
}

#[test]
fn packs_short_sequences_and_keeps_long_sequences_whole() {
    let inputs = inputs(&[32, 32, 64, 65, 2_048, 33]);
    let plan = LlamaBatchPlan::new(&inputs, 2_048, true).expect("plan");
    assert_eq!(plan.ranges, [0..3, 3..4, 4..5, 5..6]);
    assert_eq!(plan.token_capacity, 2_048);
    assert_eq!(plan.sequence_capacity, 4);
    let mut submitted = Vec::new();
    let result = plan
        .execute(&inputs, None, |batch| {
            submitted.push(batch.iter().map(|input| input.tokens.len()).sum::<usize>());
            Ok(batch
                .iter()
                .map(|input| {
                    vec![f32::from(
                        u16::try_from(input.index).expect("fixture index"),
                    )]
                })
                .collect())
        })
        .expect("inference");
    assert_eq!(submitted, [128, 65, 2_048, 33]);
    assert_eq!(
        result.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
        [1, 4, 7, 10, 13, 16]
    );
    assert!(result.iter().all(
        |(index, vector)| vector == &[f32::from(u16::try_from(*index).expect("fixture index"))]
    ));
}

#[test]
fn reserves_aligned_capacity_for_non_power_of_two_sequence_counts() {
    for count in [3, 5, 7, 15] {
        let plan = LlamaBatchPlan::new(&inputs(&vec![8; count]), 2_048, true).expect("plan");
        assert_eq!(plan.ranges.len(), 1);
        assert!(plan.sequence_capacity as usize >= count);
        assert_eq!(plan.token_capacity % plan.sequence_capacity, 0);
    }
}

#[test]
fn non_gemma_models_keep_single_sequence_batches() {
    let plan = LlamaBatchPlan::new(&inputs(&[32, 65, 2_048]), 8_192, false).expect("plan");
    assert_eq!(plan.ranges, [0..1, 1..2, 2..3]);
    assert_eq!(plan.token_capacity, 2_048);
    assert_eq!(plan.sequence_capacity, 1);
}

#[test]
fn validates_inputs_before_native_dispatch() {
    for lengths in [&[0][..], &[2_049][..], &[10, 2_049][..]] {
        assert!(LlamaBatchPlan::new(&inputs(lengths), 2_048, true).is_err());
    }
    let plan = LlamaBatchPlan::new(&inputs(&[64, 64, 1]), 128, true).expect("small context");
    assert_eq!(plan.ranges, [0..2, 2..3]);
    assert_eq!(plan.token_capacity, 128);
}

#[test]
fn cancellation_stops_dispatch_between_batches_and_before_first_batch() {
    let inputs = inputs(&[512, 512, 512]);
    let plan = LlamaBatchPlan::new(&inputs, 2_048, true).expect("plan");
    let signal = CancellationToken::new();
    let mut calls = 0;
    assert!(
        plan.execute(&inputs, Some(&signal), |batch| {
            calls += 1;
            signal.cancel();
            Ok(vec![vec![1.0]; batch.len()])
        })
        .is_err()
    );
    assert_eq!(calls, 1);
    assert!(
        plan.execute(&inputs, Some(&signal), |_| {
            panic!("cancelled work must not be submitted")
        })
        .is_err()
    );
}

#[test]
fn failed_batch_stops_dispatch_and_plan_can_be_reused() {
    let inputs = inputs(&[512, 512, 512]);
    let plan = LlamaBatchPlan::new(&inputs, 2_048, true).expect("plan");
    let mut calls = 0;
    assert!(
        plan.execute(&inputs, None, |_| {
            calls += 1;
            Err(ModelError::internal("native failure"))
        })
        .is_err()
    );
    assert_eq!(calls, 1);
    assert!(plan.execute(&inputs, None, |_| Ok(Vec::new())).is_err());
    let result = plan
        .execute(&inputs, None, |batch| Ok(vec![vec![1.0]; batch.len()]))
        .expect("retry");
    assert_eq!(result.len(), inputs.len());
}
