# Parallel work ownership

| Track | Normal write locality | Shared boundary |
| --- | --- | --- |
| Engine integration | `crates/zg-engine` | typed `ZvecGrep` methods |
| Lexical and CLI | `crates/zg-engine/src/lexical.rs`, `crates/zg-cli`, `crates/zg` | `LexicalSearchRequest/Reply` |
| Host | `crates/zg-host-native` | host crate API |
| Models | `crates/zg-engine/src/models` | private embedding catalog and runtimes |
| Daemon/MCP | `crates/zg-daemon`, `crates/zg-daemon-protocol`, `crates/zg-transport-mcp` | daemon wire DTOs and typed `ZvecGrep` calls |
| Compatibility | `compat/`, `benchmarks/`, `crates/zg-testkit` | oracle fixtures |

Cross-track changes should preserve these rules:

1. `ZvecGrep` calls private services directly.
2. Transport envelopes stay outside `zg-engine`.
3. A native dependency stays in its owner until it is deliberately integrated
   as a private engine service.
4. Shared abstractions require multiple real implementations, not planned ones.
