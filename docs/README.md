# zvec-grep documentation

**zg** is one local-first search layer for people and agents. These guides go
beyond the short path in the project [README](../README.md) and explain how to
connect an agent, shape an index, search it, and control the underlying
interfaces.

> [!IMPORTANT]
> zvec-grep is a work in progress. Commands and configuration may change before
> the first stable release.

## Start here

| I want to… | Read |
| --- | --- |
| Connect Codex, Claude Code, Cursor, or OpenCode | [Agent integrations](./01-agents.md) |
| Use zg directly from a terminal | [CLI guide](./02-cli.md) |
| Understand the tools exposed to an agent | [MCP guide](./03-mcp.md) |
| Understand indexing, updates, and search routes | [Retrieval pipeline](./04-pipeline.md) |
| See how the components and trust boundaries fit together | [Architecture](./05-architecture.md) |
| Choose between Auto, Server, and Direct execution | [Server and execution modes](./06-server.md) |
| Choose and configure an Embedding model | [Embedding models](./07-embedding.md) |
| See what is stable now and what comes next | [Roadmap](./08-roadmap.md) |
| Understand the graph model and query paths | [Graph](./09-graph.md) |

## Recommended paths

If you primarily use an agent, start with [Agent integrations](./01-agents.md),
then read the [MCP guide](./03-mcp.md) and
[Retrieval pipeline](./04-pipeline.md). The [Server guide](./06-server.md)
explains the daemon that connects them.

If you primarily use the terminal, start with the [CLI guide](./02-cli.md), then
read the [Retrieval pipeline](./04-pipeline.md). Use the
[Embedding guide](./07-embedding.md) when creating a new index. The default
`auto` execution mode is explained in
[Server and execution modes](./06-server.md).

For the whole-system mental model and trust boundaries, read
[Architecture](./05-architecture.md).

The [Roadmap](./08-roadmap.md) tracks the path from work in progress to a stable
release.

The CLI remains the source of truth for flags in the installed version:

```bash
zg help
zg help query
zg help index
```

For development setup and pull request conventions, see
[CONTRIBUTING.md](../CONTRIBUTING.md).
