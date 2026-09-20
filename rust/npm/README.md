# npm distribution

The release layout has one small meta package and one native package for each
published target. `platforms.json` is the single source of truth for package
names and npm `os`, `cpu`, and `libc` selectors.

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

1. Build, sign, and package every native target.
2. Verify payload closure, checksums, licenses, and SBOMs.
3. Publish all native packages to the canary dist-tag.
4. Install and smoke-test every native package.
5. Publish the meta package last.
6. Promote the dist-tag only after meta-package install, update, and rollback
   smoke tests pass.

`scripts/npm-release.mjs` only stages and packs packages. Publishing and moving
dist-tags remain intentionally outside the script until the release CI owns
credentials, provenance, signing, and approval policy.
