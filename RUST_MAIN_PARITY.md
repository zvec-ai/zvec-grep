# Rust / main parity checklist

Tracks changes from the Node.js `main` branch that must be reflected in the Rust implementation.

- Audit date: 2026-09-13.
- Commit window: 2026-08-30 through 2026-09-13, using first-parent history.
- Main snapshot: `d4e8a3eabac13172b1c78dfa2f1b4ccfc8b99035`.
- Rust tracking baseline: `bf9764f22ca4a73ee5ca6aeebe0417bd839eb276` (`dev/rust`).
- Scope: 25 commits, including 11 `fix` commits, four other runtime/CI changes, and ten documentation/release/template changes.

All identified gaps are in scope. Match observable behavior using Rust-appropriate
implementations; identical internal architecture is unnecessary. An item is complete
when its remaining gaps are fixed and its regression scenarios are verified.

The audit compared patches, implementations, and existing tests. It did not run the
full suite or reproduce platform-specific failures on Windows and Linux. The Rust
CLI was run to confirm that the new `--index` interface is not yet supported.

## Commit coverage

Statuses describe the audit baseline, not completed remediation.

| Main commit | Change | Rust coverage | Work item |
| --- | --- | --- | --- |
| `5265395` [#81](https://github.com/zvec-ai/zvec-grep/pull/81) | Embedding failures and retries | Partial; central failure handling is missing | A |
| `8d0d5e2` [#112](https://github.com/zvec-ai/zvec-grep/pull/112) | ModelScope fallback and download reliability | Mostly missing | B |
| `13df3ad` [#114](https://github.com/zvec-ai/zvec-grep/pull/114) | Spurious stdio bridge exits | Partial | C |
| `ef544e1` [#117](https://github.com/zvec-ai/zvec-grep/pull/117) | Race-safe daemon lease publication | Different lease architecture; publication gaps remain | C |
| `0c3a6e7` [#96](https://github.com/zvec-ai/zvec-grep/pull/96) | Watcher idle timeout | No idle eviction or timeout configuration | D |
| `03ae9cb` [#86](https://github.com/zvec-ai/zvec-grep/pull/86) | Linux directory watchers | Per-directory backend already used; registration pruning missing | D |
| `891401e` [#84](https://github.com/zvec-ai/zvec-grep/pull/84) | Windows managed-rg paths | Missing in the MCP command-string parser | E |
| `d4e8a3e` [#107](https://github.com/zvec-ai/zvec-grep/pull/107) | OpenCode JSONC and config selection | Missing | F |
| `3cc4f81` [#63](https://github.com/zvec-ai/zvec-grep/pull/63) | Qoder tool permissions | Mostly covered; forced takeover retains unrelated approvals | F |
| `02fbf87` [#62](https://github.com/zvec-ai/zvec-grep/pull/62) | Qoder IDE install path | Core behavior covered; environment-path normalization differs | F |
| `69602cb` [#58](https://github.com/zvec-ai/zvec-grep/pull/58) | Qoder IDE integration | Partial; Windows detection and public MCP parameters differ | F, G |
| `462d3be` [#52](https://github.com/zvec-ai/zvec-grep/pull/52) | Qoder integration | Installer mostly covered; consent/elicitation behavior differs | F, G |
| `1c3dbff` [#105](https://github.com/zvec-ai/zvec-grep/pull/105) | Search as the default CLI action | Missing | G |
| `7d73ca1` [#100](https://github.com/zvec-ai/zvec-grep/pull/100) | CI concurrency isolation | Missing | H |
| `87f2c2a` [#119](https://github.com/zvec-ai/zvec-grep/pull/119) | CPU-only packaged-model smoke test | No equivalent enabled Rust smoke test | H |

## A. Embedding failure handling

Relevant code: [indexing pipeline](crates/zg-engine/src/indexing/pipeline.rs),
[model interface](crates/zg-engine/src/models/spi.rs),
[Model2Vec](crates/zg-engine/src/models/model2vec/model.rs),
[job scheduler](crates/zg-daemon/src/job_scheduler.rs),
[CLI entry point](crates/zg/src/main.rs), and [rendering](crates/zg-cli/src/render.rs).

The pipeline retries failed files in a second pass. Its retry classifier covers
429 and 5xx, while other failures can trigger per-file and per-fragment fallback.
Model2Vec loads lazily inside embedding calls, so queued callers can repeatedly
attempt a failed initialization. The final indexing error reports filenames but
discards the underlying model failure reasons.

- [ ] Prepare required models once before dispatching embedding work; allow a later operation to recover after a failed preparation.
- [ ] Keep unchanged and empty-content operations free of unnecessary model downloads or loading.
- [ ] Stop scheduling on shared terminal failures: authentication, unavailable models, invalid dimensions, and local download/load failures.
- [ ] Classify transient failures, including HTTP 408 and network/timeouts, and retry within one bounded budget without a second failed-file pass.
- [ ] Preserve targeted fallback for request-specific content failures instead of treating every failure as a shared model failure.
- [ ] Preserve cancellation behavior and check cancellation before model initialization.
- [ ] Retain useful provider errors, context, causes, and retry hints through engine, daemon, MCP, CLI, and status output.
- [ ] Provide concise default CLI diagnostics and useful debug details; redact credentials in both modes and persisted job errors.
- [ ] Extend redaction to quoted credential fields, Basic authentication, URL userinfo, passwords/secrets, and standalone API-key forms covered by main.

Verify request counts for permanent and transient failures, preparation failure
across multiple batches, cancellation/recovery, empty and unchanged input, and
diagnostic preservation/redaction. Use main's new failure fixtures as the oracle;
successful lazy-loading tests alone do not cover these regressions.

## B. Model download reliability

Relevant code: [model catalog](crates/zg-engine/src/models/catalog.rs),
[Model2Vec](crates/zg-engine/src/models/model2vec/model.rs),
[Transformers](crates/zg-engine/src/models/transformers/mod.rs),
[llama.cpp](crates/zg-engine/src/models/llama_cpp/mod.rs), and
[download progress](crates/zg-engine/src/models/download_progress.rs).

Rust resolves Hugging Face artifacts only. Some caches validate only that files
are nonempty; GGUF validation checks its magic. Unique partial files already help
publication, but do not provide integrity checks or cross-process ownership.

- [ ] Add pinned source revisions, artifact sizes/checksums, and Hugging Face / ModelScope mappings.
- [ ] Check both source caches before networking and use the selected snapshot consistently.
- [ ] Fall back once on eligible HTTP, network, TLS, interrupted-stream, timeout, and integrity failures; preserve both source errors if both fail.
- [ ] Do not switch sources for HTTP 401, caller cancellation, local filesystem errors, or callback failures.
- [ ] Add response-header and read-idle deadlines without imposing a total deadline on a download that continues receiving data.
- [ ] Verify cached/downloaded artifacts, repair completion metadata, and publish verified artifacts atomically.
- [ ] Coordinate downloads across processes, recover stale owners, and prevent an old writer from deleting or replacing a successor's artifact.
- [ ] Keep progress accurate for missing artifacts and fallback; report downloading before the first response chunk and emit one fallback warning.
- [ ] Keep artifact and native-backend initialization failures outside GPU retry paths; load the resolved artifacts locally.
- [ ] Refresh the [main catalog fixture](crates/zg-engine/src/models/tests/fixtures/catalog-main-oracle.json) and record its source revision. The current checked-in oracle is stale.

Verify complete and partial caches, same-size corruption, interrupted downloads,
fallback selection, timeouts, cancellation, concurrent writers, stale-owner
recovery, successor protection, progress reporting, and offline cache reuse.

## C. Daemon publication and stdio lifetime

Relevant code: [controller](crates/zg-daemon/src/controller.rs) and
[stdio bridge](crates/zg-daemon/src/stdio.rs).

The instance record is created before its JSON is written, and ready publication
directly overwrites the visible file. Atomic handling of `startup.lock` does not
protect `instance.lock`. Malformed records fail closed, avoiding part of main's
ownership bug, but transient read failures remain. The bridge stops after a
single missing/invalid status observation.

- [ ] Publish initial and ready instance records atomically, preserving a readable previous record during replacement.
- [ ] Preserve instance ownership checks, cleanup temporary files on failure, and retry eligible transient publication errors.
- [ ] Match main's grace period for missing observations: tolerate two, reset after recovery, and stop on the third; stop immediately on a confirmed identity change.
- [ ] Verify lease acquisition/release and shutdown under concurrency using Rust's operation-lock architecture. A resident heartbeat is not required solely to copy Node.js internals.

Verify paused initial/ready writes, concurrent startup, publication failure,
temporary missing records and recovery, ownership replacement, transient EPERM,
and shutdown/release races. Existing normal concurrent-startup tests are insufficient.

## D. Watcher lifetime and Linux resource usage

Relevant code: [workspace runtime](crates/zg-daemon/src/workspace_runtime.rs) and
[native watcher](crates/zg-host-native/src/watcher.rs).

Rust has no idle eviction. Its `notify` Linux backend already registers directories,
but ignored directory trees are filtered after events rather than pruned at watch
registration, so they still consume inotify resources.

- [ ] Add the four-hour idle default and `ZVEC_GREP_WATCHER_IDLE_TIMEOUT_SECONDS`; zero disables eviction.
- [ ] Refresh activity for real use, not scheduled reconciliation or pending changes; retry eviction when a busy runtime becomes idle.
- [ ] Prune excluded directory trees during native watch registration while preserving reinclusion, hidden-file, and `noIgnore` behavior.
- [ ] Verify directory additions/removals and continued event delivery after dynamic policy/tree changes.

Verify idle, active, and busy runtimes with controlled time. Use the native Linux
backend to verify registration/resource behavior; PollWatcher event-filter tests
do not establish inotify parity.

## E. Windows managed-rg paths

Relevant code: [MCP transport](crates/zg-transport-mcp/src/lib.rs),
`scan_rg_command`.

- [ ] Preserve unquoted Windows backslashes in command-string input; retain the intended Unix escaping rules.
- [ ] Verify unquoted `rg needle src\cli`, quoted paths, spaces, and platform-specific escaping on Windows and Unix.

Ordinary CLI argument parsing already receives separate arguments and is not the
affected lexer.

## F. OpenCode and Qoder installation

Relevant code: [installer](crates/zg-cli/src/install.rs),
[JSONC handling](crates/zg-cli/src/jsonc.rs), and
[installation tests](crates/zg/tests/install.rs).

- [ ] Select OpenCode's active config using explicit overrides, `XDG_CONFIG_HOME`, and existing JSON/JSONC files in main's precedence order; normalize blank overrides.
- [ ] Support comments and trailing commas while preserving unrelated JSONC content.
- [ ] Keep conflicting unmanaged configurations unchanged, report the selected config, and clean managed entries from both applicable global config files on uninstall.
- [ ] On forced Qoder takeover, do not inherit unrelated `alwaysAllow` permissions from an unmanaged server entry.
- [ ] Preserve preexisting user policies and remove only permissions owned by this installation on uninstall.
- [ ] Complete Windows Qoder discovery, including Program Files installations.
- [ ] Align whitespace normalization of Qoder environment path overrides.
- [ ] Retain covered behavior: separate CLI/IDE paths, preflight before mutation, absolute executable paths, trust/timeout settings, token expansion, and managed cleanup.

Verify JSONC with trailing commas, active-file precedence, byte-identical unmanaged
conflicts, dual-file uninstall, forced takeover, permission ownership, environment
overrides, and Windows installation layouts.

## G. CLI and public MCP behavior

Relevant code: [CLI parser](crates/zg-cli/src/lib.rs),
[binary](crates/zg/src/main.rs), and
[MCP transport](crates/zg-transport-mcp/src/lib.rs).

- [ ] Make search the default CLI action and support main's flag-based actions, including `--index`, `--status`, `--install`, and `--server`.
- [ ] Match handling and warnings for legacy command-shaped search inputs, `--` literal input, default human-readable output, and `--compact`.
- [ ] Implement first-search local index creation and main's local-model selection behavior, preserving remote authorization checks and `indexPolicy=disabled` in direct and server modes.
- [ ] Update generated install commands, help, errors, and examples with the CLI interface.
- [ ] Remove `apiKey` and `device` from public MCP search schemas and strip caller-supplied values as main does; retain appropriate administrator/CLI overrides.
- [ ] Close the consent behavior gap: Rust currently relies on workspace grants and does not implement interactive MCP elicitation. Verify consent, cancellation, unsupported hosts, and existing grants end to end.
- [ ] Verify bidirectional request forwarding. Rust's transparent stdio relay avoids the specific SDK dispatch issue fixed by main, but still needs behavioral coverage.

Verify parser behavior and first-search flows through the actual binary, inspect
public MCP schemas, and exercise supported and unsupported MCP hosts. Main's later
README change (#131) restored released examples; it did not revert #105's code.

## H. CI and parity evidence

Relevant code: [CI workflow](.github/workflows/ci.yml).

- [ ] Scope concurrency by workflow, event, and pull request/commit, following #100, so unrelated runs do not cancel one another.
- [ ] Reconcile the security workflow portion of #100; this Rust branch currently has no corresponding workflow.
- [ ] Add an enabled CPU-only smoke test for real local models using the built/distributed Rust application, equivalent to #119's packaged-model coverage.
- [ ] Add the focused regressions listed above and run platform-specific cases on their target operating systems.
- [ ] Record the upstream revision for compatibility fixtures and refresh them when upstream behavior changes.

Ignored real-model tests and a passing stale fixture do not establish current-main
parity. Keep download/network smoke coverage explicit and distinguish it from
deterministic fixture-based tests.

## Documentation, release, and template reconciliation

These ten commits were classified separately from runtime fixes. Their content
parity remains to be checked; none is marked complete by this audit.

| Main commit | Change |
| --- | --- |
| `6fa85a8` #131 | Released CLI examples in READMEs |
| `a7bb360` #127 | Trendshift badges |
| `d756cc7` #80 | Workspace index location documentation |
| `81a80f4` #78 | GitHub issue templates |
| `899929f` #76 | WeChat QR URL |
| `cab59dc` #70 | zvec introduction links |
| `309a669` | v0.2.1 release metadata |
| `6c29f57` #60 | Usable-model documentation |
| `fcc09d7` #53 | WeChat QR links and images |
| `ee051d3` | WeChat QR documentation |

- [ ] Reconcile the final documentation/assets/templates state, including overlapping QR updates.
- [ ] Fit applicable documentation into the current Rust layout; do not restore the previously removed `docs` directory just to mirror main.
- [ ] Update CLI examples and model/index documentation to describe the implemented Rust behavior.
- [ ] Reconcile release/package metadata with the Rust release process; do not copy a historical version bump as a new release.

## Completion

Recommended order: A/B (engine), C/D (daemon/watchers), E/F/G (interfaces and
integrations), then remaining CI and documentation work. Add regression coverage
alongside each implementation rather than deferring it to the end.

- [ ] Every gap has an implementation and appropriate verification evidence.
- [ ] Architecture-specific fixes have documented equivalent behavior and coverage.
- [ ] Formatting, linting, relevant workspace tests, platform checks, and packaged smoke tests pass.
- [ ] Record the final Rust commit and update the coverage table with the closing commits/tests.
