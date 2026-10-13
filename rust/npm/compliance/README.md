# Native publication inputs

Registry publication is currently blocked until reviewed license texts and
SBOMs exist for every target in `../platforms.json`. Downloadable candidate
artifacts can still be built without these files.

Prepare the following directory for each target, such as `darwin-arm64`:

```text
<target>/LICENSES/<component-license-files>
<target>/SBOM.spdx.json
```

Include the project license and all redistributed Rust dependencies, native
SDKs, model runtimes, dictionary data, and their transitive components. The
SPDX 2.3 document must describe the actual release version and bundled payload.
These inputs need maintainer review; listing a file in an npm `files` allowlist
does not provide its contents, and placeholder documents do not establish
coverage.

Packaging copies complete target inputs alongside the checksummed runtime
payload. The release tarball digest covers the compliance files too.
A requested publication checks every target before building,
then independently checks every native tarball before the first npm mutation.
Missing or empty license texts, a missing SBOM, or an invalid SPDX document
blocks publication and promotion. Presence checks do not replace license review.
