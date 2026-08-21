import assert from "node:assert/strict";
import test from "node:test";
import { CodeExtractor } from "../../dist/engine/extraction/code/extractor.js";
import {
  SqliteGraphStorage,
  extractFileGraph,
} from "../../dist/engine/graph/index.js";

function codeFile(relativePath = "mod.ts", format = "typescript") {
  return {
    id: "file-1",
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

test("extractFileGraph builds local REFS for type annotations", async () => {
  const file = codeFile("types.ts");
  const text = `
export class Helper {}
export type Result = { ok: true };

export function run(x: Helper): Result {
  const z: Helper = x;
  return { ok: true };
}
`;
  const source = { kind: "text", text, file };
  const fragments = await new CodeExtractor().extract(source);
  const graphInput = await extractFileGraph(source, fragments);

  const refs = graphInput.edges.filter((e) => e.kind === "REFS");
  assert.ok(
    refs.some((e) => e.rel === "type" && e.ref_name === "Helper"),
    "param/local type Helper should be REFS",
  );
  assert.ok(
    refs.some((e) => e.rel === "return" && e.ref_name === "Result"),
    "return type Result should be REFS",
  );
  assert.equal(
    refs.some((e) => e.ref_name === "string" || e.ref_name === "true"),
    false,
    "predefined / literal types should be dropped",
  );

  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(
    file.id,
    graphInput.nodes,
    graphInput.edges,
    graphInput.refs,
  );
  await graph.resolvePending();

  const run = graphInput.nodes.find((n) => n.name === "run");
  const helper = graphInput.nodes.find((n) => n.name === "Helper");
  assert.ok(run && helper);
  const usages = graph.usages(helper.id, 20);
  assert.ok(
    usages.some((u) => u.id === run.id && u.rel === "type"),
    "Helper should have REFS usage from run",
  );
  graph.close();
});

test("extractFileGraph collects member refs and skips call callees", async () => {
  const file = codeFile("members.ts");
  const text = `
export function field() { return 1; }
export function callTarget() { return 2; }

export function run(obj: { field: number }) {
  const a = obj.field;
  callTarget();
}
`;
  const source = { kind: "text", text, file };
  const fragments = await new CodeExtractor().extract(source);
  const graphInput = await extractFileGraph(source, fragments);

  const memberRef = graphInput.refs.find(
    (ref) => ref.ref_kind === "member" && ref.ref_name === "obj.field",
  );
  assert.ok(memberRef);
  assert.deepEqual(memberRef.target, {
    raw: "obj.field",
    member: "field",
    receiver: { kind: "qualified", name: "obj" },
  });
  // callTarget should be CALLS, not member REFS
  assert.ok(
    graphInput.edges.some(
      (e) => e.kind === "CALLS" && e.ref_name === "callTarget",
    ),
  );
});

test("qualified call callees do not emit detached bare member refs", async () => {
  const file = codeFile("qualified-call.ts");
  const source = {
    kind: "text",
    file,
    text: `class Base {
  helper() { return 1; }
}
class Child extends Base {
  run() { return this.helper(); }
}
class Override {
  helper() { return 2; }
}`,
  };
  const graphInput = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const run = graphInput.nodes.find((node) => node.name === "run");
  assert.ok(run);
  assert.ok(
    graphInput.edges.some(
      (edge) =>
        edge.src === run.id &&
        edge.kind === "CALLS" &&
        edge.ref_name === "this.helper",
    ),
  );
  assert.equal(
    graphInput.edges.some(
      (edge) =>
        edge.src === run.id &&
        edge.kind === "REFS" &&
        edge.ref_name === "helper",
    ),
    false,
  );
  assert.equal(
    graphInput.refs.some(
      (ref) =>
        ref.owner === run.id &&
        ref.ref_kind === "member" &&
        ref.ref_name === "helper",
    ),
    false,
  );
});

test("extractFileGraph resolves cross-file type REFS", async () => {
  const graph = new SqliteGraphStorage("", { inMemory: true });
  const a = {
    ...codeFile("a.ts"),
    id: "file-a",
  };
  const b = {
    ...codeFile("b.ts"),
    id: "file-b",
  };
  const textA = `export function run(x: Helper) { return x; }`;
  const textB = `export class Helper {}`;

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
    gA.refs.some((r) => r.ref_kind === "type" && r.ref_name === "Helper"),
  );

  graph.upsertFileGraph(a.id, gA.nodes, gA.edges, gA.refs);
  graph.upsertFileGraph(b.id, gB.nodes, gB.edges, gB.refs);
  await graph.resolvePending();

  const run = gA.nodes.find((n) => n.name === "run");
  const helper = gB.nodes.find((n) => n.name === "Helper");
  assert.ok(run && helper);
  assert.ok(graph.usages(helper.id, 10).some((u) => u.id === run.id));
  graph.close();
});
