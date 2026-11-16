use super::*;

#[test]
fn batch_override_only_caps_cuda() {
    let cuda = TransformersExecutionProvider::Cuda;
    let cpu = TransformersExecutionProvider::Cpu;
    assert_eq!(batch_size(cuda, 2, Some("1")), 1);
    assert_eq!(batch_size(cuda, 2, Some("4")), 2);
    assert_eq!(batch_size(cuda, 2, Some("0")), 2);
    assert_eq!(batch_size(cuda, 2, None), 2);
    assert_eq!(batch_size(cpu, 2, Some("1")), 2);
}

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
