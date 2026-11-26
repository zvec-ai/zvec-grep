# Choose the right validation

Run local commands from `rust/`. Use the checks relevant to the files you
changed; the workflows own the full platform and hardware matrices.

## Local checks

| Change | Check |
| --- | --- |
| Rust behavior | Run the affected crate's tests, for example `cargo test --locked -p zg-engine`, plus the ordinary [contributor checks](../../CONTRIBUTING.md#verification). |
| Standalone installers or release assembly | `node --test scripts/standalone-*.test.mjs` |
| Collected standalone archives | `node scripts/standalone-smoke.mjs --directory dist/standalone` on the matching platform; first [prepare or download the candidate](releasing.md). |
| npm packaging or installation | `node --test scripts/npm-*.test.mjs npm/meta/install.test.cjs` |
| ONNX runtime deployment | `python3 scripts/test-prepare-onnxruntime.py` and `python3 scripts/test-check-glibc.py` |
| PDF extraction or source mapping | `cargo test --locked -p zg-engine --lib pdf` and `cargo test --locked -p zg --test pdf_direct` |
| PDFium packaging | `python3 scripts/test-prepare-pdfium.py`; use `python3 scripts/pdf-package-smoke.py --help` for the packaged PDF index/query checks. |
| Documentation only | Check local links, heading anchors, command examples and `git diff --check`. |

The JavaScript test harnesses require Node.js; the standalone installer itself
does not. On Windows use `python` instead of `python3`.

## CI checks

| Goal | Workflow and inputs |
| --- | --- |
| Ordinary cross-platform verification | [rust-ci.yml](../../../.github/workflows/rust-ci.yml) runs workspace checks and real-model tests on push/PR. |
| GPU verification of Rust changes | Dispatch `rust-ci.yml` with `cuda_verify=true`, `platform=all`, `linux` or `windows`, and optional `ref`. |
| Validate installable artifacts without publication | Dispatch [rust-publish.yml](../../../.github/workflows/rust-publish.yml) with `publish=false`; select one platform or `all`, and set `cuda=true` when GPU checks are needed. |
| Full release | Follow [releasing](releasing.md). Publication requires the complete CPU/GPU, installer and registry gates. |

Build jobs run on CPU hosts. GPU jobs execute the verified binaries or packages
without rebuilding. GPU checks require real GGUF layer offload or ORT CUDA graph
execution; successful startup or CPU fallback is not sufficient.

Ordinary PR CI runs real PDF indexing and retrieval with PP-S on Linux,
Windows and macOS, reusing the built executable and model cache. Release checks
reuse the same [PDF smoke test](../../scripts/pdf-package-smoke.py) against packaged runtimes.

Standalone checks cover installation, upgrades, recovery and uninstall with
Node/npm absent from the installer's PATH. A retained candidate proves artifacts
were collected; verify the relevant test jobs also succeeded.

## Optional model-download checks

Ordinary local tests skip live downloads. When changing artifact acquisition,
run both source suites explicitly:

```sh
cargo test --locked -p zg-engine --lib models::artifacts::downloader::tests::live_sources:: -- --ignored --nocapture --test-threads=1
```

This downloads and verifies the configured local-model artifacts from each
source; it does not test inference. For a targeted local rerun, set
`ZVEC_GREP_TEST_DOWNLOAD_MODEL` to a catalog model reference.
Full-catalog ONNX diagnostics are available through
[validate-onnxruntime-models.py](../../scripts/validate-onnxruntime-models.py)
(`--help` lists its required inputs).

## Find failure evidence

| Failure | Evidence |
| --- | --- |
| Local standalone smoke | `dist/standalone-smoke/results.json` and `commands.json` |
| CI standalone installation | `standalone-evidence-<target>` artifact |
| CI model inference | `real-model-evidence-*` artifacts |
| CI native package execution | `cpu-artifact-evidence-*` or `cuda-cli-evidence-*` artifacts |
| Registry installation or upgrade | `registry-smoke-evidence-<target>` artifacts |

Start with the failing command and its output. Runner preparation, model
selections and detailed assertions live in the linked workflows and test
scripts rather than a second copy of the CI configuration here.
