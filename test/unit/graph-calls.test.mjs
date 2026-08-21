import assert from "node:assert/strict";
import test from "node:test";
import { CodeExtractor } from "../../dist/engine/extraction/code/extractor.js";
import {
  SqliteGraphStorage,
  extractFileGraph,
} from "../../dist/engine/graph/index.js";

function codeFile(relativePath = "mod.ts") {
  return {
    id: "file-1",
    collectionId: "collection-1",
    absolutePath: `/repo/${relativePath}`,
    relativePath,
    rootPath: "/repo",
    sizeBytes: 100,
    lastModifiedTime: 1,
    kind: "code",
    format: "typescript",
  };
}

test("extractFileGraph builds local CALLS and pending cross-file refs", async () => {
  const file = codeFile("local.ts");
  const text = `
export function helper() {
  return 1;
}

export function run() {
  helper();
  helper();
  missingElsewhere();
  console.log("x");
}
`;
  const source = { kind: "text", text, file };
  const fragments = await new CodeExtractor().extract(source);
  const graphInput = await extractFileGraph(source, fragments);

  assert.ok(graphInput.nodes.some((n) => n.name === "run"));
  assert.ok(graphInput.nodes.some((n) => n.name === "helper"));

  const localCalls = graphInput.edges.filter((e) => e.kind === "CALLS");
  assert.equal(localCalls.length, 2);
  assert.equal(new Set(localCalls.map((edge) => edge.id)).size, 2);
  assert.deepEqual(
    localCalls.map((edge) => edge.count),
    [1, 1],
  );
  assert.equal(localCalls[0].rel, "call");

  assert.ok(
    graphInput.refs.some((r) => r.ref_name === "missingElsewhere"),
    "cross-file call should be pending",
  );
  assert.equal(
    graphInput.refs.some(
      (r) => r.ref_name === "console" || r.ref_name === "console.log",
    ),
    false,
    "builtin console should be dropped early",
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
  const helper = graphInput.nodes.find((n) => n.name === "helper");
  assert.ok(run && helper);
  assert.deepEqual(
    graph.callees(run.id, 1, 10).map((s) => s.id),
    [helper.id],
  );
  assert.equal(graph.callees(run.id, 1, 10)[0].count, 2);

  graph.upsertFileGraph(
    "other-file",
    [
      {
        id: "other-helper",
        kind: "function",
        is_exported: true,
        name: "helper",
      },
    ],
    [],
    [],
  );
  await graph.resolvePending();
  assert.equal(
    graph.callees(run.id, 1, 10).find((item) => item.id === helper.id)?.count,
    2,
    "reprojection must preserve every local call occurrence",
  );
  graph.close();
});

test("language builtins do not hide a locally defined symbol", async () => {
  const file = codeFile("local-builtin-name.ts");
  const source = {
    kind: "text",
    file,
    text: `
export function map() { return 1; }
export function run() { return map(); }
`,
  };
  const fragments = await new CodeExtractor().extract(source);
  const graphInput = await extractFileGraph(source, fragments);
  const map = graphInput.nodes.find((node) => node.name === "map");
  const run = graphInput.nodes.find((node) => node.name === "run");
  assert.ok(map && run);
  assert.ok(
    graphInput.edges.some(
      (edge) =>
        edge.kind === "CALLS" && edge.src === run.id && edge.dst === map.id,
    ),
  );
});

test("qualified builtin calls do not fall back to a local bare name", async () => {
  const file = codeFile("qualified-builtin.ts");
  const source = {
    kind: "text",
    file,
    text: `
export function log() { return 1; }
export function run() { console.log("external"); }
`,
  };
  const fragments = await new CodeExtractor().extract(source);
  const graphInput = await extractFileGraph(source, fragments);
  const log = graphInput.nodes.find((node) => node.name === "log");
  const run = graphInput.nodes.find((node) => node.name === "run");
  assert.ok(log && run);
  assert.equal(
    graphInput.edges.some(
      (edge) =>
        edge.kind === "CALLS" && edge.src === run.id && edge.dst === log.id,
    ),
    false,
  );
  assert.equal(
    graphInput.refs.some((ref) => ref.ref_name === "console.log"),
    false,
  );
});

for (const fixture of [
  {
    name: "TypeScript this receiver",
    path: "this-call.ts",
    format: "typescript",
    text: `class Demo {
  helper() { return 1; }
  run() { return this.helper(); }
}`,
  },
  {
    name: "Python self receiver",
    path: "self_call.py",
    format: "python",
    text: `class Demo:
    def helper(self):
        return 1
    def run(self):
        return self.helper()
`,
  },
]) {
  test(`${fixture.name} resolves to the local method`, async () => {
    const file = { ...codeFile(fixture.path), format: fixture.format };
    const source = { kind: "text", file, text: fixture.text };
    const fragments = await new CodeExtractor().extract(source);
    const graphInput = await extractFileGraph(source, fragments);
    const helper = graphInput.nodes.find((node) => node.name === "helper");
    const run = graphInput.nodes.find((node) => node.name === "run");
    assert.ok(helper && run);
    assert.ok(
      graphInput.edges.some(
        (edge) =>
          edge.kind === "CALLS" &&
          edge.src === run.id &&
          edge.dst === helper.id,
      ),
    );
  });
}

test("language-aware pending refs resolve cross-file builtin names", async () => {
  const graph = new SqliteGraphStorage("", { inMemory: true });
  const callerFile = { ...codeFile("caller.ts"), id: "caller-file" };
  const targetFile = { ...codeFile("target.ts"), id: "target-file" };
  const callerSource = {
    kind: "text",
    file: callerFile,
    text: `export function run() { return map(); }`,
  };
  const targetSource = {
    kind: "text",
    file: targetFile,
    text: `export function map() { return 1; }`,
  };
  const callerInput = await extractFileGraph(
    callerSource,
    await new CodeExtractor().extract(callerSource),
  );
  const targetInput = await extractFileGraph(
    targetSource,
    await new CodeExtractor().extract(targetSource),
  );
  graph.upsertFileGraph(
    callerFile.id,
    callerInput.nodes,
    callerInput.edges,
    callerInput.refs,
  );
  graph.upsertFileGraph(
    targetFile.id,
    targetInput.nodes,
    targetInput.edges,
    targetInput.refs,
  );
  await graph.resolvePending();

  const run = callerInput.nodes.find((node) => node.name === "run");
  const map = targetInput.nodes.find((node) => node.name === "map");
  assert.ok(run && map);
  assert.deepEqual(
    graph.callees(run.id, 1, 10).map((item) => item.id),
    [map.id],
  );
  graph.close();
});

test("extractFileGraph resolves cross-file calls after second file indexed", async () => {
  const graph = new SqliteGraphStorage("", { inMemory: true });

  const aFile = {
    ...codeFile("a.ts"),
    id: "file-a",
    absolutePath: "/repo/a.ts",
    relativePath: "a.ts",
  };
  const bFile = {
    ...codeFile("b.ts"),
    id: "file-b",
    absolutePath: "/repo/b.ts",
    relativePath: "b.ts",
  };

  const aSource = {
    kind: "text",
    file: aFile,
    text: `export function caller() { target(); target(); }\n`,
  };
  const bSource = {
    kind: "text",
    file: bFile,
    text: `export function target() { return 1; }\n`,
  };

  const aFrags = await new CodeExtractor().extract(aSource);
  const bFrags = await new CodeExtractor().extract(bSource);
  const aGraph = await extractFileGraph(aSource, aFrags);
  const bGraph = await extractFileGraph(bSource, bFrags);

  assert.equal(aGraph.refs.filter((r) => r.ref_name === "target").length, 2);

  graph.upsertFileGraph(aFile.id, aGraph.nodes, aGraph.edges, aGraph.refs);
  graph.upsertFileGraph(bFile.id, bGraph.nodes, bGraph.edges, bGraph.refs);
  await graph.resolvePending();

  const caller = aGraph.nodes.find((n) => n.name === "caller");
  const target = bGraph.nodes.find((n) => n.name === "target");
  assert.ok(caller && target);
  assert.deepEqual(
    graph.callees(caller.id, 1, 10).map((s) => s.id),
    [target.id],
  );
  assert.equal(graph.callees(caller.id, 1, 10)[0].count, 2);
  graph.close();
});

for (const fixture of [
  {
    name: "TypeScript import alias",
    format: "typescript",
    callerPath: "caller.ts",
    targetPath: "codec.ts",
    callerText:
      'import { decode as parse } from "./codec";\nexport function run() { parse(); }\n',
    targetText: "export function decode() { return 1; }\n",
  },
  {
    name: "Python import alias",
    format: "python",
    callerPath: "caller.py",
    targetPath: "codec.py",
    callerText: "from .codec import decode as parse\ndef run():\n    parse()\n",
    targetText: "def decode():\n    return 1\n",
  },
  {
    name: "TypeScript namespace import",
    format: "typescript",
    callerPath: "namespace-caller.ts",
    targetPath: "codec.ts",
    callerText:
      'import * as utils from "./codec";\nexport function run() { utils.decode(); }\n',
    targetText: "export function decode() { return 1; }\n",
  },
]) {
  test(`${fixture.name} resolves calls to the exported symbol`, async () => {
    const callerFile = {
      ...codeFile(fixture.callerPath),
      id: `caller-${fixture.format}`,
      format: fixture.format,
      absolutePath: `/repo/${fixture.callerPath}`,
    };
    const targetFile = {
      ...codeFile(fixture.targetPath),
      id: `target-${fixture.format}`,
      format: fixture.format,
      absolutePath: `/repo/${fixture.targetPath}`,
    };
    const callerSource = {
      kind: "text",
      file: callerFile,
      text: fixture.callerText,
    };
    const targetSource = {
      kind: "text",
      file: targetFile,
      text: fixture.targetText,
    };
    const extractor = new CodeExtractor();
    const callerGraph = await extractFileGraph(
      callerSource,
      await extractor.extract(callerSource),
    );
    const targetGraph = await extractFileGraph(
      targetSource,
      await extractor.extract(targetSource),
    );
    const graph = new SqliteGraphStorage("", { inMemory: true });
    graph.upsertFileGraph(
      callerFile.id,
      callerGraph.nodes,
      callerGraph.edges,
      callerGraph.refs,
    );
    graph.upsertFileGraph(
      targetFile.id,
      targetGraph.nodes,
      targetGraph.edges,
      targetGraph.refs,
    );
    await graph.resolvePending({ files: [callerFile, targetFile] });

    const caller = callerGraph.nodes.find((node) => node.name === "run");
    const target = targetGraph.nodes.find((node) => node.name === "decode");
    assert.ok(caller && target);
    assert.deepEqual(
      graph.callees(caller.id, 1, 10).map((item) => item.id),
      [target.id],
    );

    graph.deleteFileGraph(targetFile.id);
    graph.upsertFileGraph(
      targetFile.id,
      targetGraph.nodes,
      targetGraph.edges,
      targetGraph.refs,
    );
    await graph.resolvePending({ files: [callerFile, targetFile] });
    assert.deepEqual(
      graph.expandFileNeighbors([callerFile.id], 10).map((item) => item.id),
      [targetFile.id],
    );
    assert.deepEqual(
      graph.callees(caller.id, 1, 10).map((item) => item.id),
      [target.id],
    );
    graph.close();
  });
}

test("named import receiver calls resolve to the imported member", async () => {
  const callerFile = {
    ...codeFile("caller.ts"),
    id: "named-import-caller",
  };
  const targetFile = {
    ...codeFile("codec.ts"),
    id: "named-import-target",
  };
  const callerSource = {
    kind: "text",
    file: callerFile,
    text: `import { Demo } from "./codec";
export function run() { return Demo.helper(); }`,
  };
  const targetSource = {
    kind: "text",
    file: targetFile,
    text: `export class Demo { static helper() { return 1; } }`,
  };
  const extractor = new CodeExtractor();
  const callerInput = await extractFileGraph(
    callerSource,
    await extractor.extract(callerSource),
  );
  const targetInput = await extractFileGraph(
    targetSource,
    await extractor.extract(targetSource),
  );
  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(
    callerFile.id,
    callerInput.nodes,
    callerInput.edges,
    callerInput.refs,
  );
  graph.upsertFileGraph(
    targetFile.id,
    targetInput.nodes,
    targetInput.edges,
    targetInput.refs,
  );
  await graph.resolvePending({ files: [callerFile, targetFile] });

  const run = callerInput.nodes.find((node) => node.name === "run");
  const demo = targetInput.nodes.find((node) => node.name === "Demo");
  const helper = targetInput.nodes.find((node) => node.name === "helper");
  assert.ok(run && demo && helper);
  assert.deepEqual(
    graph.callees(run.id, 1, 10).map((item) => item.id),
    [helper.id],
  );
  assert.notEqual(helper.id, demo.id);
  graph.close();
});

test("receiver calls use the owning or explicitly named container", async () => {
  const file = codeFile("scoped-receivers.ts");
  const source = {
    kind: "text",
    file,
    text: `class First {
  helper() { return 1; }
  run() { return this.helper(); }
}
class Demo {
  static helper() { return 2; }
}
class Second {
  helper() { return 3; }
  run() { return Demo.helper(); }
}`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const nodesByName = (name) =>
    input.nodes.filter((node) => node.name === name);
  const helpers = nodesByName("helper");
  const runs = nodesByName("run");
  assert.equal(helpers.length, 3);
  assert.equal(runs.length, 2);

  const contains = input.edges.filter((edge) => edge.kind === "CONTAINS");
  const containerFor = (id) =>
    input.nodes.find(
      (node) => contains.find((edge) => edge.dst === id)?.src === node.id,
    )?.name;
  const callTargetFor = (id) =>
    input.edges.find((edge) => edge.kind === "CALLS" && edge.src === id)?.dst;
  const firstRun = runs.find((node) => containerFor(node.id) === "First");
  const secondRun = runs.find((node) => containerFor(node.id) === "Second");
  const firstHelper = helpers.find((node) => containerFor(node.id) === "First");
  const demoHelper = helpers.find((node) => containerFor(node.id) === "Demo");
  assert.ok(firstRun && secondRun && firstHelper && demoHelper);
  assert.equal(callTargetFor(firstRun.id), firstHelper.id);
  assert.equal(callTargetFor(secondRun.id), demoHelper.id);
});

test("owner receivers resolve through the inheritance chain", async () => {
  const file = codeFile("inherited-receivers.ts");
  const source = {
    kind: "text",
    file,
    text: `class Base {
  helper() { return 1; }
}
class ChildWithoutOverride extends Base {
  run() { return this.helper(); }
}
class ChildWithOverride extends Base {
  helper() { return super.helper(); }
  other() { return super.helper(); }
  own() { return this.helper(); }
}`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const contains = input.edges.filter((edge) => edge.kind === "CONTAINS");
  const containerFor = (id) =>
    input.nodes.find(
      (node) => contains.find((edge) => edge.dst === id)?.src === node.id,
    )?.name;
  const findMember = (container, name) =>
    input.nodes.find(
      (node) => node.name === name && containerFor(node.id) === container,
    );
  const baseHelper = findMember("Base", "helper");
  const inheritedRun = findMember("ChildWithoutOverride", "run");
  const overridingHelper = findMember("ChildWithOverride", "helper");
  const other = findMember("ChildWithOverride", "other");
  const own = findMember("ChildWithOverride", "own");
  assert.ok(baseHelper && inheritedRun && overridingHelper && other && own);

  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(file.id, input.nodes, input.edges, input.refs);
  await graph.resolvePending();
  for (const caller of [inheritedRun, overridingHelper, other]) {
    assert.deepEqual(
      graph.callees(caller.id, 1, 10).map((item) => item.id),
      [baseHelper.id],
    );
  }
  assert.deepEqual(
    graph.callees(own.id, 1, 10).map((item) => item.id),
    [overridingHelper.id],
  );
  assert.equal(
    graph
      .callees(overridingHelper.id, 1, 10)
      .some((item) => item.id === overridingHelper.id),
    false,
  );
  graph.close();
});

for (const fixture of [
  {
    name: "Python super()",
    format: "python",
    path: "super_call.py",
    base: "Base",
    child: "Child",
    caller: "helper",
    text: `class Base:
    def helper(self):
        return 1

class Child(Base):
    def helper(self):
        return super().helper()
`,
  },
  {
    name: "Java this with interface default method",
    format: "java",
    path: "InheritedCall.java",
    base: "Base",
    child: "Child",
    caller: "run",
    text: `interface Base {
  default int helper() { return 1; }
}
class Child implements Base {
  int run() { return this.helper(); }
}`,
  },
  {
    name: "JavaScript super",
    format: "javascript",
    path: "super-call.js",
    base: "Base",
    child: "Child",
    caller: "run",
    text: `class Base {
  helper() { return 1; }
}
class Child extends Base {
  run() { return super.helper(); }
}`,
  },
  {
    name: "C++ this pointer",
    format: "cpp",
    path: "inherited_call.cpp",
    base: "Base",
    child: "Child",
    caller: "run",
    text: `class Base {
 public:
  int helper() { return 1; }
};
class Child : public Base {
 public:
  int run() { return this->helper(); }
};`,
  },
]) {
  test(`${fixture.name} preserves receiver and resolves inherited method`, async () => {
    const file = {
      ...codeFile(fixture.path),
      format: fixture.format,
    };
    const source = { kind: "text", file, text: fixture.text };
    const input = await extractFileGraph(
      source,
      await new CodeExtractor().extract(source),
    );
    const contains = input.edges.filter((edge) => edge.kind === "CONTAINS");
    const containerFor = (id) =>
      input.nodes.find(
        (node) => contains.find((edge) => edge.dst === id)?.src === node.id,
      )?.name;
    const member = (container, name) =>
      input.nodes.find(
        (node) => node.name === name && containerFor(node.id) === container,
      );
    const target = member(fixture.base, "helper");
    const caller = member(fixture.child, fixture.caller);
    assert.ok(target && caller);

    const graph = new SqliteGraphStorage("", { inMemory: true });
    graph.upsertFileGraph(file.id, input.nodes, input.edges, input.refs);
    await graph.resolvePending();
    assert.deepEqual(
      graph.callees(caller.id, 1, 10).map((item) => item.id),
      [target.id],
    );
    graph.close();
  });
}

test("CONTAINS uses :: scope breadcrumbs", async () => {
  const file = codeFile("cls.ts");
  const source = {
    kind: "text",
    file,
    text: `
export class Foo {
  bar() {
    return 1;
  }
}
`,
  };
  const fragments = await new CodeExtractor().extract(source);
  const method = fragments.find(
    (f) => f.metadata?.kind === "code" && f.metadata.symbolName === "bar",
  );
  assert.ok(method);
  assert.equal(method.metadata.scope, "Foo");

  const graphInput = await extractFileGraph(source, fragments);
  const contains = graphInput.edges.filter((e) => e.kind === "CONTAINS");
  assert.equal(contains.length, 1);
});

test("Go interface dispatch exposes implementation candidates as a dynamic boundary", async () => {
  const file = { ...codeFile("dispatch.go"), format: "go" };
  const source = {
    kind: "text",
    file,
    text: `package p
type Runner interface { Run() }
type Alpha struct{}
func (Alpha) Run() {}
type Beta struct{}
func (Beta) Run() {}
func invoke[T Runner](value T) { value.Run() }
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const invoke = input.nodes.find((node) => node.name === "invoke");
  assert.ok(invoke);

  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(file.id, input.nodes, input.edges, input.refs);
  await graph.resolvePending();

  const boundaries = graph.dynamicBoundaries([invoke.id], 10);
  assert.equal(boundaries.length, 1);
  assert.equal(boundaries[0].reason, "polymorphic_dispatch");
  assert.deepEqual(boundaries[0].target.hints, {
    receiverType: "T",
    callArity: 0,
    candidateTypes: ["T", "Runner"],
    genericBounds: ["Runner"],
    dispatch: "interface",
  });
  const candidateNames = boundaries[0].candidates
    .map((id) => input.nodes.find((node) => node.id === id)?.name)
    .filter(Boolean);
  assert.deepEqual(candidateNames, ["Run", "Run"]);
  assert.equal(graph.stats().refCount, 0);
  assert.equal(graph.stats().pendingRefCount, 0);
  assert.equal(graph.stats().failedRefCount, 0);
  assert.equal(graph.stats().dynamicBoundaryCount, 1);
  assert.equal(boundaries[0].candidateDetails.length, 2);
  await graph.resolvePending();
  assert.equal(graph.dynamicBoundaries([invoke.id], 10).length, 1);
  graph.close();
});

test("Go concrete receiver type resolves a method call in its container", async () => {
  const file = { ...codeFile("receiver.go"), format: "go" };
  const source = {
    kind: "text",
    file,
    text: `package p
type Worker struct{}
func (worker Worker) helper() {}
func (worker Worker) run() { worker.helper() }
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const run = input.nodes.find((node) => node.name === "run");
  const helper = input.nodes.find((node) => node.name === "helper");
  assert.ok(run && helper);
  assert.ok(
    input.edges.some(
      (edge) =>
        edge.kind === "CALLS" && edge.src === run.id && edge.dst === helper.id,
    ),
  );
});

test("Rust AST arity excludes self and preserves generic parameter grouping", async () => {
  const file = { ...codeFile("arity.rs"), format: "rust" };
  const source = {
    kind: "text",
    file,
    text: `use std::collections::HashMap;
struct Value;
impl Value {
  fn run(&self, values: HashMap<String, i32>) {}
}
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const run = input.nodes.find((node) => node.name === "run");
  assert.ok(run);
  assert.equal(run.arity, 1);
});

test("Python AST arity excludes self and preserves generic parameter grouping", async () => {
  const file = { ...codeFile("arity.py"), format: "python" };
  const source = {
    kind: "text",
    file,
    text: `class Value:
  def run(self, values: dict[str, int]):
    pass
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const run = input.nodes.find((node) => node.name === "run");
  assert.ok(run);
  assert.equal(run.arity, 1);
});

test("Rust trait dispatch owns impl methods and exposes implementations", async () => {
  const file = { ...codeFile("dispatch.rs"), format: "rust" };
  const source = {
    kind: "text",
    file,
    text: `trait Runner { fn run(&self); }
struct Alpha;
impl Runner for Alpha { fn run(&self) {} }
fn invoke(value: &dyn Runner) { value.run(); }
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const invoke = input.nodes.find((node) => node.name === "invoke");
  const runMethods = input.nodes.filter((node) => node.name === "run");
  assert.ok(invoke);
  assert.equal(runMethods.length, 2);
  assert.deepEqual(
    runMethods.map((node) => node.arity),
    [0, 0],
  );
  const alphaContainers = input.nodes.filter(
    (node) => node.name === "Alpha" && node.kind === "class",
  );
  assert.equal(alphaContainers.length, 2);
  const implContainer = alphaContainers.find((node) =>
    node.signature?.startsWith("impl "),
  );
  assert.ok(implContainer);
  const implRun = runMethods.find((node) =>
    input.edges.some(
      (edge) =>
        edge.kind === "CONTAINS" &&
        edge.src === implContainer.id &&
        edge.dst === node.id,
    ),
  );
  assert.ok(
    implRun,
    "the impl block, not the same-name struct, must own its method",
  );

  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(file.id, input.nodes, input.edges, input.refs);
  await graph.resolvePending();
  const boundary = graph.dynamicBoundaries([invoke.id], 10)[0];
  assert.ok(boundary);
  assert.ok(boundary.candidates.includes(implRun.id));
  graph.close();
});

test("this.field receiver uses the owner field type, not a later local", async () => {
  const file = { ...codeFile("OwnerField.ts"), format: "typescript" };
  const source = {
    kind: "text",
    file,
    text: `interface Runner { run(): void; }
interface Other { run(): void; }
declare function makeOther(): Other;
class Use {
  value: Runner;
  invoke() {
    this.value.run();
    { const value: Other = makeOther(); value.run(); }
  }
}`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const invoke = input.nodes.find((node) => node.name === "invoke");
  assert.ok(invoke);
  const calls = input.refs.filter(
    (ref) =>
      ref.type === "symbol" &&
      ref.owner === invoke.id &&
      ref.target.member === "run",
  );
  assert.equal(calls.length, 2);
  assert.equal(calls[0].target.receiver?.name, "this.value");
  assert.equal(calls[0].target.hints?.receiverType, "Runner");
  assert.equal(calls[1].target.hints?.receiverType, "Other");
});

test("Go structural dispatch requires the complete interface method set", async () => {
  const file = { ...codeFile("method-set.go"), format: "go" };
  const source = {
    kind: "text",
    file,
    text: `package p
type Runner interface { Run(); Stop() }
type Alpha struct{}
func (a Alpha) Run() {}
func (a Alpha) Stop() {}
type Unrelated struct{}
func (u Unrelated) Run() {}
func invoke[T Runner](value T) { value.Run() }
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const invoke = input.nodes.find((node) => node.name === "invoke");
  assert.ok(invoke);
  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(file.id, input.nodes, input.edges, input.refs);
  await graph.resolvePending();
  const boundary = graph.dynamicBoundaries([invoke.id], 10)[0];
  assert.ok(boundary);
  const parentByChild = new Map(
    input.edges
      .filter((edge) => edge.kind === "CONTAINS")
      .map((edge) => [edge.dst, edge.src]),
  );
  const nameById = new Map(input.nodes.map((node) => [node.id, node.name]));
  const candidateContainers = boundary.candidates.map((id) =>
    nameById.get(parentByChild.get(id)),
  );
  assert.ok(candidateContainers.includes("Alpha"));
  assert.equal(candidateContainers.includes("Unrelated"), false);
  graph.close();
});

test("Go structural dispatch includes embedded interface method sets", async () => {
  const file = { ...codeFile("embedded-method-set.go"), format: "go" };
  const source = {
    kind: "text",
    file,
    text: `package p
type Base interface { Stop() }
type Runner interface { Base; Run() }
type Alpha struct{}
func (a Alpha) Run() {}
func (a Alpha) Stop() {}
type Unrelated struct{}
func (u Unrelated) Run() {}
func invoke[T Runner](value T) { value.Run() }
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const invoke = input.nodes.find((node) => node.name === "invoke");
  assert.ok(invoke);
  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(file.id, input.nodes, input.edges, input.refs);
  await graph.resolvePending();
  const boundary = graph.dynamicBoundaries([invoke.id], 10)[0];
  assert.ok(boundary);
  const parentByChild = new Map(
    input.edges
      .filter((edge) => edge.kind === "CONTAINS")
      .map((edge) => [edge.dst, edge.src]),
  );
  const nameById = new Map(input.nodes.map((node) => [node.id, node.name]));
  const candidateContainers = boundary.candidates.map((id) =>
    nameById.get(parentByChild.get(id)),
  );
  assert.ok(candidateContainers.includes("Alpha"));
  assert.equal(candidateContainers.includes("Unrelated"), false);
  graph.close();
});

test("Go structural dispatch includes methods promoted from embedded providers", async () => {
  const file = { ...codeFile("promoted-method-set.go"), format: "go" };
  const source = {
    kind: "text",
    file,
    text: `package p
type Runner interface { Run(); Stop() }
type RunBase struct{}
func (RunBase) Run() {}
type Wrapper struct { RunBase }
func (Wrapper) Stop() {}
type Unrelated struct{}
func (Unrelated) Run() {}
func invoke[T Runner](value T) { value.Run() }
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const invoke = input.nodes.find((node) => node.name === "invoke");
  const runBase = input.nodes.find((node) => node.name === "RunBase");
  const promotedRun = input.nodes.find(
    (node) =>
      node.name === "Run" &&
      input.edges.some(
        (edge) =>
          edge.kind === "CONTAINS" &&
          edge.src === runBase?.id &&
          edge.dst === node.id,
      ),
  );
  assert.ok(invoke && runBase && promotedRun);

  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(file.id, input.nodes, input.edges, input.refs);
  await graph.resolvePending();

  const boundary = graph.dynamicBoundaries([invoke.id], 10)[0];
  assert.ok(boundary);
  assert.ok(boundary.candidates.includes(promotedRun.id));
  assert.equal(boundary.candidates.length, 1);
  graph.close();
});

test("Rust wrapped trait objects retain the inner dynamic trait", async () => {
  const file = { ...codeFile("wrapped-dispatch.rs"), format: "rust" };
  const source = {
    kind: "text",
    file,
    text: `trait Runner { fn run(&self); }
struct Alpha;
impl Runner for Alpha { fn run(&self) {} }
fn invoke(value: Box<dyn Runner>) { value.run(); }
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const invoke = input.nodes.find((node) => node.name === "invoke");
  assert.ok(invoke);
  const call = input.refs.find(
    (ref) =>
      ref.type === "symbol" &&
      ref.owner === invoke.id &&
      ref.target.member === "run",
  );
  assert.equal(call?.target.hints?.receiverType, "Runner");
  assert.deepEqual(call?.target.hints?.candidateTypes, ["Runner"]);
  assert.equal(call?.target.hints?.dispatch, "trait");

  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(file.id, input.nodes, input.edges, input.refs);
  await graph.resolvePending();
  const boundary = graph.dynamicBoundaries([invoke.id], 10)[0];
  assert.ok(boundary);
  assert.ok(boundary.candidates.length > 0);
  graph.close();
});

test("Java interface receiver keeps virtual implementations as candidates", async () => {
  const file = { ...codeFile("Dispatch.java"), format: "java" };
  const source = {
    kind: "text",
    file,
    text: `interface Runner { void run(); }
class Alpha implements Runner { public void run() {} }
class Use { void invoke(Runner value) { value.run(); } }
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const invoke = input.nodes.find((node) => node.name === "invoke");
  assert.ok(invoke);
  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(file.id, input.nodes, input.edges, input.refs);
  await graph.resolvePending();

  const boundary = graph.dynamicBoundaries([invoke.id], 10)[0];
  assert.ok(boundary);
  assert.equal(boundary.reason, "polymorphic_dispatch");
  assert.equal(boundary.target.hints?.receiverType, "Runner");
  assert.equal(boundary.target.hints?.dispatch, "virtual");
  assert.equal(boundary.candidates.length, 1);
  graph.close();
});

test("abstract interface targets remain dynamic without concrete implementations", async () => {
  const file = { ...codeFile("AbstractDispatch.java"), format: "java" };
  const source = {
    kind: "text",
    file,
    text: `interface Runner { void run(); }
class Use { void invoke(Runner value) { value.run(); } }
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const invoke = input.nodes.find((node) => node.name === "invoke");
  assert.ok(invoke);
  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(file.id, input.nodes, input.edges, input.refs);
  await graph.resolvePending();

  assert.deepEqual(graph.callees(invoke.id, 1, 10), []);
  const boundary = graph.dynamicBoundaries([invoke.id], 10)[0];
  assert.ok(boundary);
  assert.equal(boundary.reason, "polymorphic_dispatch");
  assert.deepEqual(boundary.candidates, []);
  assert.equal(boundary.candidatesTruncated, false);
  graph.close();
});

test("Java RTA retains methods inherited by instantiated subclasses", async () => {
  const file = { ...codeFile("InheritedRta.java"), format: "java" };
  const source = {
    kind: "text",
    file,
    text: `interface Runner { void run(); }
class Base implements Runner { public void run() {} }
class Child extends Base {}
class Other implements Runner { public void run() {} }
class Use {
  void invoke(Runner value) { value.run(); }
  void create() { new Child(); new Other(); }
}
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const invoke = input.nodes.find((node) => node.name === "invoke");
  const parentByChild = new Map(
    input.edges
      .filter((edge) => edge.kind === "CONTAINS")
      .map((edge) => [edge.dst, edge.src]),
  );
  const nameById = new Map(input.nodes.map((node) => [node.id, node.name]));
  const method = (containerName) =>
    input.nodes.find(
      (node) =>
        node.name === "run" &&
        nameById.get(parentByChild.get(node.id)) === containerName,
    );
  const baseRun = method("Base");
  const otherRun = method("Other");
  assert.ok(invoke && baseRun && otherRun);

  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(file.id, input.nodes, input.edges, input.refs);
  await graph.resolvePending();

  assert.deepEqual(graph.callees(invoke.id, 1, 10), []);
  const boundary = graph.dynamicBoundaries([invoke.id], 10)[0];
  assert.ok(boundary);
  assert.deepEqual(
    new Set(boundary.candidates),
    new Set([baseRun.id, otherRun.id]),
  );
  graph.close();
});

test("Java abstract class methods remain dynamic targets", async () => {
  const file = { ...codeFile("AbstractClass.java"), format: "java" };
  const source = {
    kind: "text",
    file,
    text: `abstract class Runner { abstract void run(); }
class Use { void invoke(Runner value) { value.run(); } }
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const runner = input.nodes.find((node) => node.name === "Runner");
  const invoke = input.nodes.find((node) => node.name === "invoke");
  assert.ok(runner && invoke);
  assert.equal(runner.kind, "abstract_class");

  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(file.id, input.nodes, input.edges, input.refs);
  await graph.resolvePending();

  assert.deepEqual(graph.callees(invoke.id, 1, 10), []);
  const boundary = graph.dynamicBoundaries([invoke.id], 10)[0];
  assert.ok(boundary);
  assert.equal(boundary.reason, "polymorphic_dispatch");
  assert.deepEqual(boundary.candidates, []);
  graph.close();
});

test("RTA narrows virtual candidates to instantiated implementations", async () => {
  const file = { ...codeFile("Rta.java"), format: "java" };
  const source = {
    kind: "text",
    file,
    text: `interface Runner { void run(); }
class Alpha implements Runner { public void run() {} }
class Beta implements Runner { public void run() {} }
class Use {
  void invoke(Runner value) { value.run(); }
  void create() { new Alpha(); }
}
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const invoke = input.nodes.find((node) => node.name === "invoke");
  const create = input.nodes.find((node) => node.name === "create");
  const alphaType = input.nodes.find((node) => node.name === "Alpha");
  const alphaRun = input.nodes.find((node) => {
    if (node.name !== "run") return false;
    const parent = input.edges.find(
      (edge) => edge.kind === "CONTAINS" && edge.dst === node.id,
    )?.src;
    return (
      input.nodes.find((candidate) => candidate.id === parent)?.name === "Alpha"
    );
  });
  assert.ok(invoke && create && alphaType && alphaRun);
  assert.ok(input.edges.some((edge) => edge.kind === "INSTANTIATES"));

  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(file.id, input.nodes, input.edges, input.refs);
  await graph.resolvePending();
  assert.equal(
    graph.edges([create.id, alphaType.id], ["INSTANTIATES"], 10).edges.length,
    1,
  );
  assert.equal(graph.dynamicBoundaries([invoke.id], 10).length, 0);
  assert.deepEqual(
    graph.callees(invoke.id, 1, 10).map((candidate) => candidate.id),
    [alphaRun.id],
  );
  const edge = graph.edges([invoke.id, alphaRun.id], ["CALLS"], 10).edges[0];
  assert.equal(edge?.provenance, "heuristic");
  assert.equal(edge?.evidence, "receiver_type_member");
  graph.close();
});

test("changing the only maker from Alpha to Beta reprojects virtual dispatch", async () => {
  const typesFile = {
    ...codeFile("IncrementalTypes.java"),
    id: "types",
    format: "java",
  };
  const makerFile = { ...codeFile("Maker.java"), id: "maker", format: "java" };
  const prepare = async (file, text) => {
    const source = { kind: "text", file, text };
    return extractFileGraph(source, await new CodeExtractor().extract(source));
  };
  const types = await prepare(
    typesFile,
    `interface Runner { void run(); }
class Alpha implements Runner { public void run() {} }
class Beta implements Runner { public void run() {} }
class Use { void invoke(Runner value) { value.run(); } }`,
  );
  const invoke = types.nodes.find((node) => node.name === "invoke");
  const alphaRun = types.nodes.find((node) => {
    if (node.name !== "run") return false;
    const parent = types.edges.find(
      (edge) => edge.kind === "CONTAINS" && edge.dst === node.id,
    )?.src;
    return (
      types.nodes.find((candidate) => candidate.id === parent)?.name === "Alpha"
    );
  });
  const betaRun = types.nodes.find((node) => {
    if (node.name !== "run") return false;
    const parent = types.edges.find(
      (edge) => edge.kind === "CONTAINS" && edge.dst === node.id,
    )?.src;
    return (
      types.nodes.find((candidate) => candidate.id === parent)?.name === "Beta"
    );
  });
  assert.ok(invoke && alphaRun && betaRun);
  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(typesFile.id, types.nodes, types.edges, types.refs);
  await graph.resolvePending();
  assert.ok(graph.dynamicBoundaries([invoke.id], 10)[0]);

  const maker = await prepare(
    makerFile,
    "class Maker { void make() { new Alpha(); } }",
  );
  graph.upsertFileGraph(makerFile.id, maker.nodes, maker.edges, maker.refs);
  await graph.resolvePending();

  assert.equal(graph.dynamicBoundaries([invoke.id], 10).length, 0);
  assert.deepEqual(
    graph.callees(invoke.id, 1, 10).map((candidate) => candidate.id),
    [alphaRun.id],
  );

  const changedMaker = await prepare(
    makerFile,
    "class Maker { void make() { new Beta(); } }",
  );
  graph.upsertFileGraph(
    makerFile.id,
    changedMaker.nodes,
    changedMaker.edges,
    changedMaker.refs,
  );
  await graph.resolvePending();

  assert.deepEqual(
    graph.callees(invoke.id, 1, 10).map((candidate) => candidate.id),
    [betaRun.id],
  );
  graph.close();
});

test("deleting the only maker removes the stale RTA projection", async () => {
  const typesFile = {
    ...codeFile("DeleteMakerTypes.java"),
    id: "delete-types",
    format: "java",
  };
  const makerFile = {
    ...codeFile("DeleteMaker.java"),
    id: "delete-maker",
    format: "java",
  };
  const prepare = async (file, text) => {
    const source = { kind: "text", file, text };
    return extractFileGraph(source, await new CodeExtractor().extract(source));
  };
  const types = await prepare(
    typesFile,
    `interface Runner { void run(); }
class Alpha implements Runner { public void run() {} }
class Beta implements Runner { public void run() {} }
class Use { void invoke(Runner value) { value.run(); } }`,
  );
  const invoke = types.nodes.find((node) => node.name === "invoke");
  assert.ok(invoke);
  const maker = await prepare(
    makerFile,
    "class Maker { void make() { new Alpha(); } }",
  );
  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(typesFile.id, types.nodes, types.edges, types.refs);
  graph.upsertFileGraph(makerFile.id, maker.nodes, maker.edges, maker.refs);
  await graph.resolvePending();
  assert.equal(graph.dynamicBoundaries([invoke.id], 10).length, 0);

  graph.deleteFileGraph(makerFile.id);
  await graph.resolvePending();

  assert.equal(graph.callees(invoke.id, 1, 10).length, 0);
  assert.equal(
    graph.dynamicBoundaries([invoke.id], 10)[0]?.reason,
    "polymorphic_dispatch",
  );
  graph.close();
});

test("removing one of multiple Alpha makers keeps the stable RTA projection", async () => {
  const typesFile = {
    ...codeFile("MultiMakerTypes.java"),
    id: "multi-types",
    format: "java",
  };
  const makerAFile = {
    ...codeFile("MakerA.java"),
    id: "maker-a",
    format: "java",
  };
  const makerBFile = {
    ...codeFile("MakerB.java"),
    id: "maker-b",
    format: "java",
  };
  const prepare = async (file, text) => {
    const source = { kind: "text", file, text };
    return extractFileGraph(source, await new CodeExtractor().extract(source));
  };
  const types = await prepare(
    typesFile,
    `interface Runner { void run(); }
class Alpha implements Runner { public void run() {} }
class Beta implements Runner { public void run() {} }
class Use { void invoke(Runner value) { value.run(); } }`,
  );
  const invoke = types.nodes.find((node) => node.name === "invoke");
  const alphaRun = types.nodes.find((node) => {
    if (node.name !== "run") return false;
    const parent = types.edges.find(
      (edge) => edge.kind === "CONTAINS" && edge.dst === node.id,
    )?.src;
    return (
      types.nodes.find((candidate) => candidate.id === parent)?.name === "Alpha"
    );
  });
  assert.ok(invoke && alphaRun);
  const makerA = await prepare(
    makerAFile,
    "class MakerA { void make() { new Alpha(); } }",
  );
  const makerB = await prepare(
    makerBFile,
    "class MakerB { void make() { new Alpha(); } }",
  );
  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(typesFile.id, types.nodes, types.edges, types.refs);
  graph.upsertFileGraph(makerAFile.id, makerA.nodes, makerA.edges, makerA.refs);
  graph.upsertFileGraph(makerBFile.id, makerB.nodes, makerB.edges, makerB.refs);
  await graph.resolvePending();

  graph.deleteFileGraph(makerAFile.id);
  await graph.resolvePending();

  assert.equal(graph.dynamicBoundaries([invoke.id], 10).length, 0);
  assert.deepEqual(
    graph.callees(invoke.id, 1, 10).map((candidate) => candidate.id),
    [alphaRun.id],
  );
  graph.close();
});

test("Java RTA never promotes instantiated unrelated same-name methods", async () => {
  const file = { ...codeFile("NominalRta.java"), format: "java" };
  const source = {
    kind: "text",
    file,
    text: `interface Runner { void run(); void stop(); }
class Alpha implements Runner { public void run() {} public void stop() {} }
class Unrelated { public void run() {} }
class Use {
  void invoke(Runner value) { value.run(); }
  void make() { new Unrelated(); }
}
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const invoke = input.nodes.find((node) => node.name === "invoke");
  const unrelatedRun = input.nodes.find((node) => {
    if (node.name !== "run") return false;
    const parent = input.edges.find(
      (edge) => edge.kind === "CONTAINS" && edge.dst === node.id,
    )?.src;
    return (
      input.nodes.find((candidate) => candidate.id === parent)?.name ===
      "Unrelated"
    );
  });
  assert.ok(invoke && unrelatedRun);

  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(file.id, input.nodes, input.edges, input.refs);
  await graph.resolvePending();

  assert.equal(
    graph
      .callees(invoke.id, 1, 10)
      .some((candidate) => candidate.id === unrelatedRun.id),
    false,
  );
  const boundary = graph.dynamicBoundaries([invoke.id], 10)[0];
  assert.ok(boundary);
  assert.equal(boundary.candidates.includes(unrelatedRun.id), false);
  graph.close();
});

test("dynamic candidate selection filters overloads by call arity", async () => {
  const file = { ...codeFile("Overload.java"), format: "java" };
  const source = {
    kind: "text",
    file,
    text: `class Target {
  void run() {}
  void run(int value) {}
}
class Use { void invoke(Target value) { value.run(1); } }
`,
  };
  const input = await extractFileGraph(
    source,
    await new CodeExtractor().extract(source),
  );
  const invoke = input.nodes.find((node) => node.name === "invoke");
  const oneArg = input.nodes.find(
    (node) => node.name === "run" && node.arity === 1,
  );
  assert.ok(invoke && oneArg);
  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(file.id, input.nodes, input.edges, input.refs);
  await graph.resolvePending();
  assert.deepEqual(
    graph.callees(invoke.id, 1, 10).map((candidate) => candidate.id),
    [oneArg.id],
  );
  graph.close();
});

test("resolved dispatch facts are recomputed when a later override is indexed", async () => {
  const workerFile = { ...codeFile("worker.ts"), id: "worker-file" };
  const callerFile = { ...codeFile("caller.ts"), id: "caller-file" };
  const specialFile = { ...codeFile("special.ts"), id: "special-file" };
  const prepare = async (file, text) => {
    const source = { kind: "text", file, text };
    return extractFileGraph(source, await new CodeExtractor().extract(source));
  };
  const worker = await prepare(workerFile, "export class Worker { help() {} }");
  const caller = await prepare(
    callerFile,
    'import { Worker } from "./worker"; export function invoke(value: Worker) { value.help(); }',
  );
  const invoke = caller.nodes.find((node) => node.name === "invoke");
  const workerHelp = worker.nodes.find((node) => node.name === "help");
  assert.ok(invoke && workerHelp);

  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(workerFile.id, worker.nodes, worker.edges, worker.refs);
  graph.upsertFileGraph(callerFile.id, caller.nodes, caller.edges, caller.refs);
  await graph.resolvePending({ files: [workerFile, callerFile] });
  assert.deepEqual(
    graph.callees(invoke.id, 1, 10).map((candidate) => candidate.id),
    [workerHelp.id],
  );

  const special = await prepare(
    specialFile,
    'import { Worker } from "./worker"; export class Special extends Worker { help() {} }',
  );
  graph.upsertFileGraph(
    specialFile.id,
    special.nodes,
    special.edges,
    special.refs,
  );
  await graph.resolvePending({ files: [workerFile, callerFile, specialFile] });

  const boundary = graph.dynamicBoundaries([invoke.id], 10)[0];
  assert.ok(boundary);
  assert.equal(boundary.reason, "polymorphic_dispatch");
  assert.equal(boundary.candidates.length, 2);
  assert.equal(graph.callees(invoke.id, 1, 10).length, 0);
  graph.close();
});

test("target file rebuild preserves structured dispatch facts", async () => {
  const workerFile = { ...codeFile("worker.ts"), id: "worker-file" };
  const otherFile = { ...codeFile("other.ts"), id: "other-file" };
  const callerFile = { ...codeFile("caller.ts"), id: "caller-file" };
  const prepare = async (file, text) => {
    const source = { kind: "text", file, text };
    return extractFileGraph(source, await new CodeExtractor().extract(source));
  };
  const worker = await prepare(workerFile, "export class Worker { help() {} }");
  const other = await prepare(otherFile, "export class Other { help() {} }");
  const caller = await prepare(
    callerFile,
    'import { Worker } from "./worker"; export function invoke(value: Worker) { value.help(); }',
  );
  const invoke = caller.nodes.find((node) => node.name === "invoke");
  const workerHelp = worker.nodes.find((node) => node.name === "help");
  assert.ok(invoke && workerHelp);

  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(workerFile.id, worker.nodes, worker.edges, worker.refs);
  graph.upsertFileGraph(otherFile.id, other.nodes, other.edges, other.refs);
  graph.upsertFileGraph(callerFile.id, caller.nodes, caller.edges, caller.refs);
  await graph.resolvePending({ files: [workerFile, otherFile, callerFile] });

  graph.upsertFileGraph(workerFile.id, worker.nodes, worker.edges, worker.refs);
  await graph.resolvePending({ files: [workerFile, otherFile, callerFile] });

  assert.deepEqual(
    graph.callees(invoke.id, 1, 10).map((candidate) => candidate.id),
    [workerHelp.id],
  );
  const edge = graph.edges([invoke.id, workerHelp.id], ["CALLS"], 10).edges[0];
  assert.equal(edge?.provenance, "heuristic");
  assert.equal(edge?.confidence, 0.75);
  assert.equal(edge?.evidence, "receiver_type_member");
  graph.close();
});
