import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { createTemporaryDirectory, runCli } from "../helpers/fixtures.mjs";
import { deterministicVector } from "../helpers/fake-embedding.mjs";

test("CLI completes index, search, automatic refresh, status, and rg workflows", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-e2e-",
  );
  const root = join(temporaryDirectory, "repo");
  const home = join(temporaryDirectory, "home");
  await mkdir(join(root, "src"), { recursive: true });
  await writeFile(
    join(root, "src", "example.ts"),
    "export const FirstWorkflowSymbol = 41;\n",
  );

  const server = createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const body = JSON.parse(Buffer.concat(chunks).toString("utf8"));
    const inputs = Array.isArray(body.input) ? body.input : [];
    response.writeHead(200, { "content-type": "application/json" });
    response.end(
      JSON.stringify({
        data: inputs.map((input, index) => ({
          index,
          embedding: deterministicVector(input, 1024),
        })),
      }),
    );
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(
    () =>
      new Promise((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      ),
  );
  const address = server.address();
  assert.ok(address && typeof address === "object");
  const endpoint = `http://127.0.0.1:${address.port}/embeddings`;
  const env = { HOME: home, NO_COLOR: "1" };

  const indexed = await runCli(
    [
      "--index",
      "--embedding",
      "qwen/text-embedding-v4",
      "--api-key",
      "test-key",
      "--endpoint",
      endpoint,
      root,
    ],
    { cwd: root, env, timeout: 120_000 },
  );
  assert.match(indexed.stdout, /Indexed|index/i);

  const first = await runCli(["FirstWorkflowSymbol", "--limit", "5"], {
    cwd: root,
    env,
    timeout: 120_000,
  });
  assert.match(first.stdout, /example\.ts/);

  await writeFile(
    join(root, "src", "example.ts"),
    "export const RefreshedWorkflowSymbol = 42;\n",
  );
  const refreshed = await runCli(["RefreshedWorkflowSymbol", "--limit", "5"], {
    cwd: root,
    env,
    timeout: 120_000,
  });
  assert.match(refreshed.stdout, /RefreshedWorkflowSymbol|example\.ts/);

  const status = await runCli(["--status", root], { cwd: root, env });
  assert.match(status.stdout, /enabled|indexed/i);

  const lexical = await runCli(
    ["--rg", "-F", "RefreshedWorkflowSymbol", "src"],
    { cwd: root, env },
  );
  assert.match(lexical.stdout, /RefreshedWorkflowSymbol/);
});

test("CLI exposes stable help, version, and failure behavior", async () => {
  const help = await runCli(["--help"]);
  assert.match(help.stdout, /Usage:/);
  const version = await runCli(["--version"]);
  assert.match(version.stdout.trim(), /^\d+\.\d+\.\d+/);
  await assert.rejects(runCli(["--definitely-invalid"]), (error) => {
    assert.equal(error.code, 1);
    assert.match(error.stderr, /Unknown option/);
    return true;
  });
});
