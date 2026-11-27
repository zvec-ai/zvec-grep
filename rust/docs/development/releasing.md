# Release the standalone installers

This guide covers the native archives consumed by
[install.sh](../../standalone/install.sh) and
[install.ps1](../../standalone/install.ps1). Run local commands from `rust/`.

The installers **do not require Node.js or npm on the user's machine**.
Unix uses a POSIX shell, curl or wget, tar and a SHA-256 utility; Windows uses
PowerShell and .NET. The downloaded native executable owns installation,
activation and recovery.

The maintainer toolchain still uses Node.js and npm to build and collect native
packages. Standalone archives are assembled from those verified payloads.
Changing the distribution entry point does not remove that build dependency.

## 1. Produce a candidate without publishing

Use [rust-publish.yml](../../../.github/workflows/rust-publish.yml).
Replace `OWNER/REPO` with the repository containing `your-branch`, and choose
the target matching your test machine:

```sh
gh workflow run rust-publish.yml --repo OWNER/REPO --ref your-branch \
  -f publish=false -f cuda=false -f platform=darwin-arm64
```

Other targets are listed in [platforms.json](../../npm/platforms.json).
`platform=all` builds all six targets. `cuda=true` adds GPU verification;
`publish=false` keeps the run unpublished. A single-platform candidate cannot
be promoted to a complete release.

Wait for the selected build and verification jobs, then find the run ID and
download the metadata plus your platform's archives:

```sh
gh run list --repo OWNER/REPO --workflow rust-publish.yml --branch your-branch
gh run download RUN_ID --repo OWNER/REPO \
  --pattern 'rust-standalone-*-metadata' \
  --pattern 'rust-standalone-*-darwin-arm64' \
  --dir dist/standalone
```

Use a fresh download directory for each candidate. Change both the platform
input and artifact pattern when testing another target.

## 2. Inspect and test the installer artifacts

`dist/standalone/` contains:

| Artifact | Purpose |
| --- | --- |
| `install.sh`, `install.ps1` | User-facing installers |
| `manifest.tsv` | Version, source commit, release tag, archive names, sizes and hashes |
| `SHA256SUMS` | Checksums for archives and installer metadata |
| `zvec-grep-<version>-<target>-<accelerator>.tar.gz` or `.zip` | Complete native runtime; Windows uses ZIP |

A complete release has six CPU archives and two additional CUDA archives for
Linux/Windows x64. Users download the archive for their platform and selected
accelerator; the installer does not fetch npm packages or compile code.

Run the installer smoke suite on the matching host:

```sh
node scripts/standalone-smoke.mjs --directory dist/standalone
```

The Node-based test harness serves the candidate through a local test server,
runs install/upgrade/recovery/uninstall cases in isolated directories, and
removes Node/npm from the installer's PATH. Results and command logs are written
under `dist/standalone-smoke/`. See [validation](validation.md) for other checks.

## 3. Assemble archives locally when needed

If you already have collected native tarballs and their `release.json`, use:

```sh
node scripts/standalone-release.mjs \
  --tarball-dir dist/npm/release/tarballs \
  --output-dir dist/standalone
```

This verifies and assembles existing payloads; it does not compile or publish.
It preserves licenses and notices and emits the same installer artifacts as CI.
Plain Cargo output is not sufficient input. The
[npm package guide](../../npm/README.md#local-packaging) documents preparation
of the intermediate packages.

## 4. Publish a stable GitHub Release

Update the version in [Cargo.toml](../../Cargo.toml), then create and push the
matching `vX.Y.Z` tag. Standalone publication requires a stable version, all
platforms, and the exact verified source tag/commit in `zvec-ai/zvec-grep`:

```sh
gh workflow run rust-publish.yml --repo zvec-ai/zvec-grep --ref vX.Y.Z \
  -f publish=true -f platform=all -f dist_tag=latest
```

The current workflow first completes native CPU/GPU checks, standalone installer
checks, npm publication and registry verification. Standalone publication waits
for successful npm `latest` promotion; there is no standalone-only publishing
mode yet. The existing [npm publication permissions](../../npm/README.md#publication-policy)
are therefore still required.

The publisher uploads to a draft GitHub Release, verifies the complete asset
inventory against GitHub's sizes and hashes, and publishes only after validation.
Retries accept identical published assets without replacing them. Creating a
Git tag or uploading an incomplete candidate does not make a working installer
endpoint.

After publication, users run the scripts in
[install and upgrade](../../standalone/README.md#install-and-upgrade).
Re-running an installer upgrades the installation; an explicit version selects
that release. The scripts install published archives, not the current checkout.
