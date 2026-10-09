import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, mkdir, readFile, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import test from "node:test";
const exec = promisify(execFile);
test(
  "F5 recovery accepts an alias in the parent path but refuses a linked scratch entry",
  { skip: process.platform === "win32" },
  async (t) => {
    const base = await mkdtemp(join(tmpdir(), "zg-scratch-alias-control-"));
    t.after(() => rm(base, { recursive: true, force: true }));
    const real = join(base, "real"),
      alias = join(base, "alias"),
      link = join(base, "zg-portability-process-link");
    await mkdir(real);
    await symlink(real, alias);
    const module = pathToFileURL(
      resolve("dist/engine/storage/transfer-scratch.js"),
    ).href;
    const prepared = await exec(
      process.execPath,
      [
        "--input-type=module",
        "-e",
        `import {createTransferScratch} from ${JSON.stringify(module)};import {writeFileSync} from 'node:fs';import {join} from 'node:path';const s=createTransferScratch();writeFileSync(join(s.path,'snapshot'),'private copy');console.log(s.path);`,
      ],
      { env: { ...process.env, TMPDIR: real, TMP: real, TEMP: real } },
    );
    const scratch = prepared.stdout.trim();
    assert.equal(
      await readFile(join(scratch, "snapshot"), "utf8"),
      "private copy",
    );
    await symlink(scratch, link);
    const cli = resolve("dist/cli/index.js");
    await assert.rejects(
      exec(process.execPath, [cli, "--cleanup-transfer", link]),
      (error) => {
        assert.equal(error.code, 1);
        assert.match(error.stderr, /Not an owned transfer scratch directory/);
        return true;
      },
    );
    const recovered = await exec(process.execPath, [
      cli,
      "--cleanup-transfer",
      join(alias, basename(scratch)),
    ]);
    assert.match(
      recovered.stdout,
      /Removed the abandoned transfer scratch directory/,
    );
    await assert.rejects(readFile(join(scratch, "snapshot")), {
      code: "ENOENT",
    });
  },
);
