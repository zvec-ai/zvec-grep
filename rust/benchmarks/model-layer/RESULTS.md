# Model-layer backend benchmark results

Date: 2026-08-28

## Scope and method

- Rust: `dev/rust` at `a2d3b1a`, plus the current model-backend working tree.
- TypeScript: `main` at `d41ee79`.
- Machine: Apple M4 Pro, 12 cores, 48 GiB RAM.
- Toolchains: Rust 1.98.0 release build; Node.js 25.5.0.
- Local models used the same warm cache and generated text inputs. Downloads
  were completed before measurement.
- Each configuration ran in a fresh process. Main and Rust execution order was
  alternated. RSS was sampled every 10 ms.
- `loaded RSS` is total process RSS after warmup. `peak RSS` is the highest RSS
  during model loading, warmup, and measured inference. No-model process
  baselines were approximately 12 MiB for Rust and 75 MiB for main.
- Local models used `device=metal`. This means CPU lookup for Model2Vec, CoreML
  for Rust ONNX, WebGPU/Metal for main ONNX, and native Metal for llama.cpp.
- On macOS, llama.cpp ran with Metal residency sets disabled, matching main's
  default process configuration.
- Qwen used real DashScope requests. Its throughput includes network and remote
  service time; its RSS covers only the local HTTP client, not server-side model
  memory.

| Backend     | Representative model        | Batch | Vectors/round | Rounds | Process repeats |
| ----------- | --------------------------- | ----: | ------------: | -----: | --------------: |
| Model2Vec   | `local/potion-code-16m-v2`  |   256 |         8,192 |      3 |               3 |
| ONNX        | `local/all-minilm-l6-v2`    |     4 |           512 |      3 |               3 |
| llama.cpp   | `local/embeddinggemma-300m` |     4 |           128 |      3 |               3 |
| Qwen remote | `qwen/text-embedding-v4`    |    10 |            40 |      3 |               2 |

## Results

Values are medians across independent processes. Throughput is vectors per
second. Memory cells show `Rust / main` total process RSS.

| Backend     | Concurrency | Rust vectors/s | main vectors/s | Rust throughput vs main | Loaded RSS Rust/main |    Peak RSS Rust/main |
| ----------- | ----------: | -------------: | -------------: | ----------------------: | -------------------: | --------------------: |
| Model2Vec   |           1 |         42,278 |         26,181 |                  +61.5% |    137.0 / 146.5 MiB |     137.1 / 280.9 MiB |
| Model2Vec   |           4 |        100,508 |         73,687 |                  +36.4% |    139.5 / 266.9 MiB |     139.7 / 459.3 MiB |
| ONNX        |           1 |            294 |            764 |                  -61.5% |    472.9 / 200.2 MiB |     473.7 / 205.6 MiB |
| ONNX        |           4 |            325 |            767 |                  -57.6% |    478.8 / 201.2 MiB |     479.8 / 225.9 MiB |
| llama.cpp   |           1 |          134.0 |          113.8 |                  +17.8% |    787.0 / 634.1 MiB |   787.1 / 1,019.2 MiB |
| llama.cpp   |           4 |          142.0 |          118.1 |                  +20.2% |    841.5 / 705.8 MiB |   841.9 / 1,021.5 MiB |
| Qwen remote |           1 |           24.1 |           26.4 |                   -8.7% |     19.4 / 112.4 MiB |      20.1 / 116.6 MiB |
| Qwen remote |           4 |           63.9 |           75.9 |                  -15.8% |     21.6 / 117.8 MiB |      22.4 / 120.8 MiB |

## Findings

- Rust Model2Vec is both faster and substantially more memory-stable. Its RSS
  barely changes from concurrency 1 to 4 because the tokenizer, table, and
  compute pool are shared.
- Rust CoreML now shares one physical ONNX session across user concurrency and
  merges compatible small requests. Concurrency 4 adds only 5.9 MiB loaded RSS
  instead of creating four sessions and reaching 1.22 GiB. CoreML remains the
  main throughput and single-session memory regression versus main.
- Rust llama.cpp now selects Apple's `MTL` device instead of silently falling
  back to CPU, reuses context workers, sizes their batch buffers to the actual
  token load, and caps Metal at two physical contexts. It is 17.8-20.2% faster
  than main in this workload. Loaded RSS remains 136-153 MiB higher, but peak
  RSS is 177-232 MiB lower than main.
- Qwen results are dominated by remote latency and varied between processes.
  Rust's local client process uses about one sixth of main's RSS, but the
  throughput difference should not be interpreted as an embedding-kernel
  difference because both call the same remote service.
- Model2Vec checksums matched exactly across implementations. ONNX and
  llama.cpp checksums differ because the compared accelerated runtimes are not
  identical (CoreML versus WebGPU for ONNX and separate llama.cpp bindings);
  backend correctness remains covered by the vector-oracle and cosine tests.

## Alternative ONNX model routes on Apple Silicon

The two alternatives to CoreML were measured separately with
`local/all-minilm-l6-v2`, batch 4, 512 vectors per round, three rounds, and
three fresh-process repeats. The main WebGPU and CoreML rows are references
from the same benchmark setup.

| Route                | Artifact / device | Concurrency | Vectors/s | Loaded RSS | Peak RSS |
| -------------------- | ----------------- | ----------: | --------: | ---------: | -------: |
| Rust CoreML          | ONNX Q4 / CoreML  |           1 |       294 |  472.9 MiB | 473.7 MiB |
| Rust CoreML          | ONNX Q4 / CoreML  |           4 |       325 |  478.8 MiB | 479.8 MiB |
| Rust ORT CPU         | ONNX Q4 / MLAS    |           1 |       511 |   66.4 MiB |  67.7 MiB |
| Rust ORT CPU         | ONNX Q4 / MLAS    |           4 |       937 |  125.9 MiB | 147.1 MiB |
| Rust llama.cpp Metal | GGUF Q8_0 / Metal |           1 |       649 |  113.2 MiB | 113.3 MiB |
| Rust llama.cpp Metal | GGUF Q8_0 / Metal |           4 |       739 |  121.4 MiB | 121.5 MiB |
| Rust llama.cpp Metal | GGUF F16 / Metal  |           1 |       665 |  152.0 MiB | 152.1 MiB |
| Rust llama.cpp Metal | GGUF F16 / Metal  |           4 |       735 |  160.9 MiB | 161.2 MiB |
| main WebGPU          | ONNX Q4 / WebGPU  |           1 |       764 |  200.2 MiB | 205.6 MiB |
| main WebGPU          | ONNX Q4 / WebGPU  |           4 |       767 |  201.2 MiB | 225.9 MiB |

- ORT CPU is the best drop-in route for the current ONNX Q4 artifact: it keeps
  the existing vector output, reduces peak RSS by 67-71% versus main WebGPU,
  and scales to higher throughput at concurrency 4. Its concurrency-1
  throughput is 33% below main WebGPU. Consequently, macOS `auto` and `metal`
  requests for this Transformers model are routed to ORT CPU; an explicit
  `metal` request emits a warning instead of silently selecting the slower,
  larger CoreML path.
- llama.cpp with Q8_0 GGUF is a viable Metal route and uses less memory than
  main WebGPU, but it is not index-compatible with the current quantized ONNX
  model. Across four correctness probes its cosine similarity to the ONNX Q4
  output was 0.971-0.977, so switching existing indexes would require a
  rebuild.
- The converted GGUF output closely matched the original full-precision
  PyTorch model. The observed compatibility difference is therefore mainly
  caused by the existing ONNX Q4 quantization, rather than tokenization or the
  GGUF conversion.
- GGUF F16 used about 39 MiB more RSS than Q8_0 without a meaningful throughput
  improvement, so Q8_0 is the preferable llama.cpp artifact for this model.
