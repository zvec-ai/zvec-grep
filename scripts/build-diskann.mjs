import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import {
  copyFileSync,
  cpSync,
  existsSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const native = join(root, "native");
const output = join(
  native,
  `zg-diskann.${process.platform}-${process.arch}.node`,
);
const installing = process.argv.includes("--install");
if (installing && existsSync(output)) {
  try {
    createRequire(import.meta.url)(output);
    process.exit(0);
  } catch {
    console.warn(
      "The packaged DiskANN binding cannot load on this host; building from source.",
    );
  }
}

const source = join(root, "rust", "crates", "zg-diskann");
mkdirSync(native, { recursive: true });
if (existsSync(source)) {
  // Package a standalone copy of the same crate, so another host can compile
  // its Node binding without downloading the CLI's model runtime dependencies.
  cpSync(join(source, "src"), join(native, "src"), { recursive: true });
  copyFileSync(join(source, "build.rs"), join(native, "build.rs"));
  copyFileSync(
    join(source, "THIRD_PARTY_NOTICES.md"),
    join(native, "THIRD_PARTY_NOTICES.md"),
  );
  const workspace = readFileSync(join(root, "rust", "Cargo.toml"), "utf8");
  let manifest = readFileSync(join(source, "Cargo.toml"), "utf8");
  manifest = manifest.replace(/\[lints\]\s+workspace = true\s*/u, "");
  manifest = manifest.replace(/^([\w-]+)\.workspace = true$/gmu, (_, name) => {
    const value = workspace.match(new RegExp(`^${name} = (.+)$`, "m"))?.[1];
    if (value === undefined)
      throw new Error(`Missing workspace setting: ${name}`);
    return `${name} = ${value}`;
  });
  writeFileSync(join(native, "Cargo.toml"), `${manifest}\n[workspace]\n`);
  copyFileSync(join(root, "rust", "Cargo.lock"), join(native, "Cargo.lock"));
}
const target = join(native, "target");
const cargo = process.env.CARGO ?? "cargo";
const result = spawnSync(
  cargo,
  [
    "build",
    "--release",
    "--features",
    "node",
    "--manifest-path",
    join(native, "Cargo.toml"),
    "--target-dir",
    target,
  ],
  {
    stdio: "inherit",
    // Let rustup select the repository's pinned toolchain during npm builds.
    cwd: existsSync(source) ? join(root, "rust") : root,
  },
);
if (result.error)
  throw new Error(
    `Building DiskANN requires Rust 1.98+ and a C compiler: ${result.error.message}`,
  );
if (result.status !== 0) process.exit(result.status ?? 1);
const library =
  process.platform === "win32"
    ? "zg_diskann.dll"
    : process.platform === "darwin"
      ? "libzg_diskann.dylib"
      : "libzg_diskann.so";
copyFileSync(join(target, "release", library), output);
