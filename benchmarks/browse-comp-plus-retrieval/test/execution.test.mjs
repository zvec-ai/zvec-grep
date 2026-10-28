import assert from "node:assert/strict";
import {
  mkdtemp,
  mkdir,
  readFile,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { setTimeout as delay } from "node:timers/promises";
import { fileProgress, measureIndex } from "../execution.mjs";
import { reportWriter } from "../run.mjs";

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), "browsecomp-lifecycle-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const evidence = join(root, "evidence");
  await mkdir(evidence);
  return { cwd: root, evidence, timeout: 5000, heartbeat: 100, killGrace: 100 };
}

test("file counts come from product progress, not elapsed-time extrapolation", () => {
  assert.deepEqual(fileProgress("Indexing files: 56,981/100,195 embedding\r"), {
    completed_files: 56981,
    total_files: 100195,
  });
  assert.equal(fileProgress("Preparing model"), null);
});

test("raw logs and progress exist while indexing; fast exits and bounded tails are retained", async (t) => {
  const options = await fixture(t);
  let live = false;
  const code = `
    const fs = require('node:fs');
    fs.mkdirSync('.zvec-grep'); fs.writeFileSync('.zvec-grep/index', 'abc');
    process.stdout.write('x'.repeat(200000));
    process.stderr.write('Indexing files: 4/100195\\n');
    const timer=setInterval(()=>{if(fs.existsSync('progress-ack')) {clearInterval(timer);process.exit(0);}},20);
  `;
  const result = await measureIndex(process.execPath, ["-e", code], {
    ...options,
    onProgress: async (progress) => {
      if (progress.status === "running" && progress.completed_files === 4) {
        live = true;
        assert.ok(
          (await stat(join(options.evidence, "index.stdout.log"))).size > 65536,
        );
        await writeFile(join(options.cwd, "progress-ack"), "observed");
      }
    },
  });
  assert.ok(live, "progress was persisted before child exit");
  assert.ok(result.stdout.length <= 65536);
  assert.equal(result.measurements.index_bytes, 3);
  assert.equal(result.measurements.status, "success");
  assert.equal(
    JSON.parse(await readFile(join(options.evidence, "index-progress.json")))
      .completed_files,
    4,
  );
});

test("timeout kills descendants and preserves failed phase timing and logs", async (t) => {
  const options = await fixture(t);
  const heartbeat = join(options.cwd, "child-heartbeat");
  const childCode = `const fs=require('node:fs');setInterval(()=>fs.appendFileSync(${JSON.stringify(heartbeat)},'x'),20);`;
  const code = `
    require('node:child_process').spawn(process.execPath, ['-e', ${JSON.stringify(childCode)}], {stdio:'ignore'});
    process.on('SIGTERM',()=>{}); console.log('Indexing files: 7/100195');
    setInterval(()=>{},100);
  `;
  await assert.rejects(
    measureIndex(process.execPath, ["-e", code], { ...options, timeout: 5000 }),
    (error) => {
      assert.equal(error.result.timed_out, true);
      assert.equal(error.measurements.status, "failed");
      assert.ok(error.measurements.total_ms > 0);
      return true;
    },
  );
  const before = (await stat(heartbeat)).size;
  await delay(150);
  assert.equal(
    (await stat(heartbeat)).size,
    before,
    "descendant is still running",
  );
  const saved = JSON.parse(
    await readFile(join(options.evidence, "index.json")),
  );
  assert.equal(saved.measurements.status, "failed");
  assert.equal(saved.measurements.index_bytes, undefined);
  assert.match(
    await readFile(join(options.evidence, "index.stdout.log"), "utf8"),
    /7\/100195/,
  );
});

test("cancellation retains evidence and differs from a timeout", async (t) => {
  const options = await fixture(t);
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 5000);
  t.after(() => clearTimeout(timer));
  await assert.rejects(
    measureIndex(
      process.execPath,
      ["-e", "console.log('started');setInterval(()=>{},100)"],
      {
        ...options,
        signal: controller.signal,
        onProgress: async () => {
          const log = await readFile(
            join(options.evidence, "index.stdout.log"),
            "utf8",
          );
          if (log.includes("started")) controller.abort();
        },
      },
    ),
    (error) => {
      assert.equal(error.result.cancelled, true);
      assert.equal(error.result.timed_out, false);
      assert.equal(error.measurements.status, "failed");
      return true;
    },
  );
  assert.match(
    await readFile(join(options.evidence, "index.stdout.log"), "utf8"),
    /started/,
  );
});

test("missing executable fails promptly with an on-disk measurement", async (t) => {
  const options = await fixture(t);
  await assert.rejects(
    measureIndex(join(options.cwd, "does-not-exist"), [], options),
  );
  const saved = JSON.parse(
    await readFile(join(options.evidence, "index.json")),
  );
  assert.equal(saved.measurements.status, "failed");
});

test("a zero exit without a measurable index remains failed and retains timing", async (t) => {
  const options = await fixture(t);
  await assert.rejects(
    measureIndex(process.execPath, ["-e", "process.exit(0)"], options),
    (error) => {
      assert.equal(error.result.code, 0);
      assert.equal(error.measurements.status, "failed");
      assert.equal(error.measurements.index_bytes, null);
      assert.ok(error.measurements.total_ms > 0);
      return true;
    },
  );
});

test("concurrent checkpoints keep completed calls without declaring incomplete arms green", async (t) => {
  const { cwd } = await fixture(t);
  const report = {
    status: "incomplete",
    commit: "a".repeat(40),
    mode: "query",
    harness_commit: "b".repeat(40),
    errors: [],
    arm: { rows: [], performance_groups: [] },
  };
  const write = reportWriter(cwd, report);
  await Promise.all([write(), write(), write()]);
  const saved = JSON.parse(await readFile(join(cwd, "report.json")));
  assert.equal(saved.status, "incomplete");
  assert.equal(saved.arm.performance.index.total_ms.total, null);
  assert.doesNotMatch(await readFile(join(cwd, "summary.md"), "utf8"), /✅/);
});
