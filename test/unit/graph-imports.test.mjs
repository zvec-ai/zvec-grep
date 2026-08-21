import assert from "node:assert/strict";
import test from "node:test";
import { CodeExtractor } from "../../dist/engine/extraction/code/extractor.js";
import {
  FilePathIndex,
  SqliteGraphStorage,
  collectImportSpecs,
  extractFileGraph,
  isExternalImportSpec,
  rawRef,
  resolveImportPath,
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

test("expandFileNeighbors applies limit independently to every seed", async () => {
  const first = codeFile("a-seed", "src/a.ts");
  const second = codeFile("z-seed", "src/z.ts");
  const targets = Array.from({ length: 4 }, (_, index) =>
    codeFile(`target-${index}`, `src/t${index}.ts`),
  );
  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(
    first.id,
    [],
    [],
    targets.slice(0, 3).map((_, index) =>
      rawRef({
        type: "import",
        owner: first.id,
        refName: `./t${index}`,
        line: index + 1,
      }),
    ),
  );
  graph.upsertFileGraph(
    second.id,
    [],
    [],
    [
      rawRef({
        type: "import",
        owner: second.id,
        refName: "./t3",
        line: 1,
      }),
    ],
  );
  for (const target of targets) graph.upsertFileGraph(target.id, [], [], []);
  await graph.resolvePending({ files: [first, second, ...targets] });

  const neighbors = graph.expandFileNeighbors([first.id, second.id], 1);
  assert.deepEqual(
    neighbors.map((item) => item.fid),
    [first.id, second.id],
  );
  assert.equal(neighbors.length, 2);
  graph.close();
});

test("isExternalImportSpec drops npm / node / stdlib", () => {
  assert.equal(isExternalImportSpec("lodash", "typescript"), true);
  assert.equal(isExternalImportSpec("node:fs", "javascript"), true);
  assert.equal(isExternalImportSpec("./utils", "typescript"), false);
  assert.equal(isExternalImportSpec("os", "python"), true);
  assert.equal(isExternalImportSpec(".utils", "python"), false);
  assert.equal(isExternalImportSpec("stdio.h", "c"), true);
});

test("resolveImportPath resolves JS/TS relative + extension table", () => {
  const files = [
    codeFile("a", "src/a.ts"),
    codeFile("b", "src/utils.ts"),
    codeFile("c", "src/lib/index.ts"),
  ];
  const index = new FilePathIndex(files);

  assert.deepEqual(resolveImportPath("./utils", "a", "typescript", index), {
    status: "resolved",
    fileId: "b",
    absolutePath: "/repo/src/utils.ts",
  });
  assert.deepEqual(resolveImportPath("./lib", "a", "typescript", index), {
    status: "resolved",
    fileId: "c",
    absolutePath: "/repo/src/lib/index.ts",
  });
  assert.equal(
    resolveImportPath("lodash", "a", "typescript", index).status,
    "external",
  );
  assert.equal(
    resolveImportPath("./missing", "a", "typescript", index).status,
    "failed",
  );
});

test("resolveImportPath resolves python dotted-relative", () => {
  const files = [
    codeFile("pkg", "pkg/mod.py", "python"),
    codeFile("util", "pkg/util.py", "python"),
    codeFile("sib", "sib.py", "python"),
  ];
  const index = new FilePathIndex(files);
  assert.equal(
    resolveImportPath(".util", "pkg", "python", index).fileId,
    "util",
  );
  assert.equal(
    resolveImportPath("..sib", "pkg", "python", index).fileId,
    "sib",
  );
});

test("collectImportSpecs extracts relative JS imports and drops lodash", async () => {
  const file = codeFile("f1", "src/app.ts");
  const text = `
import { formatDate } from "./utils";
import map from "lodash";
export { helper } from "./helper";
`;
  const specs = await collectImportSpecs({ kind: "text", text, file });
  assert.deepEqual(specs.map((s) => s.spec).sort(), ["./helper", "./utils"]);
});

test("collectImportSpecs reads commented Python bindings from the AST", async () => {
  const file = codeFile("python-comments", "pkg/app.py", "python");
  const text = `from .codec import (
  foo,
  # a comma here, must not become a binding
  bar as baz,
)
`;
  const specs = await collectImportSpecs({ kind: "text", text, file });
  assert.deepEqual(specs, [
    {
      spec: ".codec",
      line: 1,
      bindings: [
        { imported: "foo", local: "foo" },
        { imported: "bar", local: "baz" },
      ],
    },
  ]);
});

test("collectImportSpecs ignores commas inside JS comments", async () => {
  const file = codeFile("js-comments", "src/app.ts");
  const text = `import {
  foo,
  /* misleading, comma */
  bar as baz,
} from "./codec";
`;
  const specs = await collectImportSpecs({ kind: "text", text, file });
  assert.deepEqual(specs, [
    {
      spec: "./codec",
      line: 1,
      bindings: [
        { imported: "foo", local: "foo" },
        { imported: "bar", local: "baz" },
      ],
    },
  ]);
});

test("extractFileGraph + resolvePending builds IMPORTS edges", async () => {
  const a = codeFile("file-a", "src/a.ts");
  const b = codeFile("file-b", "src/utils.ts");
  const textA = `
import { formatDate } from "./utils";
export function run() {
  return formatDate();
}
`;
  const textB = `
export function formatDate() {
  return "ok";
}
`;

  const fragmentsA = await new CodeExtractor().extract({
    kind: "text",
    text: textA,
    file: a,
  });
  const fragmentsB = await new CodeExtractor().extract({
    kind: "text",
    text: textB,
    file: b,
  });
  const graphA = await extractFileGraph(
    { kind: "text", text: textA, file: a },
    fragmentsA,
  );
  const graphB = await extractFileGraph(
    { kind: "text", text: textB, file: b },
    fragmentsB,
  );

  assert.ok(
    graphA.refs.some(
      (r) =>
        r.ref_kind === "import" &&
        r.ref_name === "./utils" &&
        (r.type === "import" || r.type === "import_binding") &&
        r.owner === a.id,
    ),
  );

  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(a.id, graphA.nodes, graphA.edges, graphA.refs);
  graph.upsertFileGraph(b.id, graphB.nodes, graphB.edges, graphB.refs);
  await graph.resolvePending({ files: [a, b] });

  const neighbors = graph.expandFileNeighbors([a.id], 10);
  assert.deepEqual(
    neighbors.map((n) => n.id),
    [b.id],
  );
  assert.deepEqual(graph.fileScope(a.id, 1, 10), [b.id]);

  // Imported file disambiguates call target when multiple formatDate exist.
  const c = codeFile("file-c", "src/other.ts");
  graph.upsertFileGraph(
    c.id,
    [
      {
        id: "sym-other",
        kind: "function",
        is_exported: true,
        name: "formatDate",
      },
    ],
    [],
    [],
  );
  // Re-upsert A so pending call to formatDate can resolve with import preference.
  graph.upsertFileGraph(a.id, graphA.nodes, graphA.edges, graphA.refs);
  await graph.resolvePending({ files: [a, b, c] });

  const run = graphA.nodes.find((n) => n.name === "run");
  const fmt = graphB.nodes.find((n) => n.name === "formatDate");
  assert.ok(run && fmt);
  assert.deepEqual(
    graph.callees(run.id, 1, 10).map((s) => s.id),
    [fmt.id],
  );
  graph.close();
});

test("deleted and re-added import targets are reprojected from source facts", async () => {
  const caller = codeFile("caller", "src/caller.ts");
  const target = codeFile("target", "src/target.ts");
  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(
    caller.id,
    [],
    [],
    [
      rawRef({
        type: "import",
        owner: caller.id,
        refName: "./target",
        line: 1,
      }),
    ],
  );
  graph.upsertFileGraph(target.id, [], [], []);
  await graph.resolvePending({ files: [caller, target] });
  assert.deepEqual(graph.expandFileNeighbors([caller.id], 10), [
    { fid: caller.id, id: target.id, direction: "out" },
  ]);

  graph.deleteFileGraph(target.id);
  assert.deepEqual(graph.expandFileNeighbors([caller.id], 10), []);
  assert.equal(graph.stats().refCount, 1);

  graph.upsertFileGraph(target.id, [], [], []);
  await graph.resolvePending({ files: [caller, target] });
  assert.deepEqual(graph.expandFileNeighbors([caller.id], 10), [
    { fid: caller.id, id: target.id, direction: "out" },
  ]);
  graph.close();
});
