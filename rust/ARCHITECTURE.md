# Rust architecture

## Public application boundary

`zg-engine` has one behavioral entry point: `ZvecGrep`. Each public method
accepts a concrete request and returns its concrete reply:

```rust,ignore
ZvecGrep::new() -> ZvecGrep
ZvecGrep::query(QueryRequest) -> Result<QueryReply, EngineError>
ZvecGrep::lexical_search(LexicalSearchRequest) -> Result<LexicalSearchReply, EngineError>
ZvecGrep::index(IndexRequest) -> Result<IndexReply, EngineError>
ZvecGrep::inspect(InspectRequest) -> Result<InspectReply, EngineError>
ZvecGrep::change_index(ChangeIndexRequest) -> Result<ChangeIndexReply, EngineError>
ZvecGrep::job(JobRequest) -> Result<JobReply, EngineError>
ZvecGrep::close()
```

Every request carries an optional workspace `root`; omitting it uses the process
working directory. `ZvecGrep` owns shared engine resources rather than one
workspace, so one long-lived instance can serve many roots. Multiple instances
remain legal for configuration or test isolation; there is no global static
singleton.

There is deliberately no `Core`, `Command`, `Operation`, `Reply`, `Outcome`,
`CorePorts` or `OperationExecutor` in the engine. `RequestOutcome` is also
absent: an in-process call returns the reply it requested.

The implemented lexical path is:

```text
CLI -> ZvecGrep::lexical_search -> private embedded grep service
MCP -> ZvecGrep::lexical_search -> private embedded grep service
```

## Internal services

Implementation modules live in `zg-engine` and remain private. A service may
own concurrency, cancellation or resource cleanup required by its native
resource, but those policies do not justify a second generic dispatcher.

The lexical service owns admission, `grep` matcher/searcher construction,
`ignore` traversal, filtering and deterministic ordering. It runs blocking
filesystem search work outside Tokio's async workers and does not spawn a
ripgrep executable.

`ZvecGrep` also owns one private model runtime manager. Its cache key includes
the model reference and resource-affecting runtime options (credential
fingerprint, endpoint, cache directory and device). Acquiring the same key
returns counted leases over one shared model instance. Concurrent embedding
calls share the instance's lazily loaded tokenizer and weights rather than
loading independent copies. Closing `ZvecGrep` rejects new leases, immediately
retires idle runtimes and lets active leases finish before their runtime is
released.

Embedding model lifecycle events stay private until a runtime lease maps them
into the public `IndexProgress::embedding` shape. In-process callers attach an
`IndexProgressReporter` to `IndexRequest`; daemon protocol events carry the
serializable `IndexProgress` value instead of the runtime-only callback.

## Process transport

The daemon owns one process-level `Arc<ZvecGrep>`. `zg-transport-mcp` translates
each tool input directly into the corresponding typed method call; it does not
introduce a generic executor or engine command bus.

Other daemon serialization belongs to `zg-daemon-protocol`. Its wire DTOs are
transport types only; the in-process engine never translates itself into a
transport command.

## Supporting crates

The embedding catalog and Model2Vec runtime live in the private
`zg-engine::models` module. They are not yet composed into the high-level
index/query path, but integration requires no public model API or crate cycle.

`zg-host-native` remains an independently testable scanner and watcher crate not
yet composed into the high-level index/query path. When integrated, its concrete
implementation should move behind a private engine service without introducing
a dispatcher layer.

## Dependency rules

- Application code depends on `zg-engine`, never on private implementation
  modules.
- Transport DTOs do not enter the engine API.
- Native implementation types stay in the crate that owns the dependency.
- Add an abstraction only when at least two real implementations require it.
- Concurrency limits live next to the resource they constrain.
- Output ordering and error codes remain deterministic across Direct and Server
  modes.
