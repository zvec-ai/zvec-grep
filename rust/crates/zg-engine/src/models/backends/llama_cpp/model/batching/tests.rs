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
    let plan = LlamaBatchPlan::new(&inputs, 2_048, Some(128)).expect("plan");
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
fn larger_token_budgets_pack_more_sequences_without_changing_inputs() {
    let inputs = inputs(&[64; 16]);
    for (budget, batches, sequences) in [(128, 8, 2), (256, 4, 4), (512, 2, 8), (1_024, 1, 16)] {
        let plan = LlamaBatchPlan::new(&inputs, 2_048, Some(budget)).expect("plan");
        assert_eq!(plan.ranges.len(), batches);
        assert!(plan.ranges.iter().all(|range| range.len() == sequences));
        assert_eq!(plan.token_capacity as usize, budget);
        assert_eq!(plan.sequence_capacity as usize, sequences);
        let result = plan
            .execute(&inputs, None, |batch| Ok(vec![vec![1.0]; batch.len()]))
            .expect("inference");
        assert_eq!(
            result.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            inputs.iter().map(|input| input.index).collect::<Vec<_>>()
        );
    }
}

#[test]
fn mixed_lengths_respect_token_budget_and_keep_oversized_inputs_separate() {
    let inputs = inputs(&[20, 100, 9, 300, 4, 124, 129, 1]);
    let plan = LlamaBatchPlan::new(&inputs, 2_048, Some(128)).expect("plan");
    assert_eq!(plan.ranges, [0..2, 2..3, 3..4, 4..6, 6..7, 7..8]);
    let mut submitted = Vec::new();
    plan.execute(&inputs, None, |batch| {
        submitted.push(batch.iter().map(|input| input.tokens.len()).sum::<usize>());
        Ok(vec![vec![1.0]; batch.len()])
    })
    .expect("inference");
    assert_eq!(submitted, [120, 9, 300, 128, 129, 1]);
    assert_eq!(plan.token_capacity, 512);
    assert_eq!(plan.sequence_capacity, 2);
}

#[test]
fn reserves_aligned_capacity_for_non_power_of_two_sequence_counts() {
    for count in [3, 5, 7, 15] {
        let plan = LlamaBatchPlan::new(&inputs(&vec![8; count]), 2_048, Some(128)).expect("plan");
        assert_eq!(plan.ranges.len(), 1);
        assert!(plan.sequence_capacity as usize >= count);
        assert_eq!(plan.token_capacity % plan.sequence_capacity, 0);
    }
}

#[test]
fn absent_budget_keeps_cpu_and_unsupported_models_single_sequence() {
    let plan = LlamaBatchPlan::new(&inputs(&[32, 65, 2_048]), 8_192, None).expect("plan");
    assert_eq!(plan.ranges, [0..1, 1..2, 2..3]);
    assert_eq!(plan.token_capacity, 2_048);
    assert_eq!(plan.sequence_capacity, 1);
}

#[test]
fn validates_inputs_before_native_dispatch() {
    for lengths in [&[0][..], &[2_049][..], &[10, 2_049][..]] {
        for budget in [None, Some(128)] {
            assert!(LlamaBatchPlan::new(&inputs(lengths), 2_048, budget).is_err());
        }
    }
    assert!(LlamaBatchPlan::new(&inputs(&[1]), 2_048, Some(0)).is_err());
    assert!(LlamaBatchPlan::new(&inputs(&[1]), 0, None).is_err());
}

#[test]
fn context_size_caps_the_packing_budget() {
    let plan = LlamaBatchPlan::new(&inputs(&[64, 64, 1]), 128, Some(1_024)).expect("small context");
    assert_eq!(plan.ranges, [0..2, 2..3]);
    assert_eq!(plan.token_capacity, 128);
    assert_eq!(plan.sequence_capacity, 2);
}

#[test]
fn empty_input_does_not_dispatch_native_work() {
    let plan = LlamaBatchPlan::new(&[], 2_048, Some(128)).expect("empty plan");
    assert!(plan.ranges.is_empty());
    assert!(
        plan.execute(&[], None, |_| panic!("empty work must not be submitted"))
            .expect("empty output")
            .is_empty()
    );
}

#[test]
fn physical_batches_own_whole_inputs_and_preserve_original_indices() {
    let original = inputs(&[32, 64, 8_192, 16, 16]);
    let plan = LlamaBatchPlan::new(&original, 8_192, Some(256)).expect("plan");
    let batches = plan.into_batches(original.clone());
    assert_eq!(batches.iter().map(Vec::len).collect::<Vec<_>>(), [2, 1, 2]);
    assert_eq!(batches[1][0].tokens.len(), 8_192);
    let flattened = batches.into_iter().flatten().collect::<Vec<_>>();
    for (actual, expected) in flattened.iter().zip(&original) {
        assert_eq!(actual.index, expected.index);
        assert_eq!(actual.tokens, expected.tokens);
    }
}

#[test]
fn cancellation_stops_dispatch_between_batches_and_before_first_batch() {
    let inputs = inputs(&[512, 512, 512]);
    let plan = LlamaBatchPlan::new(&inputs, 2_048, Some(128)).expect("plan");
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
    let plan = LlamaBatchPlan::new(&inputs, 2_048, Some(128)).expect("plan");
    let mut calls = 0;
    assert!(
        plan.execute(&inputs, None, |batch| {
            calls += 1;
            if calls == 2 {
                Err(ModelError::internal("native failure"))
            } else {
                Ok(vec![vec![1.0]; batch.len()])
            }
        })
        .is_err()
    );
    assert_eq!(calls, 2);
    let mut mismatch_calls = 0;
    assert!(
        plan.execute(&inputs, None, |_| {
            mismatch_calls += 1;
            Ok(Vec::new())
        })
        .is_err()
    );
    assert_eq!(mismatch_calls, 1);
    let result = plan
        .execute(&inputs, None, |batch| Ok(vec![vec![1.0]; batch.len()]))
        .expect("retry");
    assert_eq!(result.len(), inputs.len());
}
