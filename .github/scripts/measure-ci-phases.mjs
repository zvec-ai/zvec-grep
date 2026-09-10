import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { appendFileSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { cpus, platform, release, tmpdir, totalmem } from "node:os";
import { basename, join } from "node:path";
import { performance } from "node:perf_hooks";
import { fileURLToPath } from "node:url";

const output = process.env.PHASE_OUTPUT ?? "phase-results.jsonl";
function emit(record) {
  const line = JSON.stringify(record);
  appendFileSync(output, line + "\n");
  console.log(line);
}

if (!process.argv.includes("--child")) {
  emit({
    kind: "environment",
    node: process.version,
    platform: platform(),
    release: release(),
    arch: process.arch,
    cpu: cpus()[0]?.model,
    cores: cpus().length,
    memory: totalmem(),
    image: process.env.ImageVersion,
    sha: process.env.GITHUB_SHA,
  });
  for (let iteration = 1; iteration <= 5; iteration++) {
    const child = spawnSync(
      process.execPath,
      [fileURLToPath(import.meta.url), "--child"],
      {
        env: { ...process.env, PHASE_ITERATION: String(iteration) },
        stdio: "inherit",
        timeout: 180_000,
      },
    );
    assert.equal(
      child.status,
      0,
      `iteration ${iteration}: ${child.error ?? child.signal ?? "failed"}`,
    );
  }
} else {
  const iteration = Number(process.env.PHASE_ITERATION);
  let phase = "load";
  // Buffer nested measurements to keep logging out of the timed operations.
  const records = [];
  function measure(name, operation, details = {}) {
    const start = performance.now();
    const cpu = process.cpuUsage();
    const phaseAtStart = phase;
    function finish(error) {
      const usage = process.cpuUsage(cpu);
      records.push({
        kind: "measurement",
        iteration,
        phase: phaseAtStart,
        name,
        ms: performance.now() - start,
        cpuMs: (usage.user + usage.system) / 1000,
        ...details,
        ...(error ? { error: String(error) } : {}),
      });
    }
    try {
      const value = operation();
      if (value && typeof value.then === "function") {
        return value.then(
          (result) => {
            finish();
            return result;
          },
          (error) => {
            finish(error);
            throw error;
          },
        );
      }
      finish();
      return value;
    } catch (error) {
      finish(error);
      throw error;
    }
  }
  const require = createRequire(import.meta.url);
  const zvec = measure("module.zvec", () => require("@zvec/zvec"));
  function instrumentCollection(collection, label) {
    const methods = new Map();
    return new Proxy(collection, {
      get(target, key) {
        const value = Reflect.get(target, key, target);
        if (typeof value !== "function") return value;
        if (!methods.has(key))
          methods.set(key, (...args) =>
            measure(
              `native.${String(key)}`,
              () =>
                value.apply(
                  target,
                  process.env.PHASE_OPTIMIZE_CONCURRENCY === "1" &&
                    ["optimize", "optimizeSync"].includes(String(key))
                    ? [{ ...args[0], concurrency: 1 }]
                    : args,
                ),
              {
                collection: label,
              },
            ),
          );
        return methods.get(key);
      },
    });
  }
  // Install hooks before the ESM facade captures the CommonJS exports.
  for (const name of ["ZVecCreateAndOpen", "ZVecOpen", "ZVecInitialize"]) {
    const original = zvec[name];
    zvec[name] = (...args) => {
      const label = typeof args[0] === "string" ? basename(args[0]) : undefined;
      const value = measure(`native.${name}`, () => original(...args), {
        collection: label,
      });
      return name === "ZVecInitialize"
        ? value
        : instrumentCollection(value, label);
    };
  }
  const { createZvecGrep } = await measure(
    "module.service",
    () => import("../../dist/index.js"),
  );
  const { FakeEmbeddingModel } =
    await import("../../test/helpers/fake-embedding.mjs");
  const embedding = new FakeEmbeddingModel();
  const embed = embedding.doEmbed.bind(embedding);
  embedding.doEmbed = (...args) =>
    measure("embedding.fake", () => embed(...args));
  const directory = await mkdtemp(join(tmpdir(), "zvec-ci-phases-"));
  const root = join(directory, "repo");
  let service;
  async function stage(name, operation) {
    phase = name;
    const result = await measure("stage", operation);
    if (result?.timings)
      records.push({
        kind: "pipeline",
        iteration,
        phase,
        timings: result.timings,
      });
    return result;
  }
  try {
    await stage("fixture", async () => {
      await mkdir(join(root, "src"), { recursive: true });
      await writeFile(
        join(root, "src", "alpha.ts"),
        "export const UniqueAlphaSymbol = 41;\n",
      );
      await writeFile(
        join(root, "src", "ignored.log"),
        "UniqueIgnoredSymbol\n",
      );
    });
    service = await stage("service.create", () =>
      createZvecGrep({
        root,
        home: join(directory, "home"),
        embeddingModel: embedding,
      }),
    );
    const indexed = await stage("index", () =>
      service.index({ includePaths: ["src/**"], excludePaths: ["**/*.log"] }),
    );
    assert.equal(indexed.filesAdded, 1);
    assert.equal((await stage("info", () => service.info())).indexed, true);
    const first = await stage("search", () =>
      service.context({ root, query: "UniqueAlphaSymbol", limit: 5 }),
    );
    assert.ok(
      first.items.some((item) => item.file.relativePath.endsWith("alpha.ts")),
    );
    assert.ok(
      first.items.every(
        (item) => !item.file.relativePath.endsWith("ignored.log"),
      ),
    );
    await stage("fixture.update", () =>
      writeFile(
        join(root, "src", "alpha.ts"),
        "export const UniqueUpdatedSymbol = 42;\n",
      ),
    );
    const refreshed = await stage("refresh.search", () =>
      service.context({ root, query: "UniqueUpdatedSymbol", limit: 5 }),
    );
    assert.ok(
      refreshed.items.some((item) =>
        item.file.relativePath.endsWith("alpha.ts"),
      ),
    );
    assert.equal(await stage("drop", () => service.dropIndex()), true);
    assert.equal(await stage("drop.absent", () => service.dropIndex()), false);
    assert.equal(
      (await stage("info.absent", () => service.info())).indexed,
      false,
    );
  } finally {
    try {
      if (service) await stage("close", () => service.close());
    } finally {
      await stage("cleanup", () =>
        rm(directory, { recursive: true, force: true }),
      );
      for (const record of records) emit(record);
    }
  }
}
