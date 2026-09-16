import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, readdir, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  createDaemonLogger,
  opaqueIdentity,
  rootIdentity,
} from "../dist/daemon/logger.js";
import { runWithTraceContext } from "../dist/observability/trace-context.js";

test("structured daemon logs omit credentials and full query text", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-logger-"));
  t.after(async () => rm(home, { recursive: true, force: true }));
  const logger = createDaemonLogger(home);
  logger.event("request.completed", {
    request_id: "request-1",
    root_id: rootIdentity("/private/repository"),
    model_id: opaqueIdentity("provider/model/endpoint"),
    duration_ms: 12,
    token: "secret-token-value",
    api_key: "secret-api-key-value",
    query_text: "complete sensitive query text",
  });
  await logger.flush();

  const path = join(home, "daemon", "logs", "server.log");
  const text = await readFile(path, "utf8");
  const record = JSON.parse(text.trim());
  assert.equal(record.event, "request.completed");
  assert.equal(record.request_id, "request-1");
  assert.equal(record.duration_ms, 12);
  assert.doesNotMatch(
    text,
    /secret-token|secret-api-key|sensitive query|private\/repository/,
  );
  if (process.platform !== "win32")
    assert.equal((await stat(path)).mode & 0o777, 0o600);
});

test("structured daemon logs correlate with the active trace", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-trace-log-"));
  t.after(async () => rm(home, { recursive: true, force: true }));
  const logger = createDaemonLogger(home);
  runWithTraceContext(
    {
      traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
      traceId: "4bf92f3577b34da6a3ce929d0e0e4736",
    },
    () => logger.event("embedding.request"),
  );
  await logger.flush();

  const record = JSON.parse(
    (await readFile(join(home, "daemon", "logs", "server.log"), "utf8")).trim(),
  );
  assert.equal(record.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
});

test("rotation preserves queued UTF-8 records and bounds backup count", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-rotation-"));
  t.after(() => rm(home, { recursive: true, force: true }));
  const fields = { message: "日志".repeat(20) };
  const bytes = Buffer.byteLength(
    `${JSON.stringify({
      timestamp: new Date().toISOString(),
      event: "entry",
      level: "info",
      ...fields,
      sequence: 0,
    })}\n`,
  );
  const logger = createDaemonLogger(home, { maxBytes: bytes, keep: 2 });
  for (let sequence = 0; sequence < 5; sequence++) {
    logger.event("entry", { ...fields, sequence });
  }
  await logger.flush();
  const path = join(home, "daemon", "logs", "server.log");
  for (const [suffix, sequence] of [
    [".1", 4],
    [".2", 3],
  ]) {
    const text = await readFile(`${path}${suffix}`, "utf8");
    assert.equal(Buffer.byteLength(text), bytes);
    assert.equal(JSON.parse(text).sequence, sequence);
    if (process.platform !== "win32") {
      assert.equal((await stat(`${path}${suffix}`)).mode & 0o777, 0o600);
    }
  }
  assert.equal(await readFile(path, "utf8"), "");
  await assert.rejects(stat(`${path}.3`), { code: "ENOENT" });
});

test("restart rotates an existing oversized file and keeps whole records", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-restart-log-"));
  t.after(() => rm(home, { recursive: true, force: true }));
  const first = createDaemonLogger(home);
  first.event("before-restart", { message: "x".repeat(300) });
  await first.flush();
  const path = join(home, "daemon", "logs", "server.log");
  await assert.rejects(stat(`${path}.1`), { code: "ENOENT" });
  const second = createDaemonLogger(home, { maxBytes: 200, keep: 1 });
  second.event("after-restart");
  await second.flush();
  assert.equal(JSON.parse(await readFile(path, "utf8")).event, "after-restart");
  assert.equal(
    JSON.parse(await readFile(`${path}.1`, "utf8")).event,
    "before-restart",
  );
});

test("keep zero discards old logs without creating backups", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-no-backup-"));
  t.after(() => rm(home, { recursive: true, force: true }));
  const logger = createDaemonLogger(home, { maxBytes: 1, keep: 0 });
  logger.event("old");
  await logger.flush();
  const next = createDaemonLogger(home, { maxBytes: 200, keep: 0 });
  next.event("new");
  await next.flush();
  await logger.flush();
  const path = join(home, "daemon", "logs", "server.log");
  assert.equal(JSON.parse(await readFile(path, "utf8")).event, "new");
  await assert.rejects(stat(`${path}.1`), { code: "ENOENT" });
});

test("lowering retention removes surplus numbered backups after restart", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-retention-"));
  t.after(() => rm(home, { recursive: true, force: true }));
  const first = createDaemonLogger(home, { maxBytes: 1, keep: 5 });
  for (let i = 0; i < 5; i++) first.event("entry", { sequence: i });
  await first.flush();
  const directory = join(home, "daemon", "logs");
  assert.equal((await readdir(directory)).length, 6);
  for (const keep of [2, 0]) {
    const logger = createDaemonLogger(home, { maxBytes: 1, keep });
    logger.event("entry");
    await logger.flush();
    assert.deepEqual((await readdir(directory)).sort(), [
      "server.log",
      ...Array.from({ length: keep }, (_, i) => `server.log.${i + 1}`),
    ]);
  }
});

test("flush drains queued records and allows subsequent writes", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-flush-"));
  t.after(() => rm(home, { recursive: true, force: true }));
  const logger = createDaemonLogger(home);
  for (let i = 0; i < 100; i++) logger.event("entry", { sequence: i });
  const firstFlush = logger.flush();
  logger.event("entry", { sequence: 100 });
  await Promise.all([firstFlush, logger.flush(), logger.flush()]);
  const records = (
    await readFile(join(home, "daemon", "logs", "server.log"), "utf8")
  )
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line));
  assert.deepEqual(
    records.map((record) => record.sequence),
    Array.from({ length: 101 }, (_, i) => i),
  );
});

test("stream errors do not crash the daemon or prevent recovery after flush", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-log-error-"));
  t.after(() => rm(home, { recursive: true, force: true }));
  const path = join(home, "daemon", "logs", "server.log");
  await mkdir(path, { recursive: true });
  const logger = createDaemonLogger(home);
  logger.event("cannot-write");
  await logger.flush();
  await rm(path, { recursive: true });
  logger.event("recovered");
  await logger.flush();
  assert.equal(JSON.parse(await readFile(path, "utf8")).event, "recovered");
});
