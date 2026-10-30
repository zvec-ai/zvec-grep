use super::*;

fn minilm() -> TransformersConfig {
    crate::models::get_embedding_model_catalog_entry("local/all-minilm-l6-v2")
        .and_then(crate::models::EmbeddingCatalogEntry::transformers_config)
        .expect("MiniLM")
}

#[test]
fn concurrency_is_a_bounded_compute_budget() {
    assert_eq!(cpu_threads(1), 1);
    assert!((1..=crate::models::runtime::LOCAL_CONCURRENCY_CAP).contains(&cpu_threads(usize::MAX)));
    let entry = minilm();
    assert_eq!(
        execution_limit(entry, TransformersExecutionProvider::Cuda, 1),
        1
    );
    assert_eq!(
        execution_limit(entry, TransformersExecutionProvider::Cuda, usize::MAX),
        4
    );
    assert_eq!(
        execution_limit(entry, TransformersExecutionProvider::DirectMl, 4),
        1
    );
    let mut other = entry;
    other.reference = "local/unmeasured-model";
    assert_eq!(
        execution_limit(other, TransformersExecutionProvider::Cuda, 4),
        1
    );
}

#[test]
fn padded_shape_and_recent_queries_bound_merged_work() {
    let entry = minilm();
    let policy = BatchPolicy::default();
    assert!(policy.fits(entry, 16, 64, false));
    assert!(!policy.fits(entry, 32, 32, false));
    assert!(policy.fits(entry, 8, 160, false));
    assert!(!policy.fits(entry, 16, 160, false));
    assert!(!policy.fits(entry, 8, 256, false));
    assert!(!policy.fits(entry, 8, 160, true));
    assert_eq!(BatchPolicy::request_limit(entry, 4, true), 2);
    assert!(!policy.fits(entry, usize::MAX, usize::MAX, false));
}

#[test]
fn slow_batches_reduce_the_budget_after_warmup() {
    let entry = minilm();
    let mut policy = BatchPolicy::default();
    for _ in 0..3 {
        policy.observe(16, 64, Duration::from_millis(40), 4);
    }
    assert!(policy.fits(entry, 16, 64, false));
    policy.observe(16, 64, Duration::from_millis(40), 4);
    assert!(!policy.fits(entry, 16, 64, false));
    assert!(policy.fits(entry, 8, 64, false));
}
