use super::*;

#[test]
fn execution_is_capped_by_fixed_pool_for_all_multi_session_providers() {
    for provider in [
        TransformersExecutionProvider::Cpu,
        TransformersExecutionProvider::Cuda,
        TransformersExecutionProvider::DirectMl,
        TransformersExecutionProvider::WebGpu,
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
