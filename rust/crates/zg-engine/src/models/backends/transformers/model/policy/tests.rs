use super::*;

fn minilm() -> TransformersConfig {
    crate::models::get_embedding_model_catalog_entry("local/all-minilm-l6-v2")
        .and_then(crate::models::EmbeddingCatalogEntry::transformers_config)
        .expect("MiniLM")
}

#[test]
fn execution_is_capped_by_fixed_pool_for_both_cpu_and_cuda() {
    for provider in [
        TransformersExecutionProvider::Cpu,
        TransformersExecutionProvider::Cuda,
    ] {
        for sessions in [1, 2, 4, 8] {
            assert_eq!(pool_size(provider, sessions), sessions);
            for requested in [0, 1, 2, 4, usize::MAX] {
                assert_eq!(
                    execution_limit(provider, requested, sessions),
                    requested.clamp(1, sessions)
                );
            }
        }
    }
    assert_eq!(pool_size(TransformersExecutionProvider::CoreMl, 4), 1);
}

#[test]
fn batch_limits_are_static_and_independent_of_execution_lanes() {
    let entry = minilm();
    assert!(BatchPolicy::fits(entry, 4, 256));
    assert!(!BatchPolicy::fits(entry, 5, 64));
    assert!(!BatchPolicy::fits(entry, 4, 257));
    assert!(!BatchPolicy::fits(entry, usize::MAX, usize::MAX));
    assert_eq!(BatchPolicy::length_bucket(65), 128);
    assert_eq!(BatchPolicy::length_bucket(128), 128);
    assert_eq!(BatchPolicy::length_bucket(129), 256);
}
