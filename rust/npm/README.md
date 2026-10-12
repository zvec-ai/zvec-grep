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
3. Publish all native packages to the canary dist-tag.
4. Install and smoke-test every native package.
5. Publish the meta package last.
6. Promote the dist-tag only after meta-package install, update, and rollback
   smoke tests pass.

`.github/workflows/rust-publish.yml` builds and installs every target on its
native runner, checks CLI startup and direct/server search output, and collects
the candidates into a downloadable artifact. Regular runs do not publish.
Intel macOS installation and runtime checks must pass before the other four
platform builds start. Its ONNX Runtime with CoreML and zvec SDK are built from
pinned sources, cached, and bundled with the native package. The Intel release
targets macOS 14 and later.

Explicit publication runs from a `rust-v<version>`-tagged commit on `main` and
uses npm provenance with trusted publishing or `NPM_TOKEN`. Native packages are
published first and the meta package last. The default `rust-canary` dist-tag
keeps the TypeScript package as the default installation until `latest` is
explicitly selected. Developer certificate signing and Apple notarization can
be added to the native build steps independently of npm publishing.
