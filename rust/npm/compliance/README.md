# Optional native package metadata

Every release package includes the project's root `LICENSE`. Supplemental
license collections and SBOMs in this directory are optional for candidate
packaging, registry publication, and promotion.

To include supplemental metadata for a target such as `darwin-arm64`, provide
either or both of the following:

```text
<target>/LICENSES/<component-license-files>
<target>/SBOM.spdx.json
```

CUDA components can carry their own inputs under
`<target>/<component-suffix>/LICENSES/` and `SBOM.spdx.json`, for example
`linux-x64-gnu/ort-cuda-part1/` or `win32-x64-msvc/ggml-cuda13/`.
Any supplied SBOM should describe that package's actual payload (including fragment
provenance and the original library where relevant), not an unchanged SBOM for
the former monolithic package. See `platforms.json` for all component names.

When preparing these documents, account for redistributed Rust dependencies,
native SDKs, model runtimes, dictionary data, and their transitive components.
Use the actual release version and bundled payload. Listing a file in an npm
`files` allowlist does not provide its contents, and placeholder documents do
not establish coverage.

Packaging copies any supplied target or component inputs alongside the
checksummed runtime payload. Neither input requires the other, and the release
pipeline does not validate SBOM contents. The release tarball digest covers
these files too.

Upstream license and notice files already staged with native libraries remain
in the packages, including `UPSTREAM_NOTICES/` in redistributed ORT components.
Optional supplemental metadata does not remove third-party license obligations;
the project's Apache-2.0 license does not replace upstream licenses.
