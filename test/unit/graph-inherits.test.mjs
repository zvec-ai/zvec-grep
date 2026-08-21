import assert from "node:assert/strict";
import test from "node:test";
import { CodeExtractor } from "../../dist/engine/extraction/code/extractor.js";
import {
  SqliteGraphStorage,
  extractFileGraph,
} from "../../dist/engine/graph/index.js";

function codeFile(id, relativePath, format = "typescript") {
  return {
    id,
    collectionId: "collection-1",
    absolutePath: `/repo/${relativePath}`,
    relativePath,
    rootPath: "/repo",
    sizeBytes: 100,
    lastModifiedTime: 1,
    kind: "code",
    format,
  };
}

test("extractFileGraph builds local INHERITS for TS class/interface", async () => {
  const file = codeFile("f1", "types.ts");
  const text = `
export class Base {}
export interface IFace {}
export class Child extends Base implements IFace {}
export interface IChild extends IFace {}
`;
  const source = { kind: "text", text, file };
  const fragments = await new CodeExtractor().extract(source);
  const graphInput = await extractFileGraph(source, fragments);

  const inherits = graphInput.edges.filter((e) => e.kind === "INHERITS");
  assert.equal(inherits.length, 3);
  assert.ok(inherits.some((e) => e.rel === "extends" && e.ref_name === "Base"));
  assert.ok(
    inherits.some((e) => e.rel === "implements" && e.ref_name === "IFace"),
  );
  assert.ok(
    inherits.some(
      (e) =>
        e.rel === "extends" &&
        e.ref_name === "IFace" &&
        e.src !== inherits.find((x) => x.rel === "implements")?.src,
    ),
  );

  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(
    file.id,
    graphInput.nodes,
    graphInput.edges,
    graphInput.refs,
  );
  await graph.resolvePending();

  const child = graphInput.nodes.find((n) => n.name === "Child");
  const base = graphInput.nodes.find((n) => n.name === "Base");
  const iface = graphInput.nodes.find((n) => n.name === "IFace");
  assert.ok(child && base && iface);
  const bases = graph.hierarchy(child.id, "bases", 10).map((s) => s.id);
  assert.ok(bases.includes(base.id));
  assert.ok(bases.includes(iface.id));
  assert.deepEqual(
    graph.hierarchy(base.id, "derived", 10).map((s) => s.id),
    [child.id],
  );
  graph.close();
});

test("extractFileGraph resolves cross-file extends after second file indexed", async () => {
  const graph = new SqliteGraphStorage("", { inMemory: true });
  const a = codeFile("file-a", "child.ts");
  const b = codeFile("file-b", "base.ts");
  const textA = `export class Child extends Base {}`;
  const textB = `export class Base {}`;

  const fragsA = await new CodeExtractor().extract({
    kind: "text",
    text: textA,
    file: a,
  });
  const fragsB = await new CodeExtractor().extract({
    kind: "text",
    text: textB,
    file: b,
  });
  const gA = await extractFileGraph(
    { kind: "text", text: textA, file: a },
    fragsA,
  );
  const gB = await extractFileGraph(
    { kind: "text", text: textB, file: b },
    fragsB,
  );

  assert.ok(
    gA.refs.some((r) => r.ref_kind === "extends" && r.ref_name === "Base"),
  );

  graph.upsertFileGraph(a.id, gA.nodes, gA.edges, gA.refs);
  graph.upsertFileGraph(b.id, gB.nodes, gB.edges, gB.refs);
  await graph.resolvePending();

  const child = gA.nodes.find((n) => n.name === "Child");
  const base = gB.nodes.find((n) => n.name === "Base");
  assert.ok(child && base);
  assert.deepEqual(
    graph.hierarchy(child.id, "bases", 10).map((s) => s.id),
    [base.id],
  );
  graph.close();
});

test("extractFileGraph collects python bases and drops object", async () => {
  const file = codeFile("py", "mod.py", "python");
  const text = `
class Base:
    pass

class Child(Base, object):
    pass
`;
  const source = { kind: "text", text, file };
  const fragments = await new CodeExtractor().extract(source);
  const graphInput = await extractFileGraph(source, fragments);
  const inherits = graphInput.edges.filter((e) => e.kind === "INHERITS");
  assert.equal(inherits.length, 1);
  assert.equal(inherits[0].ref_name, "Base");
  assert.equal(
    graphInput.refs.some((r) => r.ref_name === "object"),
    false,
  );
});

test("extractFileGraph drops JS builtin base Object", async () => {
  const file = codeFile("js", "a.ts");
  const text = `export class A extends Object {}`;
  const source = { kind: "text", text, file };
  const fragments = await new CodeExtractor().extract(source);
  const graphInput = await extractFileGraph(source, fragments);
  assert.equal(graphInput.edges.filter((e) => e.kind === "INHERITS").length, 0);
  assert.equal(
    graphInput.refs.some((r) => r.ref_name === "Object"),
    false,
  );
});
