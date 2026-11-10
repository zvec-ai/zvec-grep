import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import test from "node:test";
import { parseArgs } from "../../dist/cli/args.js";
import {
  handleStdoutError,
  installStdoutErrorHandler,
} from "../../dist/cli/stdio.js";
import { cliPath, createTemporaryDirectory } from "../helpers/fixtures.mjs";

function completion(child) {
  let stderr = "";
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  return new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (code, signal) => resolve({ code, signal, stderr }));
  });
}

for (const exitCode of [0, 1]) {
  test(`partial stdout consumption preserves exit ${exitCode} and async cleanup`, async () => {
    const child = spawn(
      process.execPath,
      [
        "--input-type=module",
        "--eval",
        `
          import { once } from "node:events";
          import { setImmediate } from "node:timers/promises";
          import { installStdoutErrorHandler } from ${JSON.stringify(new URL("../../dist/cli/stdio.js", import.meta.url).href)};
          installStdoutErrorHandler({ command: "query", options: {}, positionals: [] });
          const closed = once(process, "message");
          process.stdout.write("first chunk");
          await closed;
          process.disconnect();
          try {
            process.stdout.write("after consumer closed");
            await setImmediate();
            process.exitCode = ${exitCode};
          } finally {
            await setImmediate();
            console.error("cleanup completed");
          }
        `,
      ],
      { stdio: ["ignore", "pipe", "pipe", "ipc"], timeout: 10_000 },
    );
    const completed = completion(child);
    let received = false;
    child.stdout.once("data", () => {
      received = true;
      child.stdout.once("close", () => child.send("consumer closed"));
      child.stdout.destroy();
    });
    assert.deepEqual(await completed, {
      code: exitCode,
      signal: null,
      stderr: "cleanup completed\n",
    });
    assert.equal(received, true);
  });
}

for (const checkReady of [false, true]) {
  test(`CLI server status with closed stdout preserves check-ready=${checkReady}`, async (t) => {
    const home = await createTemporaryDirectory(t, "zvec-grep-stdio-");
    const child = spawn(
      process.execPath,
      [
        "--liftoff-only",
        cliPath,
        "--server",
        "status",
        "--home",
        home,
        ...(checkReady ? ["--check-ready"] : []),
      ],
      { stdio: ["ignore", "pipe", "pipe"], timeout: 10_000 },
    );
    const completed = completion(child);
    child.stdout.destroy();
    const result = await completed;
    assert.equal(result.signal, null);
    assert.equal(result.code, checkReady ? 1 : 0, result.stderr);
    assert.doesNotMatch(result.stderr, /EPIPE|Unhandled/);
    if (checkReady) assert.match(result.stderr, /server is not ready/);
  });
}

for (const args of [
  ["--server", "--stdio"],
  ["--server", "run"],
]) {
  test(`${args.join(" ")} retains ownership of stdout errors`, (t) => {
    const before = process.stdout.listeners("error");
    t.after(() => {
      for (const listener of process.stdout.listeners("error")) {
        if (!before.includes(listener))
          process.stdout.removeListener("error", listener);
      }
    });
    installStdoutErrorHandler(parseArgs(args));
    assert.deepEqual(process.stdout.listeners("error"), before);
  });
}

test("stdout errors other than EPIPE are still raised", () => {
  const error = Object.assign(new Error("write failure"), { code: "EIO" });
  assert.throws(() => handleStdoutError(error), error);
});
