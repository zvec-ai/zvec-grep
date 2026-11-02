# Micro-benchmarks

Component-level performance benchmarks measure latency, throughput and resource
usage within a defined engine boundary. End-to-end agent and retrieval evaluations
such as SWE-QA-Bench live in [benchmarks](../benchmarks/README.md).

- [Model inference](model/PROTOCOL.md): embedding performance across models,
  input word counts, batch sizes, CLI concurrency and requested devices.
  The Python runner lives here; native workloads remain in the private model test
  module under `crates/zg-engine/src/models/tests/model_micro_bench/`.

The model micro-benchmark measures complete-call latency, throughput and external
CPU/RSS/GPU usage. It does not validate model correctness, actual token lengths
or the execution device, and needs no backend statistics probes.

The runner requires Python 3.9+ and `psutil`; see the
[environment setup](model/PROTOCOL.md#invocation). Run the Python checks from `rust/`:

```sh
python3 -m unittest discover -s micro-bench/model -p 'test_benchmark.py'
```

Keep generated reports and raw measurements in a separate output directory.
