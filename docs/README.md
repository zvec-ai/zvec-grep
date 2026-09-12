# zvec-grep documentation

**zg** is one local-first search layer for people and agents. These guides go
beyond the short path in the project [README](../README.md) and explain how to
connect an agent, shape an index, search it, and control the underlying
interfaces.

> [!IMPORTANT]
> zvec-grep is a work in progress. Commands and configuration may change before
> the first stable release.

> [!WARNING]
> These guides track **`main`**, where search is the default interface and
> maintenance uses long options (`zg --index`, `zg --status`, …). The published
> npm package (currently `0.2.2`) still uses subcommands (`zg index`,
> `zg query`, `zg status`, …). For an installed binary, follow the project
> [README](../README.md), https://zvec.org/en/docs/zvec-grep/, or `zg help` /
> `zg <command> --help`. Do not copy `zg --index` examples from this tree onto
> `0.2.2`.

| Published (`0.2.2`) | `main` (these guides) |
| --- | --- |
| `zg index …` | `zg --index …` |
| `zg query …` | `zg …` (positional search) |
| `zg status …` | `zg --status …` |
| `zg install …` | `zg --install …` |
| `zg help` / `zg help query` | `zg --help` / `zg --help search` |

## Start here

| I want to… | Read |
| --- | --- |
| Connect Codex, Claude Code, Qwen Code, Cursor, or OpenCode | [Agent integrations](./01-agents.md) |
| Use zg directly from a terminal | [CLI guide](./02-cli.md) |
| Understand the tools exposed to an agent | [MCP guide](./03-mcp.md) |
| Understand indexing, updates, and search routes | [Retrieval pipeline](./04-pipeline.md) |
| See how the components and trust boundaries fit together | [Architecture](./05-architecture.md) |
| Choose between Auto, Server, and Direct execution | [Server and execution modes](./06-server.md) |
| Choose and configure an Embedding model | [Embedding models](./07-embedding.md) |
| See what is stable now and what comes next | [Roadmap](./08-roadmap.md) |

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

The installed CLI remains the source of truth for the package you have:

```bash
# published 0.2.2
zg help
zg help query
zg <command> --help

# main / these guides
zg --help
zg --help search
zg --index --help
```

For development setup and pull request conventions, see
[CONTRIBUTING.md](../CONTRIBUTING.md).
