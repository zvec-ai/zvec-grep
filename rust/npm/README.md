# npm distribution

The release layout has one small meta package and one native package for each
published target. `platforms.json` is the single source of truth for package
names and npm `os`, `cpu`, and `libc` selectors.

The current published targets are macOS ARM64/x64, Linux x64/ARM64 with glibc,
and Windows x64.

The platform package owns the complete runtime payload:

```text
bin/zg[.exe]
bin/*.dll                 # Windows runtime DLLs, when required
lib/*                     # Unix runtime libraries, when required
checksums.json
LICENSES/
SBOM.spdx.json
```

The meta package locks every platform package to its exact version through
`optionalDependencies`. Its `postinstall` selects the installed package,
validates package identity and every payload checksum, copies runtime libraries,
and replaces `bin/zg.exe` last. The npm command is always named `zg`; `.exe` is
used as the stable internal target so npm generates a direct Windows shim.

The published meta package supports Node.js 14.14 and newer for installation.
Node is not used after `postinstall`; `zg` runs the selected native executable
directly. The root workspace may require a newer Node version for development
and package tests without reducing the published package's compatibility.

Release order:

1. Build and package every native target; apply ad hoc signatures on macOS.
2. Verify payload closure, checksums, licenses, and SBOMs.
3. Publish all native packages to `rust-canary`, then publish the meta package
   to `rust-canary` last. Candidate publication never changes `latest`.
4. On every native runner, install the exact meta-package version from the
   registry alone, so npm must select its native optional dependency.
5. Verify CLI startup, managed-rg, daemon startup and IPC, then update from a
   distinct published baseline version and roll back to that version.
6. Promote native `latest` tags and then the meta `latest` tag in a separate job,
   only after all five registry install/update/rollback jobs pass.

`.github/workflows/rust-publish.yml` builds and installs every target on its
native runner, checks CLI startup, managed-rg and actual daemon IPC, and collects
the candidates into a downloadable artifact. Regular runs do not publish.
Intel macOS installation and runtime checks must pass before the other four
platform builds start. Its ONNX Runtime with CoreML and zvec SDK are built from
pinned sources, cached, and bundled with the native package. The Intel release
targets macOS 14 and later.

Windows prepares the prebuilt ONNX Runtime selected by locked `ort-sys` with
bounded download retries, verifies its SHA-256, and caches the complete archive
and extracted runtime before compiling. The original static-link configuration
is preserved.

Explicit publication runs from a `rust-v<version>`-tagged commit on `main` and
uses npm provenance with trusted publishing or `NPM_TOKEN`. Native packages are
published first and the meta package last, always under `rust-canary`. Selecting
`latest` requests the separate promotion job; any registry smoke failure leaves
the existing default installation unchanged.

Publication is currently blocked until reviewed license texts and SPDX SBOMs
exist for every native target under [`compliance/`](compliance/README.md).
Candidate artifacts can still be downloaded, but they are not publication-ready
while these inputs are missing. Both the initial source check and the tarball
preflight fail before publication when compliance inputs are incomplete.

Publication and promotion explicitly install npm 11.21.0, which supports OIDC
authentication for `npm dist-tag`. Configure trusted publishing for all six
packages with repository `zvec-ai/zvec-grep`, workflow filename
`rust-publish.yml`, and both **Allow npm publish** and **Allow npm dist-tag**.
The dist-tag permission is independent and disabled by default; `id-token:
write` alone does not grant it. See the [npm trusted publishing documentation](https://docs.npmjs.com/trusted-publishers/#managing-dist-tags-with-trusted-publishing).
An eligible `NPM_TOKEN` remains an alternative authentication method.

Developer certificate signing and Apple notarization can be added to the native
build steps independently of npm publishing.
