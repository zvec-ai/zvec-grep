use super::*;

fn minilm() -> TransformersConfig {
    crate::models::get_embedding_model_catalog_entry("local/all-minilm-l6-v2")
        .and_then(crate::models::EmbeddingCatalogEntry::transformers_config)
        .expect("MiniLM")
}

#[test]
fn execution_budget_uses_hardware_for_cpu_and_two_lanes_for_cuda() {
    assert_eq!(cpu_threads(0), 1);
    assert_eq!(cpu_threads(usize::MAX), hardware_threads());
    let entry = minilm();
    for requested in [0, 1, 2, 4, usize::MAX] {
        assert_eq!(
            execution_limit(entry, TransformersExecutionProvider::Cuda, requested),
            requested.clamp(1, 2)
        );
    }
    let mut other = entry;
    other.reference = "local/other-model";
    assert_eq!(
        execution_limit(other, TransformersExecutionProvider::Cuda, 4),
        2
    );
}

#[test]
fn batch_limits_are_static_and_independent_of_execution_lanes() {
    let entry = minilm();
    assert!(BatchPolicy::fits(entry, 16, 256));
    assert!(!BatchPolicy::fits(entry, 17, 64));
    assert!(!BatchPolicy::fits(entry, 16, 257));
    assert!(!BatchPolicy::fits(entry, usize::MAX, usize::MAX));
    assert_eq!(BatchPolicy::length_bucket(65), 128);
    assert_eq!(BatchPolicy::length_bucket(128), 128);
    assert_eq!(BatchPolicy::length_bucket(129), 256);
}
