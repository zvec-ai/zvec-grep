import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import test, { after } from "node:test";
import { Client, InMemoryTransport } from "@modelcontextprotocol/client";
import { createZvecGrepMcpServer } from "../../dist/mcp/tools.js";
import { DaemonBackend } from "../../dist/daemon/backend.js";
import { createZvecGrep } from "../../dist/index.js";
import { exportWorkspaceIndex } from "../../dist/engine/transfer/index.js";
import { CountingEmbeddingModel } from "../helpers/counting-embedding.mjs";
import { buildLegacyHome } from "../helpers/legacy-index.mjs";
import { useIsolatedZvecGrepHome } from "../helpers/isolated-home.mjs";
useIsolatedZvecGrepHome();
const bases = [];
const templates = new Map();
after(async () => {
  for (const base of bases) await rm(base, { recursive: true, force: true });
});
async function source(longPaths) {
  if (templates.has(longPaths)) return templates.get(longPaths);
  const template = (async () => {
    const base = await mkdtemp(join(tmpdir(), "zg-mcp-summary-"));
    bases.push(base);
    const root = join(base, "source");
    const documents = longPaths
      ? join(root, "a".repeat(180), "b".repeat(180), "c".repeat(180))
      : root;
    await mkdir(documents, { recursive: true });
    for (let i = 0; i < 25; i++)
      await writeFile(
        join(documents, `document-${i}.md`),
        `# Item ${i}\nlantern orchard ${i}\n`,
      );
    const service = await createZvecGrep({
      root,
      embeddingModel: new CountingEmbeddingModel(),
    });
    let info;
    try {
      await service.index();
      info = await service.info();
    } finally {
      await service.close();
    }
    const legacy = join(base, "legacy"),
      artifact = join(base, "artifact");
    await buildLegacyHome(root, legacy, info.workspaceIndex.id);
    await exportWorkspaceIndex({
      sourceHome: join(root, ".zvec-grep"),
      artifactPath: artifact,
    });
    return { base, legacy, artifact, id: info.workspaceIndex.id };
  })();
  templates.set(longPaths, template);
  return template;
}
for (const operation of ["migrate", "import"])
  for (const longPaths of [false, true])
    for (const full of [false, true]) {
      test(`F6 MCP ${operation} ${full ? "explicit full list" : "bounded default list"} reports count and truncation in both outputs${longPaths ? " with long paths" : ""}`, async (t) => {
        const s = await source(longPaths);
        const destinationRoot = join(s.base, `${operation}-${full}`);
        await mkdir(destinationRoot);
        const backend = new DaemonBackend({ version: "test" });
        const server = createZvecGrepMcpServer(backend, "test", {
          toolset: "full",
        });
        const client = new Client({ name: "summary-control", version: "1" });
        t.after(async () => {
          try {
            await client.close();
            await server.close();
          } finally {
            await backend.close();
          }
        });
        const [left, right] = InMemoryTransport.createLinkedPair();
        await Promise.all([client.connect(left), server.connect(right)]);
        const input = {
          confirm: true,
          destinationRoot,
          ...(operation === "migrate"
            ? { sourceHome: s.legacy }
            : { artifactPath: s.artifact }),
          ...(full ? { includeAllMissingFiles: true } : {}),
        };
        const reply = await client.callTool({
          name: `zvec_grep_index_${operation}`,
          arguments: input,
        });
        assert.notEqual(reply.isError, true, JSON.stringify(reply));
        const result = reply.structuredContent.result;
        assert.equal(result.missingFilesCount, 25);
        assert.equal(result.missingFilesTruncated, !full);
        if (longPaths && !full) {
          assert.ok(result.missingFiles.length > 0);
          assert.ok(result.missingFiles.length < 20);
        } else {
          assert.equal(result.missingFiles.length, full ? 25 : 20);
        }
        const characters = result.missingFiles.reduce(
          (sum, path) => sum + path.length,
          0,
        );
        if (!full) assert.ok(characters <= 4096);
        else if (longPaths) assert.ok(characters > 4096);
        assert.equal(
          new Set(result.missingFiles).size,
          result.missingFiles.length,
        );
        assert.equal(result.indexId, s.id);
        assert.equal(result.verification.vectorsPreserved, true);
        const text = reply.content.find((x) => x.type === "text").text;
        assert.deepEqual(JSON.parse(text), reply.structuredContent);
        if (!full)
          assert.ok(text.length < 6000, `response too large: ${text.length}`);
      });
    }
