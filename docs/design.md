# zvec-grep Design

## What zvec-grep is

zvec-grep is a general-purpose, local-first context search engine. It helps
people, applications, and AI agents quickly search large collections for
relevant context, without having to read the entire collection first.

It is designed to make many kinds of files and content searchable through a
single engine.

zvec-grep can combine several complementary signals:

- exact, lexical, and full-text matches;
- semantic or multimodal vector similarity;
- structure and metadata such as files, pages, sections, symbols, and
  relationships;
- hybrid ranking when no single signal is sufficient.

Local-first does not mean local-only. Scanning, indexing, storage, and search
can run entirely on the user's machine, and local models are a first-class
path. Callers may instead choose remote embedding, reranking, extraction, or
other model APIs when their deployment, quality, or hardware requirements make
that preferable. Remote access is explicit and replaceable rather than a
requirement built into the engine.

## Its role in an LLM workspace

zvec-grep is a retrieval substrate, not the owner of the knowledge or the
agent that maintains it. The LLM or application remains responsible for
reading sources, reasoning, organizing knowledge, and creating or updating the
files that form the durable source of truth. zvec-grep builds a disposable,
rebuildable index over those files and other source artifacts. It never treats
its index as the authoritative copy and never edits the indexed sources.

This complements agent-maintained knowledge patterns such as Karpathy's
[LLM Wiki](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f).
In that pattern, an LLM incrementally organizes knowledge into a persistent,
structured collection of files instead of synthesizing everything again from
raw documents for every question. As that collection grows, zvec-grep provides
the fast search layer over both source material and LLM-maintained artifacts.

The division of responsibility is intentional:

```text
Sources and durable files ── source of truth, maintained by people and LLMs
             │
             ▼
        zvec-grep index ──── derived search structure
             │
             ▼
       ranked context candidates
             │
             ▼
       LLM or application ── reasons, answers, organizes, and writes files
```

Without an index, an agent may repeatedly list directories, open broad files,
or load large collections merely to discover what might be relevant. zvec-grep
makes that initial narrowing step fast and inexpensive by returning a ranked
set of promising candidates. It does not claim to understand the task as
deeply as the agent or guarantee that every result is relevant. The agent or
application remains responsible for inspecting the candidates, gathering more
context when necessary, and deciding what actually matters.

This division uses each component for what it does well: zvec-grep reduces the
search space with efficient indexes and retrieval algorithms, while the LLM
performs the deeper interpretation and reasoning. The result can be lower
search latency, less context-window pressure, fewer input tokens, and more room
for the model to do work that requires its intelligence.

## Design principles

### Local first

Indexing and search should work without sending repository contents to a
remote service. Remote embedding providers may be supported, but they must be
optional and explicit.

### Fast enough for large repositories

The normal case is incremental. Files that have not changed should not be
read, extracted, embedded, or written again. Work that must be performed is
bounded and batched so that a repository with many small files does not pay a
large fixed cost per file.

Long-running work must expose progress, support cancellation, and checkpoint
often enough to recover without restarting the entire job.

### Search should combine complementary signals

Lexical search is precise for identifiers and exact terms. Vector search is
useful when the query and the relevant code use different words. zvec-grep
supports both and uses reciprocal-rank fusion for hybrid results.

Search results should be explainable. A caller can see whether a result came
from lexical recall, vector recall, or both, together with the original scores
and ranks.

### Structure matters

Code should preferably be indexed along syntax boundaries such as functions,
methods, and types. Markdown should be divided into sections without treating
headings inside fenced code as document headings. Plain line-based chunking is
the fallback when a structured extractor is unavailable or parsing fails.

### The engine is a library

The core product is a standalone Rust library. It must not know about command
line parsing, HTTP, terminal formatting, or a particular user interface.

The dependency direction is deliberately one-way:

```text
CLI ───────► Engine
 │
 └────────► Server client ─────► Server ─────► Engine
```

The server may expose the engine to multiple clients. The CLI may either use
the engine directly or connect to a running server. The engine never depends
on either of them.

### Prefer simple, explicit code

Modules should represent product concepts rather than implementation
convenience. Names such as `file`, `workspace`, `indexing`, `embedding`,
`storage`, and `search` should make the system understandable to a newcomer.

We avoid generic `utils`, `helpers`, and `common` modules. A source file starts
flat and is split into a submodule only when its implementation becomes large
enough that the split improves navigation.

### Replaceable boundaries

Embedding and storage are traits. The indexing and search policies do not
depend on one model runtime or database implementation. This lets us replace
an implementation without reorganizing the engine.

### Derived formats may change

zvec-grep is still beta. We prioritize a clear design over compatibility with
old APIs or index formats. An incompatible model or schema is detected and
reported instead of silently mixing data. Rebuilding an index is an explicit,
safe operation because the index is derived from source files.

### Portable by default

The primary targets are common desktop and server platforms. Platform-specific
acceleration, such as Metal or CUDA, is optional. The portable CPU path must
remain available. Mobile is not a current target, but engine boundaries should
not unnecessarily prevent future mobile work.

## Architecture

The repository is a Cargo workspace:

```text
crates/
├── engine/   standalone indexing and search library
├── server/   server and client library
└── cli/      command-line application
```

Within the engine, responsibilities are divided as follows:

```text
Workspace scan
      │
      ▼
Incremental diff ──► File extraction ──► Batched embedding
      │                                      │
      └──────────────────┬───────────────────┘
                         ▼
                  Persistent storage
                    FTS + vectors
                         │
              ┌──────────┴──────────┐
              ▼                     ▼
        Lexical recall        Vector recall
              └──────────┬──────────┘
                         ▼
                 Filtering + fusion
                         ▼
                   Search results
```

### Workspace and files

A workspace has one canonical root. Files receive stable identifiers derived
from their relative paths. Scanning follows Git ignore rules, caller-provided
include and exclude patterns, hidden-file policy, default dependency/build
directory exclusions, and a maximum file size.

Incremental indexing compares the scan with the stored manifest:

- unchanged metadata avoids opening the file;
- a changed timestamp with an unchanged content hash avoids re-embedding;
- changed contents replace the file's segments only after extraction and
  embedding succeed;
- deleted files remove their stored segments;
- a failed file does not destroy its last usable indexed representation.

### Extraction

An extracted `FileSegment` contains:

- a stable segment identifier;
- its file and relative path;
- a line, page, or whole-file location;
- its kind, such as code, document section, or text;
- optional code symbol metadata;
- the text used for search and embedding.

Tree-sitter provides syntax boundaries for supported programming languages.
Markdown has a dedicated section extractor. Other textual formats use bounded
line chunks with small overlaps. Binary and currently unsupported rich formats
produce no text segments rather than being interpreted incorrectly.

PDF and other rich document formats will be implemented as separate
extractors. They should produce the same `FileSegment` abstraction, using page
locations when appropriate. This keeps search and storage independent of file
parsing libraries.

### Embedding

The `Embedder` trait accepts a batch of text and returns one vector per input.
Its schema identifies the model, vector dimension, and a conservative input
size used by extraction.

The current implementations are:

- a deterministic token-hash embedder for tests and dependency-free fallback;
- an optional llama.cpp backend for local embedding GGUF models.

The engine does not silently truncate arbitrary input after extraction.
Instead, extraction uses the model's configured input budget. Providers must
preserve input order and return exactly one correctly sized vector per input.

### Storage

Storage owns indexed file metadata, extracted segments, lexical indexes, and
vectors. The production implementation uses Zvec. An in-memory implementation
supports tests and embedded use cases.

Persistent workspace data lives under:

```text
<workspace>/.zvec-grep/index/
├── manifest.json
└── segments/
```

The manifest is written atomically. Zvec mutations and manifest checkpoints
are batched separately from embedding calls, avoiding a database flush or a
full manifest rewrite for every file. A workspace lock prevents two processes
from owning the same persistent collection simultaneously.

### Indexing

Indexing is a synchronous engine operation. A server or GUI can run it on a
blocking worker without making the engine depend on an async runtime.

The operation reports structured phases for scanning, file processing,
embedding, persistence, and completion. Cancellation is cooperative and is
checked between bounded units of work. File-level failures are returned as
data; systemic failures such as an unusable model or storage engine terminate
the operation.

### Search

Search supports three modes:

- lexical;
- vector;
- hybrid.

Hybrid search recalls candidates independently and combines their ranks using
reciprocal-rank fusion. Recall depth grows adaptively when filters remove too
many early candidates. Filters can constrain paths, formats, modification
times, symbol names, and symbol kinds.

An exhaustive lexical path is also available. It scans and extracts current
files without creating or modifying an index. This is useful for exact searches
where freshness is more important than repeated-query performance.

### Server and CLI

The server is the long-running owner of an engine instance. It will coordinate
concurrent clients, run blocking engine work safely, and expose stable request
and response types.

The CLI is a user interface rather than a second engine. It is responsible for
argument parsing, progress rendering, result formatting, and choosing between
direct and server-backed operation.

## Error and safety model

Expected failures are typed errors, not process exits or string conventions.
Poisoned locks, incompatible models, invalid globs, I/O failures, embedding
failures, storage failures, cancellation, and busy workspace indexes remain
distinguishable to callers.

The engine modifies only its workspace-local derived index. Rebuilding never
modifies source files. Existing indexed data is replaced only after the new
representation has been successfully produced.

## Testing and dependency policy

Every implemented component should have focused unit tests. Cross-component
tests cover the complete scan-to-search path and persistent Zvec behavior.
Formatting, tests, and Clippy with warnings denied are required quality gates.

The project is Apache-2.0. Dependencies must use licenses accepted by
`deny.toml`, come from approved registries or sources, and avoid unnecessary
platform-specific requirements in the default build.

## Current non-goals

- Compatibility with the TypeScript beta API or index format.
- Mobile applications or mobile packaging.
- Editing, generating, or rewriting repository files.
- Treating remote services as a requirement for local search.
- Putting CLI, transport, or presentation concerns into the engine.
