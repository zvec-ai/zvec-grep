# Low-memory vector-index quantization

Status: implementation in progress (2026-11-14). This document separates user
requirements from engineering choices and records verification limits.

## User-specified requirements

- Reduce RAM used by zg vector indexes by storing document vectors compactly as
  binary rather than ordinary floating-point vectors.
- Keep query embeddings floating point and score them against stored binary
  document vectors; do not silently binarize queries and substitute Hamming
  distance.
- Commit the implementation, push the branch to the user's fork, and open an
  upstream feature request and PR.
- Test in a disposable local feature-test index; do not rebuild or replace
  registered user indexes for this feature test.
- Try the local Copilot API's Metis embedding model if available.
- The user questioned whether packing already-binary Metis document embeddings
  should preserve results (2026-11-15); verify this with real cached Metis vectors
  rather than treating a varying-magnitude synthetic benchmark as Metis evidence.

## Current engineering findings

- This implementation stores a sign bit per document-vector component in a
  binary field, plus one FP32 scale per vector. It does not store a second FP16
  or FP32 document vector.
- Query-time scoring uses the original FP32 query and a sign-vector
  reconstruction. Cosine ignores scale; dot-product and Euclidean use the
  per-document mean-absolute-value scale. It is not Hamming scoring. This
  reconstruction loses information for arbitrary varying-magnitude vectors, but
  is lossless for vectors whose components are exactly ±0.03125.
- Binary-sign storage writes an explicit top-level schema marker, so older
  strict readers reject it instead of opening the binary collection as FP16;
  legacy/default FP16 schema JSON remains unchanged.
- Current binary search scans all packed document vectors in bounded scalar
  query pages and ranks them; it does not use HNSW. Retrieval latency and
  quality therefore need separate measurement from memory savings.
- The local API at `127.0.0.1:4141` exposes `text-embedding-3-small` but does
  not list Metis. An OpenAI-style request for
  `github/metis-1024-I16-Binary` returned `model_not_supported`; the Metis
  integration test is therefore not verified on this endpoint.
- Public VS Code client source describes Metis query/document selection using
  one `/embeddings` route with `input_type`; it does not establish a stable,
  publicly documented GitHub API contract or a packed-bits response format.
- A local read-only cache sample found 3,197 of 3,200 vectors with exactly the
  values ±0.03125, and three with many distinct float values. The hashed cache
  keys do not expose the purpose, so these are document-shaped and query-shaped
  candidates, not independently verified purpose labels. VS Code's Metis
  storage code packs signs and reconstructs ±0.03125. Existing workstation zg
  uses its native Metis provider and FP16/DiskANN; port 4141 is a different API
  path, so that endpoint's unsupported-model response does not imply the native
  Metis integration is unavailable.
- A separate read-only comparison selected 10,000 cached vectors whose every
  component was ±0.03125 and 20 finite nonzero, non-sign-only cached vectors;
  selection used cache-key order, did not filter expiry, and actual purposes
  remain unknown. Across 200,000 cosine scores, packing/reconstruction changed
  the mean absolute score by 1.82e-18 and at most 1.11e-16 due to floating-point
  rounding. Each of the 20 top-ten sets matched the exact-float top ten (no
  boundary ties within 1e-12). This confirms mathematical equivalence for the
  selected values, not semantic relevance or production index performance.
- A separate debug-build test constructed disposable, optimized production
  fragment collections from 1,024 binary-shaped and three dense query-shaped
  cached vectors, selected in cache-key order. Against exact cosine, both
  FP16/HNSW and binary-sign returned 30/30 top-ten hits. FP16/HNSW used
  4,606,824 collection bytes and averaged 0.81–1.40 ms/query; binary-sign used
  965,683 bytes and averaged 16.47–18.64 ms/query. These are two tiny,
  non-isolated runs of fragment collections only; this is not an end-to-end
  workspace benchmark, not DiskANN, and has no relevance judgments. PSS was
  sampled sequentially in the same process, so it cannot be interpreted as a
  controlled RAM comparison.
- Exploratory debug-build benchmark with 2,000 deterministic, varying-magnitude
  synthetic 1,024-dimensional vectors: FP16/HNSW used 6,471,672 index bytes,
  47,864,832 PSS bytes, 343 ms build, 2 ms query, and 10/10 top-10 overlap
  against exact cosine. Binary-sign used 540,386 index bytes, 38,658,048 PSS
  bytes, 68 ms build, 52 ms query, and 4/10 overlap. These are one-run synthetic
  measurements, not real-model quality results; the resident-memory comparison
  includes process overhead. A 20,000-vector binary-sign run used 4,286,806
  index bytes and 46,984,192 PSS bytes, with a 542 ms query and 6/10 overlap;
  the matching FP16 run was stopped after sustained high CPU use, so that
  corpus size has no paired baseline.

## Current implementation proposal (not a user requirement)

- Expose explicit per-workspace `fp16` (default) and `binary-sign` modes.
- Persist the selected mode in embedding-index metadata. A mode change requires
  a rebuild, while old published workspace state remains intact until a
  successful replacement.
- Use a bucketed, bounded-page scalar scan for sign vectors because the current
  zvec vector ANN index does not support the desired packed-bit field/query
  pairing. Revisit this if zvec adds an efficient float-query/binary-document
  ANN path. The current exhaustive scan means the mode is experimental and not
  established as a routine-search replacement. The synthetic quality drop must
  not be interpreted as measured quality loss for binary-valued Metis documents.

## Acceptance evidence to collect

- A disposable index builds locally, reopens, and returns vector results; the
  default FP16 path remains unchanged. Exercise any server-compatible path
  without touching registered user indexes.
- Compare the same corpus and queries in FP16 and binary-sign modes: vector and
  index bytes, build time, query latency, and process PSS/RSS. Do not claim a RAM
  win without measured before/after results.
- Compare retrieval results against the FP16 baseline on a fixed fixture and
  report ranking differences; the earlier 43-query MTEB sample was exploratory,
  not an official score or a predeclared quality threshold.
- The ignored `private_real_cache_production_index` test can be explicitly run
  with `ZG_PRIVATE_METIS_CACHE` pointing to a private cache and an isolated
  temporary directory. It creates private disposable indexes containing
  cache-derived vectors and prints aggregate counts only. Do not copy raw cache
  data or generated index files into Git or publish them.
- Retry the Metis endpoint only when the local Copilot API exposes a supported
  Metis request path; otherwise record the limitation and keep the disposable
  synthetic/local test isolated.
