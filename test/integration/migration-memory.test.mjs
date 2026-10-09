import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { largeLegacy } from "../helpers/large-legacy.mjs";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { useIsolatedZvecGrepHome } from "../helpers/isolated-home.mjs";

useIsolatedZvecGrepHome();

test(
  "migration and logical transfer stream a large index under a 128 MiB heap",
  { timeout: 180_000 },
  async (t) => {
    const parent = await createTemporaryDirectory(t, "zg-memory-");
    const { sourceHome } = await largeLegacy(parent, 12000);
    const { stdout } = await promisify(execFile)(
      process.execPath,
      [
        "--max-old-space-size=128",
        fileURLToPath(
          new URL("../fixtures/migration-memory.mjs", import.meta.url),
        ),
        sourceHome,
        parent,
        "12000",
      ],
      { timeout: 150_000, maxBuffer: 1024 * 1024 },
    );
    t.diagnostic(stdout.trim());
    assert.match(stdout, /"completed":true/);
  },
);
