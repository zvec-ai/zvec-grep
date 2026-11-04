# Model module micro-benchmark contract v5

One case is **model × input length L in words × batch size B × CLI
embedding concurrency C × requested device**. One timed workload, `embedding`,
measures batch-call latency, throughput and external resource usage.

The measurement boundary is the existing private model module. It includes
runtime admission, input formatting, tokenization, native computation and the
engine's normal postprocessing. Scanning, chunking, storage and retrieval are
outside this boundary. Model correctness is covered by the model tests, not this
micro-benchmark: there are no token-length, vector-value, normalization,
repeatability or truncation checks, and no boundary-audit prerequisite. The
benchmark adds no backend/runtime statistics probes.

## Matrix and input generation

Defaults:

- All local catalog models; no remote or paid models.
- Input lengths L (whitespace-separated words): `32 256 1024`.
- Batch sizes B: `1 4 16`.
- CLI embedding concurrency C: `default 1 2 4 8`.
- Requested devices: `cpu metal`; CUDA is selectable with a CUDA build.
- One process round per executable case; use `--rounds` to request repeats.

Resolve each configured L to `min(L, model.max_input_tokens)` and B to
`min(B, model.max_batch_size)`, then deduplicate those values per model. This is
configuration resolution against catalog limits, not validation of tokenized
inputs. Store requested tiers and resolved cases in the manifest. Case paths are:

```text
MODEL/DEVICE/lL/bBATCH/cCONCURRENCY/r01/result.json
```

Generate a deterministic pool of eight input variants, each containing L
whitespace-separated words. Use the same `EmbeddingPurpose::Document` format for
every call. Record generator identity, input hashes and byte counts. Do not run a
tokenizer probe or inspect how the backend transforms the inputs. Prefixes,
special tokens, token filtering and truncation remain ordinary engine behavior.

**L is the number of whitespace-separated words per generated input, not the
tokenizer's token count.** The report uses the compact label `L=32`. Input-word
throughput remains in raw and aggregate JSON but is not displayed in the table.
The raw field names `input_tokens` and
`nominal_tokens_per_second` are retained for compatibility. Identical L values
do not establish identical native token counts across models.

Timed workers draw batches from a deterministic shared queue. Keep B and C fixed
within a case. A successful batch means that the embedding API returned `Ok`;
record the number of returned vectors without asserting its size or contents.

## Concurrency and device configuration

Use production `resolve_index_concurrency` and `batch_concurrency`. Clear inherited
indexing concurrency overrides so `default` uses the backend default:

- llama.cpp has one in-flight batch; C controls native context parallelism within it.
- Transformers/ONNX uses the production in-flight batch limit, capped at eight;
  its current default is one.
- model2vec uses its production default (currently two) or the explicit budget.

Record requested C, resolved runtime budget and configured in-flight batch count.
`default` remains a distinct case from an explicit value even if they resolve to
the same budget. Do not add extra batch workers on top of a native context budget.

The fixed first column records the **requested device only**. Actual execution device
and fallback are not verified. External GPU utilization is a resource observation,
not evidence that this case executed on that GPU. Do not infer a provider from
CPU/GPU activity.

The known CPU-only model2vec capability can skip non-CPU cases. Save a bound
`case-status.json` with the reason, keep its table row, and omit its measurement
rounds. A skip record cannot coexist with measured rounds. Other backends are not
preclassified as unsupported from their names.

## Timing and rounds

Each round uses a fresh process and runtime manager. Prepare the model and input
pool, then warm at least three worker waves while covering the input-pool batch
cycle. Warmup calls exercise the engine without comparing outputs; call failures
remain execution failures. Preparation and warmup are outside measurement.

Run one continuous closed-loop phase: each completed worker immediately takes
its next batch. The baseline targets both **15 seconds** and **100 completed
batches**, ending at a balanced input-cycle boundary. Stop new submissions after
**120 seconds** and drain already-issued calls. Preserve an unmet sample target
when the cap fires. The cap does not cancel native work or bound total wall time.
The process watchdog defaults to **1,800 seconds**.

Smoke uses one second, at least three completed batches and a ten-second dispatch
cap. Label it as a short validation run; it is excluded from performance
comparison even when multiple rounds are requested.

Shuffle cases independently per round using the saved seed. Default idle time
between processes is two seconds. Keep inference serial across cases, and finish
builds/downloads before measurement. Idle time does not establish temperature
control. Preparation may warm filesystem caches, so load timing is not
storage-cold timing.

## Metrics and resource collection

- **Batch latency:** time the whole embedding call, including runtime waiting and
  preprocessing. Compute p50/p95/p99 over successful batches; never divide a batch
  percentile by B or C.
- **Vector throughput:** returned vectors from successful calls divided by the
  entire measured phase duration, including tail drain.
- **Input-word throughput:** successful batches × B × L divided by that same
  duration. Retain this `words/s` metric in raw and aggregate JSON only; it is not
  measured native token throughput.
- **Counts:** completed, failed and timed-out batches. Process watchdog timeouts
  are separate from known batch timeout counts. Missing batch evidence is N/A or
  explicitly partial, never an invented zero or one. Keep counts and failure
  information in JSON rather than a table column.
- **RSS:** process RSS sampling peak during embedding. It includes the harness,
  request records and allocator caches, and is not an exact allocation peak.
- **GPU memory:** embedding-phase process GPU memory peak when observable.
  Apple unified memory is not dedicated VRAM; unavailable values are N/A, never
  inferred from RSS.
- **CPU:** process mean and fixed nonoverlapping one-second peak; 100% is one
  logical core. Sample every 20 ms and at phase transitions. Interpolate CPU time
  only across gaps of at most 250 ms; invalid windows and phases shorter than one
  second have no peak. Keep raw evidence rather than clipping impossible values.
- **GPU utilization:** device-wide mean/peak, sampled every 500 ms through
  IORegistry on macOS (including CPU cases) or `nvidia-smi` on CUDA. It includes other applications and
  is not process-attributed utilization.

Resource workers use the same OS monotonic clock. On macOS, bracket
`proc_pid_rusage` with that clock, use the midpoint and convert CPU ticks with
`mach_timebase_info`. Record sampling gaps and coverage. Observe external process
CPU separately using PID/name/creation time, without arguments or environment.
Flag phases with more than one external CPU core for at least 10% of observed
time. These warnings do not prove performance loss. Do not stop other processes;
record thermal state only when observed.

Compute metrics per round, then aggregate independent rounds. The table displays
only p50 batch latency; p95/p99 remain in JSON. p95 requires 100 successful batches
and p99 requires 1,000; otherwise record N/A. The table shows the median of round
metrics; non-percentile metrics also show min–max ranges, which are not confidence
intervals. Preserve failed rounds and their counts/logs.

## Reproducibility and measurement checks

Record binary/source/lockfile/input hashes, catalog/artifact revisions, build
profile/features/compiler, ORT library version/hash, machine model, CPU/GPU models,
OS/RAM and numerical thread environment. Use Rust 1.98.0 release and pinned ORT
1.21.0. Artifact setup
and SHA256 checks happen before timing and identify the files being measured;
they are not model correctness tests.

The manifest fingerprints the complete matrix, policy, profile, duration/sample
limits, rounds/order/seed/cooldown/timeout, cache and provenance. Each round has a
case fingerprint including L and B. Changed configurations cannot resume an
existing experiment. Keep interrupted partial directories with an incomplete
suffix; retain completed failures instead of silently rerunning them. Publish
`result.json` after saving raw evidence. Resume and verification flag rounds
whose completion marker exists but required evidence files are missing.

Python checks measurement records: case identity, request IDs, input scheduling,
timestamps, counts, throughput arithmetic, percentile calculations and planned
coverage. It does not verify model outputs, actual token lengths or providers.
`protocol_pass` means execution and measurement-accounting success, not model
correctness. Performance-comparison eligibility additionally requires the
baseline profile and adequate resource sampling. Unavailable metrics stay N/A.

Schema 5 uses new result directories. Do not rewrite or resume historical v3/v4
datasets under this protocol.

## Single-table output

`REPORT.html` contains one fixed seven-column table. The first column is sticky;
there are no expanded details, full-view switches or extra report pages. Model
labels link to aggregate JSON and raw rounds. Above the table, show only the
title, hardware/OS information and a filter field. Historical reports may fill
missing hardware labels from a timestamped `environment-extra.json` bound to the
original machine record; never replace recorded values or modify the manifest.

1. Model / L / B / C / requested device, for example `L=32 · B=1 · C=1 · CPU`.
2. Batch p50 latency in milliseconds.
3. Vector throughput in vectors/s.
4. Embedding-phase RSS peak in MiB.
5. Process GPU memory peak in MiB, or N/A.
6. CPU mean / one-second peak in percent.
7. Device-wide GPU mean / peak in percent.

Markdown and `summary.json` have the same seven fields. For any case with an
execution/collection failure in any round, batch errors or timeouts, a process
hard timeout, incomplete rounds or an unsupported device, retain its first-column
identity and show `-` in every other table cell. Do not display successful-round
metrics for such a case. Successful cases still show N/A for unavailable resource
metrics.

`metrics.json` retains case identities, all latency percentiles, input-word
throughput, round and batch counts, failure information, numeric distributions,
sample counts, exclusions and raw-round links. Short validation runs are marked in the
report title; quality details remain available through case tooltips and JSON.

## Invocation

The runner supports macOS and Linux and requires Python 3.9+. Install its only
third-party Python dependency in a virtual environment (tested with `psutil`
7.2.2). CUDA additionally requires `nvidia-smi` and a CUDA-enabled Rust build.

```sh
cd rust
python3 -m venv .venv
. .venv/bin/activate
python3 -m pip install psutil==7.2.2
python3 -m unittest discover -s micro-bench/model -p 'test_benchmark.py'
cargo +1.98.0 test -p zg-engine --release --lib --no-run --message-format=json > build-v5.jsonl
python3 micro-bench/model/suite.py --build-json build-v5.jsonl --cache /absolute/cache --output /absolute/setup-v5 --prepare-only
python3 micro-bench/model/suite.py --build-json build-v5.jsonl --cache /absolute/cache --output /absolute/results-v5 --lengths 32 256 1024 --batch-sizes 1 4 16 --embedding-concurrency default 1 2 4 8 --devices cpu metal
python3 micro-bench/model/verify.py /absolute/results-v5
```

Use `--models` for a subset, `--devices cpu` for CPU-only execution and `--smoke`
for bounded runner validation. Build and runner tests precede performance runs;
no correctness audit is required to start a measurement.
