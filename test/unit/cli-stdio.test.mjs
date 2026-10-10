import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { handleStdoutError } from "../../dist/cli/stdio.js";

test("CLI exits cleanly with a closed stdout pipe", async () => {
  const child = spawn(
    process.execPath,
    [
      fileURLToPath(new URL("../../dist/cli/index.js", import.meta.url)),
      "--help",
    ],
    { stdio: ["ignore", "pipe", "pipe"], timeout: 10_000 },
  );
  let stderr = "";
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  const closed = new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (code, signal) => resolve({ code, signal }));
  });
  child.stdout.destroy();

  assert.deepEqual(await closed, { code: 0, signal: null });
  assert.equal(stderr, "");
});

test("stdout EPIPE exits cleanly when a downstream pipe closes", () => {
  let exitCode;
  const error = Object.assign(new Error("write EPIPE"), { code: "EPIPE" });

  assert.throws(
    () =>
      handleStdoutError(error, (code) => {
        exitCode = code;
        throw new Error("exit intercepted");
      }),
    /exit intercepted/,
  );

  assert.equal(exitCode, 0);
});

test("stdout errors other than EPIPE are still raised", () => {
  const error = Object.assign(new Error("write failure"), { code: "EIO" });

  assert.throws(() => handleStdoutError(error, () => 0), error);
});
