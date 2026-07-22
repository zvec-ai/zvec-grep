# Benchmark skills

The `zvec-grep` skill is a temporary benchmark integration shim for Qwen Code,
which does not yet have a managed integration in `zg install`. Codex and
OpenCode use the zvec-grep MCP server instead.

The runner builds the task index separately because indexing represents
user-owned setup rather than agent behavior.
