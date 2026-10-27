# Native publication inputs

Registry publication is currently blocked until reviewed license texts and
SBOMs exist for every target in `../platforms.json`. Downloadable candidate
artifacts can still be built without these files.

Prepare the following directory for each target, such as `darwin-arm64`:

```text
<target>/LICENSES/<component-license-files>
<target>/SBOM.spdx.json
```

CUDA components also need their own reviewed inputs under
`<target>/<component-suffix>/LICENSES/` and `SBOM.spdx.json`, for example
`linux-x64-gnu/ort-cuda-part1/` or `win32-x64-msvc/ggml-cuda13/`.
Each document must describe that package's actual payload (including fragment
provenance and the original library where relevant), not an unchanged SBOM for
the former monolithic package. See `platforms.json` for all component names.

Include the project license and all redistributed Rust dependencies, native
SDKs, model runtimes, dictionary data, and their transitive components. The
SPDX 2.3 document must describe the actual release version and bundled payload.
These inputs need maintainer review; listing a file in an npm `files` allowlist
does not provide its contents, and placeholder documents do not establish
coverage.

Packaging copies the corresponding target or component inputs alongside the checksummed runtime
payload. The release tarball digest covers the compliance files too.
A requested publication checks every target before building,
then independently checks every native tarball before the first npm mutation.
Missing or empty license texts, a missing SBOM, or an invalid SPDX document
blocks publication and promotion. Presence checks do not replace license review.
