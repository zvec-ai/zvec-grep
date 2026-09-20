# zg-host-native

Native filesystem scanning and resident watch sessions. Scanner/watcher request
types, cancellation control and traits belong to this crate; they are not
exported by `zg-engine`.

This crate is standalone: it exposes `HostError` and does not depend on
`zg-engine`. Engine composition maps native failures to `EngineError` at the
indexing boundary.

The crate provides metadata-first discovery, bounded source reads, normalized
change batches and explicit scan/watch resource limits. The engine supplies
`PathPolicy` decisions; this crate does not interpret glob, format or ignore
rules. Scanner traversal and watcher registration both apply that policy before
entering directories. Watch sessions observe rule files, refresh their directory
registrations when rules or the tree change, and preserve deletion events for
previously indexed paths.
