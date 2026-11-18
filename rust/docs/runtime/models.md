# Embedding models and concurrency

Each workspace uses one embedding model for text and code. Runtime ownership
is described in [engine and workspace](engine.md); build dependencies are in
[native builds](../development/building.md).

## Model selection

New indexes without `--embedding`, `ZVEC_GREP_EMBEDDING`, or a configured default
select Potion Code for code-heavy workspaces and Bekko for text-heavy workspaces.
The shared engine analyzes only files admitted by the index's scan rules and
supported text formats. Each file contributes at most 64 KiB of its size; data,
configuration, and ambiguous code/document formats are counted separately.
A code or text share of at least 60% identifies the dominant scene. Mixed
workspaces use the larger share; ties and absent evidence use Potion Code.
Explicit indexing explains the selection on stderr. First-search setup shows
the same analysis and preselects the recommendation unless a configured model
takes precedence, regardless of download status. Direct and Server CLI modes resolve
the caller's default through the shared engine, keeping it distinct from an explicit
model. The engine chooses again after acquiring the writer lock: an index published
while the request was queued takes precedence over the caller's default. MCP performs
the same analysis inside the daemon. Workspace analysis inputs for model setup stay
in-process; its v2 request shape is unchanged.
Compatible existing indexes retain their stored model, including during
`--rebuild` without `--embedding`; supply a model explicitly to switch it.

This version indexes text and code with one embedding model per workspace. Choose
it with `zg --index --embedding <model>` or set a default with
`zg --config model set <model> --default`. Code, documents and structured text use
that same model and retrieval collection. Code has its own content payload kind.
Images and other unsupported sources are reported as skipped; multimodal content
and per-content model routing are not supported. Changing the model requires
`zg --index --rebuild --embedding <model>`.

## Bekko backend

The local catalog includes `local/bekko-256-int8`.
Select it with `zg --index --embedding local/bekko-256-int8 --device cpu`.
Mean pooling and the 256-dimensional normalized output remain FP32.
Queries and documents have no prefix or special tokens, with a 32,768-token limit.
The Rust Bekko backend (`bekko`) reads the quantized table and per-token scales
directly from the pinned ONNX file; inference runs on the shared CPU pool
without an ONNX session or padded batches. It retains unknown tokens and
matches the ONNX FP32 multiply, accumulation and mean before normalization.
Batch rows tokenize and aggregate in parallel within the shared compute pool;
each row retains its sequential arithmetic, and output order is preserved.
Single-input requests stay on the current compute worker.
The real Bekko CI smoke compares all 256 output components against the ONNX
reference, including empty inputs, unknown tokens, truncation boundaries and
concurrent batches.
The maximum batch size is 32 and default indexing concurrency is 1.
`ZVEC_GREP_BEKKO_PARITY_INPUTS` optionally adds a JSON array of corpus texts to
that smoke test for local differential validation.
Existing indexes must be rebuilt when switching embedding models, even when
their vector dimensions match.

## Index concurrency

Index embedding concurrency is configured with
`zg --index --embedding-concurrency <n>`.
The option is rejected outside `--index`. For automatic indexing and
refreshes as well, set `ZVEC_GREP_EMBEDDING_CONCURRENCY=<n>`. Transformers/ORT clamps
positive integers to its supported range. Explicit zero is rejected for every
backend. Explicit CLI/API index settings take precedence over this environment
variable, followed by the legacy
`ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM` for llama.cpp indexing, then backend defaults.
Invalid environment values warn and select the automatic default. These overrides
do not configure a separate query worker and are not persisted in workspace settings.
Direct and Server indexing use the same engine boundary; restart the Server after
changing its inherited environment, while explicit CLI overrides need no restart.

For llama.cpp, the limit controls contexts within a serialized batch and is capped
at eight, defaulting to one. Transformers/ORT shares a bounded runtime between
indexing and queries for the same model, cache and device:

## ORT execution pool

- C controls the session count on both CPU and CUDA, defaulting to one and
  capped to available hardware concurrency. Direct indexing uses the explicit
  CLI/API value, then `ZVEC_GREP_EMBEDDING_CONCURRENCY`, then the default.
- Server startup captures `ZVEC_GREP_EMBEDDING_CONCURRENCY` as each model/device
  pool's capacity. Sessions are created lazily on first preparation. A client's
  concurrency only limits its own operation within that pool: requests above capacity warn
  and are capped, omitted values use server capacity. Requests never resize the
  resident pool or change its thread configuration. Independent operations may
  execute concurrently up to the shared pool capacity. Different models/devices
  have separate pools; C is not a process-wide session count.
- `ZVEC_GREP_ORT_THREAD_BUDGET=T` configures the single process-global ORT
  intra-op pool shared by all Sessions. It defaults to half the available logical
  threads, rounded up, and is capped to available hardware concurrency. Invalid
  environment values warn and use defaults. Changing the server pool capacity or
  global thread budget requires a Server restart; changing a client operation's
  concurrency does not. T controls CPU operator parallelism, not GPU threads or all process
  threads; native Run callers, tokenizers and other libraries have their own
  threads. Sessions share T without dividing it by C or rebuilding to change thread budgets.
- Each Session owns one fixed native worker; no Session receives concurrent Run
  calls. A query can use an idle Session, with at most one query batch alongside
  document work. CPU, CUDA, DirectML and WebGPU use the same fixed-pool execution
  limits; CoreML retains its single-session limit.
- Each request retains its input batch; the scheduler does not merge requests.
  Upstream indexing still batches inputs using each model's catalog limits
  (four texts per request for MiniLM). Queries run first; running inference is
  not preempted. CPU preparation and postprocessing overlap native execution.
- Pending inputs have separate document/query limits: document admission is capacity+2
  requests on CPU and 4*(capacity+1) on CUDA; queries have four slots. Copied text remains
  bounded by 32/8 MiB respectively, with at most 8 MiB per request. Full queues apply
  cancellable backpressure. Native work retains runtime ownership until it settles.

Explicit concurrency values must be positive. The effective backend budget is
reported when it differs from the request.
Admission includes bounded prefetch based on pool capacity, independent of a Server request's C, so a single
executing Session can prepare its next batch in advance. Extra Sessions allocate
their own weights and workspaces; batch activations also contribute to memory.
Initialization failure releases the partial pool before CPU fallback; inference
fallback drains GPU work and converts the entire pool to CPU. Batch limits remain
independent of C and T. Diagnostics report actual Sessions, the global intra-op
configuration, session initialization count, fallback count and native batch sizes.

## Other backends

Potion/model2vec defaults to two batch tasks and retains an independent CPU worker
capacity; remote models retain adaptive request scheduling. Their overrides are
not capped at eight. llama.cpp continues to cache separate native resources for
different fixed context budgets, retiring unused variants when switching.

## Artifact downloads

Model artifact downloads send a `zvec-grep/<version>` User-Agent, including after
CDN redirects. A transient connection failure or header/read timeout retries the
same artifact once over IPv4 before trying the next source (Hugging Face, then
ModelScope). Failed partial files are discarded and the retry is cancellable.
HTTP status errors, certificate errors, and integrity failures do not trigger an
IPv4 retry. Proxy settings remain in effect; with a proxy, the IPv4 constraint
applies to the connection to the proxy, which may resolve the destination itself.

See [live model download checks](../development/validation.md#live-model-downloads)
for full-catalog and single-model validation.
