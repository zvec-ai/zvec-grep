import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import {
  cp,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { pathToFileURL } from "node:url";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { createZvecGrep } from "../../dist/index.js";
import { CountingEmbeddingModel } from "../helpers/counting-embedding.mjs";
import { buildLegacyHome } from "../helpers/legacy-index.mjs";
import { useIsolatedZvecGrepHome } from "../helpers/isolated-home.mjs";
useIsolatedZvecGrepHome();
const cli = resolve("dist/cli/index.js");
const markerName = ".zvec-transfer-owner.json";
async function waitFor(predicate) {
  const deadline = Date.now() + 10000;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error("control did not execute");
    await delay(10);
  }
}
function launch(args, env) {
  const child = spawn(process.execPath, [cli, ...args], {
    env: { ...process.env, ...env },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stdout = "",
    stderr = "";
  child.stdout.on("data", (x) => (stdout += x));
  child.stderr.on("data", (x) => (stderr += x));
  const done = new Promise((resolve, reject) => {
    child.on("error", reject);
    child.on("close", (code, signal) =>
      resolve({ code, signal, stdout, stderr }),
    );
  });
  return { child, done };
}
function tree(path) {
  const files = [];
  const visit = (root) => {
    for (const entry of readdirSync(root, { withFileTypes: true })) {
      const p = join(root, entry.name);
      if (entry.isDirectory()) visit(p);
      else
        files.push([
          p.slice(path.length),
          createHash("sha256").update(readFileSync(p)).digest("hex"),
        ]);
    }
  };
  visit(path);
  return files.sort((a, b) => a[0].localeCompare(b[0]));
}
async function fixture(t) {
  const base = await mkdtemp(join(tmpdir(), "zg-cli-transfer-control-"));
  const temporary = join(base, "tmp"),
    home = join(base, "home"),
    root = join(base, "source"),
    legacy = join(base, "legacy"),
    destination = join(base, "destination"),
    control = join(base, "blocked.json"),
    release = join(base, "continue"),
    preload = join(base, "preload.mjs");
  let service;
  const children = [];
  t.after(async () => {
    writeFileSync(release, "");
    for (const run of children) {
      if (run.child.exitCode === null && run.child.signalCode === null)
        run.child.kill("SIGKILL");
      await run.done;
    }
    if (existsSync(control)) {
      const { pid } = JSON.parse(readFileSync(control, "utf8"));
      try {
        if (pid !== process.pid) process.kill(pid, "SIGKILL");
      } catch {
        /* The owned test process has already exited. */
      }
    }
    await service?.close();
    await rm(base, { recursive: true, force: true });
  });
  for (const p of [temporary, home, root, destination]) await mkdir(p);
  await writeFile(
    join(root, "one.md"),
    "# One\nunchanged lantern orchard phrase\n",
  );
  service = await createZvecGrep({
    root,
    embeddingModel: new CountingEmbeddingModel(),
  });
  await service.index();
  const info = await service.info();
  await service.close();
  service = undefined;
  await buildLegacyHome(root, legacy, info.workspaceIndex.id);
  await writeFile(
    preload,
    `import fs from 'node:fs';import {syncBuiltinESMExports} from 'node:module';import {dirname} from 'node:path';
const original=fs.cpSync;fs.cpSync=(source,target,options)=>{const result=original(source,target,options);
if(String(target).includes('zg-transfer-source-')&&!fs.existsSync(process.env.F5_CONTROL)){
fs.writeFileSync(process.env.F5_CONTROL,JSON.stringify({pid:process.pid,snapshot:dirname(target)}));
while(!fs.existsSync(process.env.F5_RELEASE))Atomics.wait(new Int32Array(new SharedArrayBuffer(4)),0,0,10);
}return result;};syncBuiltinESMExports();`,
  );
  const env = {
    ZVEC_GREP_HOME: home,
    TMPDIR: temporary,
    TMP: temporary,
    TEMP: temporary,
  };
  const start = () => {
    const run = launch(["--migrate-index", legacy, destination], {
      ...env,
      NODE_OPTIONS: `--import=${pathToFileURL(preload).href}`,
      F5_CONTROL: control,
      F5_RELEASE: release,
    });
    children.push(run);
    return run;
  };
  const recover = async (path) => {
    const run = launch(["--cleanup-transfer", path], env);
    children.push(run);
    return run.done;
  };
  return {
    base,
    temporary,
    root,
    legacy,
    destination,
    control,
    release,
    start,
    recover,
  };
}

for (const signal of ["SIGINT", "SIGTERM"]) {
  test(
    `F5 CLI ${signal} removes the owned source snapshot before it exits`,
    { skip: process.platform === "win32" },
    async (t) => {
      const f = await fixture(t),
        before = tree(f.legacy),
        run = f.start();
      await waitFor(() => existsSync(f.control));
      const observed = JSON.parse(await readFile(f.control, "utf8"));
      assert.ok(existsSync(observed.snapshot));
      run.child.kill(signal);
      await delay(100);
      await writeFile(f.release, "");
      const result = await run.done;
      assert.deepEqual(
        await readdir(f.temporary),
        [],
        "cancellation must remove its private source copy",
      );
      assert.deepEqual(tree(f.legacy), before);
      assert.equal(result.code, 1);
      assert.match(result.stderr, /cancelled/i);
      assert.equal(
        existsSync(join(f.destination, ".zvec-grep", "manifest.json")),
        false,
      );
    },
  );
}

test(
  "F5 recorded scratch recovery after SIGKILL removes only the dead transfer copy",
  { skip: process.platform === "win32" },
  async (t) => {
    const f = await fixture(t),
      before = tree(f.legacy),
      run = f.start();
    await waitFor(() => existsSync(f.control));
    const observed = JSON.parse(await readFile(f.control, "utf8"));
    const names = await readdir(f.temporary);
    const own = names.find((x) => x.startsWith("zg-portability-process-"));
    assert.ok(own, "CLI must record ownership before copying native data");
    const scratch = join(f.temporary, own);
    const owner = JSON.parse(await readFile(join(scratch, markerName), "utf8"));
    assert.equal(owner.pid, run.child.pid);
    assert.equal(owner.childPid, observed.pid);
    const unrelated = join(f.temporary, "unrelated");
    await mkdir(unrelated);
    await writeFile(join(unrelated, "keep"), "preserve");
    const active = await f.recover(scratch);
    assert.equal(active.code, 1);
    assert.match(active.stderr, /active or cannot be checked/);
    assert.ok(existsSync(observed.snapshot));
    run.child.kill("SIGKILL");
    await run.done;
    const childActive = await f.recover(scratch);
    assert.equal(childActive.code, 1);
    assert.match(childActive.stderr, /active or cannot be checked/);
    process.kill(observed.pid, "SIGKILL");
    await waitFor(() => {
      try {
        process.kill(observed.pid, 0);
        return false;
      } catch (error) {
        return error.code === "ESRCH";
      }
    });
    const sourceAfterKill = tree(f.legacy);
    const readerRecords = sourceAfterKill.filter(([path]) =>
      path.startsWith("/locks/"),
    );
    assert.equal(readerRecords.length, 1);
    assert.equal(
      JSON.parse(await readFile(join(f.legacy, readerRecords[0][0]), "utf8"))
        .pid,
      observed.pid,
    );
    assert.deepEqual(
      sourceAfterKill.filter(([path]) => !path.startsWith("/locks/")),
      before,
    );
    const destinationGuard = join(
      f.destination,
      ".zvec-grep",
      "locks",
      "home.write",
    );
    await mkdir(destinationGuard, { recursive: true });
    await writeFile(
      join(destinationGuard, "lock.json"),
      "retained destination owner\n",
    );
    await writeFile(
      join(f.destination, ".zvec-grep", "INCOMPLETE"),
      "retained incomplete marker\n",
    );
    const destinationBefore = tree(f.destination);
    const result = await f.recover(scratch);
    assert.equal(result.code, 0, result.stderr);
    assert.deepEqual(await readdir(f.temporary), ["unrelated"]);
    assert.equal(await readFile(join(unrelated, "keep"), "utf8"), "preserve");
    assert.deepEqual(
      tree(f.legacy),
      sourceAfterKill,
      "temporary recovery must not alter source locks",
    );
    assert.deepEqual(
      tree(f.destination),
      destinationBefore,
      "temporary recovery must not bypass destination locks",
    );
  },
);

test(
  "F5 recovery refuses a foreign directory, copied owner record and symlink",
  { skip: process.platform === "win32" },
  async (t) => {
    const f = await fixture(t),
      run = f.start();
    await waitFor(() => existsSync(f.control));
    const name = (await readdir(f.temporary)).find((x) =>
      x.startsWith("zg-portability-process-"),
    );
    assert.ok(name, "an ownership record is required");
    const scratch = join(f.temporary, name);
    const foreign = join(f.temporary, "zg-portability-process-foreign");
    await mkdir(foreign, { mode: 0o700 });
    await writeFile(join(foreign, "keep"), "foreign data");
    let result = await f.recover(foreign);
    assert.equal(result.code, 1);
    assert.ok(existsSync(join(foreign, "keep")));
    await cp(join(scratch, markerName), join(foreign, markerName));
    result = await f.recover(foreign);
    assert.equal(result.code, 1);
    assert.match(result.stderr, /ownership record does not match/i);
    const link = join(f.temporary, "zg-portability-process-link");
    await symlink(scratch, link);
    result = await f.recover(link);
    assert.equal(result.code, 1);
    assert.match(result.stderr, /Not an owned transfer scratch directory/);
    run.child.kill("SIGTERM");
    await delay(100);
    await writeFile(f.release, "");
    await run.done;
    assert.equal(await readFile(join(foreign, "keep"), "utf8"), "foreign data");
    assert.equal(existsSync(scratch), false);
  },
);

test(
  "F5 normal cleanup preserves a changed ownership record instead of trusting its new contents",
  { skip: process.platform === "win32" },
  async (t) => {
    const f = await fixture(t),
      run = f.start();
    await waitFor(() => existsSync(f.control));
    const name = (await readdir(f.temporary)).find((x) =>
      x.startsWith("zg-portability-process-"),
    );
    assert.ok(name, "an ownership record is required");
    const scratch = join(f.temporary, name),
      marker = join(scratch, markerName);
    const changed = JSON.parse(await readFile(marker, "utf8"));
    changed.childPid = null;
    const bytes = JSON.stringify(changed) + "\n";
    await writeFile(marker, bytes);
    run.child.kill("SIGTERM");
    await delay(100);
    await writeFile(f.release, "");
    const result = await run.done;
    assert.equal(result.code, 1);
    assert.match(result.stderr, /identity changed/);
    assert.equal(
      await readFile(marker, "utf8"),
      bytes,
      "cleanup must preserve the changed record",
    );
  },
);
