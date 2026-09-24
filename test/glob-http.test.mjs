import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { createTemporaryDirectory } from "./helpers/fixtures.mjs";
import { inWorker } from "./helpers/glob-worker.mjs";

test("MCP searches with adversarial globs complete and leave the daemon responsive", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-glob-http-");
  await mkdir(join(root, "src"));
  await writeFile(
    join(root, "src", "authorization-operation.ts"),
    "export function globRegressionAnswer() { return 42; }\n",
  );
  const moduleUrl = (path) =>
    JSON.stringify(new URL(path, import.meta.url).href);
  await inWorker(
    `
    const { Client, StreamableHTTPClientTransport } = await import(${JSON.stringify(import.meta.resolve("@modelcontextprotocol/client"))});
    const { DaemonHttpServer } = await import(${moduleUrl("../dist/daemon/http-server.js")});
    const { DaemonBackend } = await import(${moduleUrl("../dist/daemon/backend.js")});
    const { createZvecGrep } = await import(${moduleUrl("../dist/index.js")});
    const { FakeEmbeddingModel } = await import(${moduleUrl("./helpers/fake-embedding.mjs")});
    const service = await createZvecGrep({ root: workerData, embeddingModel: new FakeEmbeddingModel() });
    try { await service.index(); } finally { await service.close(); }
    const backend = new DaemonBackend({ version: 'glob-test', modelPoolOptions: { createModel: () => new FakeEmbeddingModel() } });
    const server = new DaemonHttpServer({ host: '127.0.0.1', port: 0, version: 'glob-test', backend });
    const client = new Client({ name: 'glob-test', version: '1' }, { versionNegotiation: { mode: { pin: '2026-07-28' } } });
    try {
      const address = await server.start();
      const base = 'http://127.0.0.1:' + address.port;
      await client.connect(new StreamableHTTPClientTransport(new URL(base + '/mcp/admin')));
      const search = (globs) => client.callTool({ name: 'zvec_grep_search', arguments: { root: workerData, fts: 'globRegressionAnswer', globs } });
      const ordinary = await search(['*.ts']);
      assert.notEqual(ordinary.isError, true);
      assert.match(ordinary.content[0].text, /src\\/authorization-operation\\.ts/);
      for (const pattern of ['**'.repeat(20) + 'Z', '*a'.repeat(20) + 'Z']) {
        const [result, health] = await Promise.all([search([pattern]), fetch(base + '/healthz')]);
        assert.notEqual(result.isError, true);
        assert.doesNotMatch(result.content[0].text, /src\\/authorization-operation\\.ts/);
        assert.equal(health.status, 200);
      }
      assert.match((await search(['*.ts'])).content[0].text, /src\\/authorization-operation\\.ts/);
    } finally {
      await client.close();
      await server.close();
      await backend.close();
    }
  `,
    root,
    15_000,
  );
});
