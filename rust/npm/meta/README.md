# zvec-grep

This is the npm distribution wrapper for the native `zg` CLI. The package uses
an exact-version optional dependency to install only the native package matching
the current operating system, CPU architecture, and Linux libc.

The install script verifies the native payload checksum and materializes the
`zg` executable. Running `zg` after installation enters the native binary
directly; it does not use a JavaScript launcher.

Node.js 14.14 or newer is required only while npm installs the package. The
installed command does not start Node.js.

If installation scripts or optional dependencies are disabled, reinstall
without `--ignore-scripts` and without `--omit=optional`.
