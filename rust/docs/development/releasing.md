# Packaging and releasing

Run local commands from the workspace directory, currently `rust/`.
The [npm distribution contract](../../npm/README.md) owns package layout,
platform support and installer policy. [Validation](validation.md) owns the
CPU/GPU and registry gates. [Standalone installation](../../standalone/README.md)
covers native bundles and release assets.

## Prepare local candidates

Validate the release manifest and build the current platform dry-run packages:

```sh
npm run release:verify
npm run release:pack
```

Use `npm run release:cpu` for an explicitly CPU-only package.
Generated release tarballs live in `dist/npm/release/tarballs/`.

CI can package a previously built target artifact without rebuilding it:

```sh
node scripts/npm-release.mjs pack-platform \
  --target linux-x64-gnu \
  --binary /path/to/zg \
  --lib-dir /path/to/runtime-libs \
  --no-build
```

The runtime library directory must include zvec's shared library. Without
`--lib-dir`, runtime libraries are read from the binary's directory.
This entry point is CPU-only and applies the same mixed-directory check as
local `pack --no-build`, including when `--binary` or `--lib-dir` is explicit.

For a complete audited CUDA bundle, preserve its `bin/` and `lib/` layout:

```sh
node scripts/npm-release.mjs pack-platform \
  --target linux-x64-gnu \
  --bundle-dir target/release/zg-cuda \
  --no-build
```

Bundle mode accepts only regular files under `bin/` and `lib/`; `bin/`
contains exactly the target executable. It cannot be combined with
`--binary`, `--lib-dir`, or `--cpu-only`. All payload files are checksummed.

## Collect and inspect exact artifacts

`release:platform` creates native packages; `release:collect` packs the meta
package and records exact tarball hashes in `release.json`. Collection requires
the full platform/component set unless `--targets` explicitly requests a
validation-only subset:

```sh
npm run release:collect -- --targets linux-x64-gnu
npm run release:artifact-smoke
```

Run smoke checks on the matching host platform. `release:smoke` is an alias for
`release:artifact-smoke`. Both install collected bytes in an isolated npm
prefix and verify the CLI/daemon without compiling or repacking.
`-- --accelerator auto --expect-cuda` requires working CUDA installation;
`--expect-cpu` requires no CUDA requests or installation on a CPU host.
A failed local rebuild invalidates `native-build.json`; `--no-build` refuses
reuse until a build completes.

`release:collect` rejects missing or extra tarballs. Even a partial candidate's
meta package locks all six platform versions. `--output-dir <path>` keeps
inspection artifacts separate from the default release directory.
Each compressed tarball has a 180 MiB budget enforced at packing, collection
and publication preflight.

## Validate in CI

The release workflow is manually dispatched. Choose `all` or one native
target from [platforms.json](../../npm/platforms.json):

```sh
gh workflow run rust-publish.yml --ref <branch> \
  -f publish=false -f cuda=false -f platform=linux-x64-gnu
```

Set `cuda=true,publish=false` to include GPU validation. All selected builds
run in parallel; concurrency groups are scoped to branch and platform selection,
and newer runs do not cancel an in-progress run in the same group.
See [validation](validation.md#package-validation) for required checks and evidence.

All-platform runs retain `rust-release-<version>`; subset runs retain
`rust-candidate-<version>` with `partial: true`. Artifacts contain selected
native/component tarballs, the exact-version meta tarball, `SHA256SUMS`,
`release.json` with the source commit, and platform-specific installation
commands. Follow those commands to inspect a candidate locally. Artifacts are
retained for 14 days. A downloadable candidate alone does not prove runtime
validation succeeded.

## Publish and promote

Update the version in [Cargo.toml](../../Cargo.toml). Package versions come from
Cargo, not the tag name. `publish=true` requires `platform=all`.
`rust-canary` permits `main` or a tag; `latest` requires a tag.

```sh
gh workflow run rust-publish.yml --ref main -f publish=true
gh workflow run rust-publish.yml --ref <tag> -f publish=true -f dist_tag=latest
```

The selected ref must contain the intended workflow and version changes.
Every publication first uses `rust-canary`: runtime components, base native
packages, then the meta package. `latest` is a separate promotion after the
entire registry validation matrix passes. Ordinary build/pack commands neither
publish nor move tags.

An already published version is reusable only when its tarball integrity matches
the collected candidate. `release:publish` supports retrying a partial
publication without replacing an existing version; `release:promote` moves tags
only for an identical published release.

Configure npm trusted publishing for all 15 packages with both **Allow npm
publish** and **Allow npm dist-tag**, or supply an eligible `NPM_TOKEN`.
Publication/promotion use npm 11.21.0 for OIDC dist-tag support. The release
workflow must have `id-token: write`, but that alone does not grant npm tag
permission. See [publication policy](../../npm/README.md#publication-policy).

## Standalone releases

`npm run release:standalone` assembles complete bundles from verified npm
payloads. `npm run release:standalone-smoke` validates the installers.
The workflow publishes standalone GitHub Release assets after the native
release gates; see the [standalone release workflow](../../standalone/README.md#release-workflow)
for manifests, installers and recovery behavior.
