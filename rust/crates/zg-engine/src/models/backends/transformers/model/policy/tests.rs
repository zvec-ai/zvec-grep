use super::*;

fn minilm() -> TransformersConfig {
    crate::models::get_embedding_model_catalog_entry("local/all-minilm-l6-v2")
        .and_then(crate::models::EmbeddingCatalogEntry::transformers_config)
        .expect("MiniLM")
}

#[test]
fn execution_budget_uses_hardware_for_cpu_and_configured_pool_for_cuda() {
    assert_eq!(cpu_threads(0), 1);
    assert_eq!(cpu_threads(usize::MAX), hardware_threads());
    for sessions in [1, 2, 4, 8] {
        for requested in [0, 1, 2, 4, usize::MAX] {
            assert_eq!(
                execution_limit(TransformersExecutionProvider::Cuda, requested, sessions),
                requested.clamp(1, sessions)
            );
            assert_eq!(
                threads_per_session(requested, sessions),
                (requested.clamp(1, hardware_threads()) / sessions).max(1)
            );
        }
    }
    assert_eq!(threads_per_session(1, 4), 1);
}

#[test]
fn session_setting_is_a_positive_count_with_default_one() {
    assert_eq!(parse_session_count(None), Some(1));
    for value in ["0", "-1", "", "many", "1.5"] {
        assert_eq!(parse_session_count(Some(value)), None);
    }
    assert_eq!(parse_session_count(Some(" 4 ")), Some(4));
    for provider in [
        TransformersExecutionProvider::Cpu,
        TransformersExecutionProvider::Cuda,
    ] {
        assert_eq!(pool_size(provider, 4), 4);
    }
    assert_eq!(pool_size(TransformersExecutionProvider::CoreMl, 4), 1);
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
