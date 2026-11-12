import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const source = readFileSync(join(root, "scripts/build-zvec.py"), "utf8");
const zvecCommit = source.match(/^ZVEC_COMMIT = "([a-f0-9]{40})"/m)[1];
const nodeCommit = source.match(/^NODE_COMMIT = "([a-f0-9]{40})"/m)[1];
const native = join(root, "native");
assert.ok(
  existsSync(native),
  "Native runtime missing; run npm run build:native",
);
const directories = readdirSync(native)
  .filter((name) => name.startsWith("zvec-native-"))
  .map((name) => join(native, name));
if (existsSync(join(native, "manifest.json"))) directories.push(native);
const targets = new Set();
for (const directory of directories) {
  const manifest = JSON.parse(readFileSync(join(directory, "manifest.json")));
  assert.equal(manifest.zvec_commit, zvecCommit, "Wrong zvec source revision");
  assert.equal(manifest.node_commit, nodeCommit, "Wrong Node binding revision");
  const target = `${manifest.platform}-${manifest.arch}`;
  if (directory !== native) {
    assert.equal(directory, join(native, `zvec-native-${target}`));
  }
  const library = {
    darwin: "libzvec_c_api.dylib",
    linux: "libzvec_c_api.so",
    win32: "zvec_c_api.dll",
  }[manifest.platform];
  assert.ok(library, `Unsupported native platform: ${manifest.platform}`);
  const files = ["zvec_node_binding.node", library];
  if (manifest.platform === "win32") files.push("zvec_c_api.lib");
  assert.deepEqual(Object.keys(manifest.files).sort(), files.sort());
  for (const file of files) {
    assert.equal(
      createHash("sha256")
        .update(readFileSync(join(directory, file)))
        .digest("hex"),
      manifest.files[file],
      `Native checksum mismatch: ${target}/${file}`,
    );
  }
  for (const license of ["LICENSE.zvec", "LICENSE.zvec-node"]) {
    assert.ok(existsSync(join(directory, license)), `Missing ${license}`);
  }
  targets.add(target);
}
const required = process.argv.includes("--all")
  ? ["linux-x64", "darwin-arm64", "win32-x64"]
  : [`${process.platform}-${process.arch}`];
for (const target of required) {
  assert.ok(targets.has(target), `Native runtime missing for ${target}`);
}
console.log(`Verified native runtimes: ${[...targets].sort().join(", ")}`);
