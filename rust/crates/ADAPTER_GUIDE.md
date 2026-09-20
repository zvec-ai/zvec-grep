# Adding a native implementation

Create a separate crate only when the native component is independently useful
or testable. Keep its library types and runtime policy inside that crate.

When the component becomes part of `ZvecGrep`:

1. define or reuse the typed engine request and reply;
2. put orchestration behind a private `zg-engine` service;
3. call the service directly from the corresponding `ZvecGrep` method;
4. map native failures to `EngineError`;
5. add compatibility tests against the TypeScript behavior.

Do not introduce a public adapter registry, port bundle, command dispatcher or
an `*-internal` crate merely to compose the implementation. If Rust crate
dependencies would form a cycle, move the concrete implementation behind the
engine boundary or isolate genuinely neutral value types.

Checklist:

- cancellation is observable during expensive work;
- processes, queues and workers have explicit limits;
- output order is deterministic;
- partial artifacts are cleaned up;
- native revision and artifact fingerprints are recorded where applicable.
