import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  findSchemaVariant,
  isProductPreparationFailure,
  nativeCandidate,
  schemaAllowsType,
} from "../engines/zg/run.mjs";
import {
  NativeIndexProductError,
  parseNativeStatus,
  snapshotIndex,
} from "../engines/zg/snapshot.mjs";

const readyStatus = (entities) => `Workspace index: ready
Root: /tmp/corpus
Index path: /tmp/corpus/.zvec-grep
Embedding: local/potion-code-16m-v2
Files: scanned=12 indexed=10 pending=0 failed=0
Entities: ${entities}
Indexed source size: 8192 bytes
`;

// Layout emitted by the main CLI since #222, including grouped counts and a
// workspace-relative storage path. Coverage is unchanged / scanned files.
const groupedReadyStatus = `✓ Workspace index is ready
  /tmp/corpus

  Coverage    ████████████████████ 100%  381 / 381 files
  Entities    4,269
  Source size 3,060,923 bytes
  Queue       0 pending · 0 failed

  Embedding   local/potion-code-16m-v2
              256 dimensions · cosine
  FTS         tokenizer=jieba filters=lowercase

  Storage     .zvec-grep/generations/current/storage
  Version     2
  Nested Git  included
`;

test("the packed Rust npm metadata resolves to a native zg binary", () => {
  const root = "/tmp/candidate/node_modules/@zvec/zvec-grep";
  assert.deepEqual(
    nativeCandidate(root, "/tmp/candidate", { bin: { zg: "bin/zg" } }),
    {
      consumer: "/tmp/candidate",
      packageRoot: root,
      cli: join(root, "bin/zg"),
      runtime: "rust-native",
    },
  );
  assert.throws(
    () => nativeCandidate(root, "/tmp/candidate", { bin: { zg: "cli.mjs" } }),
    /native Rust CLI/,
  );
  assert.throws(
    () => nativeCandidate(root, "/tmp/candidate", { bin: { zg: "../zg" } }),
    /escapes/,
  );
});

test("public Rust status output supplies a stable ready-index audit", () => {
  const status = parseNativeStatus(`Workspace index: ready
Root: /tmp/corpus
Index path: /tmp/corpus/.zvec-grep
Nested Git repositories: excluded
Embedding: local/potion-code-16m-v2
FTS: tokenizer=unicode filters=lowercase
Files: scanned=12 indexed=10 pending=0 failed=0
Entities: 42
Indexed source size: 8192 bytes
`);
  assert.deepEqual(status, {
    state: "ready",
    root: "/tmp/corpus",
    index_path: "/tmp/corpus/.zvec-grep",
    embedding: "local/potion-code-16m-v2",
    files_scanned: 12,
    files_indexed: 10,
    files_pending: 0,
    files_failed: 0,
    entities_indexed: 42,
    indexed_source_bytes: 8192,
  });
  assert.throws(
    () =>
      parseNativeStatus(`Workspace index: ready
Root: /tmp/corpus
Index path: /tmp/corpus/.zvec-grep
Embedding: local/potion-code-16m-v2
Files: scanned=12 indexed=10 pending=1 failed=0
Entities: 42
Indexed source size: 8192 bytes
`),
    /pending/,
  );
});

test("parsed zero-entity indexes are product failures, while unknown output remains invalid", () => {
  assert.throws(
    () => parseNativeStatus(readyStatus(0)),
    NativeIndexProductError,
  );
  assert.equal(
    isProductPreparationFailure(
      "snapshot",
      new NativeIndexProductError("native index contains no entities"),
    ),
    true,
  );
  assert.throws(() => parseNativeStatus("unrecognized status output"), {
    name: "AssertionError",
  });
  assert.equal(
    isProductPreparationFailure("snapshot", new Error("unknown status output")),
    false,
  );
});

test("grouped Rust status preserves legacy audit fields for local and remote models", () => {
  for (const embedding of [
    "local/potion-code-16m-v2",
    "qwen/qwen3.7-text-embedding",
  ]) {
    const stdout = groupedReadyStatus.replace(
      "local/potion-code-16m-v2",
      embedding,
    );
    const legacy = `Workspace index: ready
Root: /tmp/corpus
Index path: /tmp/corpus/.zvec-grep/generations/current/storage
Embedding: ${embedding}
Files: scanned=381 indexed=381 pending=0 failed=0
Entities: 4269
Indexed source size: 3060923 bytes
`;
    assert.deepEqual(parseNativeStatus(stdout), parseNativeStatus(legacy));
  }
});

test("grouped status accepts ANSI, CRLF, ASCII coverage and candidate HOME paths", () => {
  const plain = parseNativeStatus(groupedReadyStatus);
  for (const storage of [
    ".zvec-grep/generations/current/storage",
    "~/corpus/.zvec-grep/generations/current/storage",
    "/tmp/corpus/.zvec-grep/generations/current/storage",
  ]) {
    const stdout = groupedReadyStatus
      .replace("  /tmp/corpus", "  ~/corpus")
      .replace(".zvec-grep/generations/current/storage", storage)
      .replace("████████████████████", "####################")
      .replaceAll("\n", "\r\n")
      .replace(
        "✓ Workspace index is ready",
        "\x1b[32m✓ Workspace index is ready\x1b[0m",
      )
      .replace("4,269", "\x1b[1m4,269\x1b[0m");
    assert.deepEqual(parseNativeStatus(stdout, { home: "/tmp" }), plain);
  }
  assert.equal(
    parseNativeStatus(groupedReadyStatus.replace("  /tmp/corpus", "  ~"), {
      home: "/tmp/corpus",
    }).root,
    "/tmp/corpus",
  );
});

test("grouped non-ready headings remain product failures", () => {
  for (const heading of [
    "! Workspace index needs an update",
    "✗ Workspace index failed",
    "○ Workspace indexing is disabled",
    "○ Workspace index is not created",
    "? Workspace index is not configured",
    "? Workspace index status is unknown",
    "! Workspace index requires a rebuild",
  ]) {
    assert.throws(
      () =>
        parseNativeStatus(
          groupedReadyStatus.replace("✓ Workspace index is ready", heading),
        ),
      NativeIndexProductError,
    );
  }
});

test("grouped coverage is indexed-file evidence only for a fully ready scan", () => {
  for (const [from, to, error] of [
    ["0 pending", "1 pending", /pending/],
    ["0 failed", "1 failed", /failed/],
    ["381 / 381", "380 / 381", /coverage/],
    ["100%", "99%", /coverage/],
    [
      "  Version",
      "  Changes     0 added · 1 modified · 0 deleted\n  Version",
      /changed/,
    ],
    [
      "  Version",
      "  Changes     1 added · 0 modified · 0 deleted\n  Version",
      /changed/,
    ],
    [
      "  Version",
      "  Changes     0 added · 0 modified · 20 deleted\n  Version",
      /changed/,
    ],
    ["4,269", "0", /no entities/],
  ]) {
    assert.throws(
      () => parseNativeStatus(groupedReadyStatus.replace(from, to)),
      error,
    );
  }
  assert.throws(
    () =>
      parseNativeStatus(
        groupedReadyStatus.replace("381 / 381", "0 / 0").replace("100%", "0%"),
      ),
    /no indexed files/,
  );
});

test("malformed grouped status is invalid evidence, not a zero retrieval score", () => {
  for (const [from, to] of [
    ["4,269", "4,26"],
    ["4,269", "9,007,199,254,740,992"],
    ["381 / 381", "38,1 / 381"],
    ["  /tmp/corpus", "  relative/corpus"],
    ["  Embedding", "  Missing embedding"],
    ["  Coverage", "  Missing coverage"],
    ["  Entities", "  Missing entities"],
    ["  Source size", "  Missing source size"],
    ["  Queue", "  Missing queue"],
    ["  Storage", "  Missing storage"],
    ["  Version", "  Changes     invalid change counts\n  Version"],
  ]) {
    assert.throws(
      () => parseNativeStatus(groupedReadyStatus.replace(from, to)),
      {
        name: "AssertionError",
      },
    );
  }
});

test("failed readiness checks and malformed status keep original command evidence", async (t) => {
  const output = await mkdtemp(join(tmpdir(), "zg-retrieval-status-"));
  t.after(() => rm(output, { recursive: true, force: true }));
  const cases = [
    {
      name: "nonzero",
      result: {
        stdout: `${readyStatus(2).replace("pending=0", "pending=1")}Failed files:\n  src/broken.py\n`,
        stderr: "index has pending files\n",
        code: 2,
        signal: null,
        timed_out: false,
      },
      error: /status failed/,
    },
    {
      name: "unrecognized",
      result: {
        stdout: "unexpected status format\n",
        stderr: "warning from status\n",
        code: 0,
        signal: null,
        timed_out: false,
      },
      error: /omitted state/,
    },
    {
      name: "empty-index",
      result: {
        stdout: readyStatus(0),
        stderr: "",
        code: 0,
        signal: null,
        timed_out: false,
      },
      error: NativeIndexProductError,
    },
  ];
  for (const entry of cases) {
    const directory = join(output, entry.name);
    const runStatus = async () => {
      if (entry.name === "nonzero")
        throw Object.assign(new Error("status failed"), {
          result: entry.result,
        });
      return entry.result;
    };
    await assert.rejects(
      snapshotIndex({
        cli: "zg",
        root: "/tmp/corpus",
        output: directory,
        runStatus,
      }),
      entry.error,
    );
    assert.equal(
      await readFile(join(directory, "status.txt"), "utf8"),
      entry.result.stdout,
    );
    assert.equal(
      await readFile(join(directory, "status.stderr.txt"), "utf8"),
      entry.result.stderr,
    );
    assert.deepEqual(
      JSON.parse(await readFile(join(directory, "status-exit.json"), "utf8")),
      {
        code: entry.result.code,
        signal: entry.result.signal,
        timed_out: entry.result.timed_out,
      },
    );
  }
});

test("successful status retains its existing snapshot identity", async (t) => {
  const output = await mkdtemp(join(tmpdir(), "zg-retrieval-ready-"));
  t.after(() => rm(output, { recursive: true, force: true }));
  const stdout = readyStatus(42);
  const summary = await snapshotIndex({
    cli: "zg",
    root: "/tmp/corpus",
    output,
    runStatus: async (cli, args) => {
      assert.equal(cli, "zg");
      assert.deepEqual(args, [
        "--status",
        "/tmp/corpus",
        "--mode",
        "direct",
        "--check-ready",
        "--debug",
      ]);
      return { stdout, stderr: "", code: 0, signal: null, timed_out: false };
    },
  });
  assert.equal(summary.files, 10);
  assert.equal(summary.fragments, 42);
  assert.deepEqual(Object.keys(summary.artifacts).sort(), [
    "status.json",
    "status.txt",
  ]);
  assert.equal(await readFile(join(output, "status.txt"), "utf8"), stdout);
});

test("legacy and grouped snapshots share logical identity and retain raw evidence", async (t) => {
  const output = await mkdtemp(join(tmpdir(), "zg-retrieval-grouped-"));
  t.after(() => rm(output, { recursive: true, force: true }));
  const outputs = [
    `Workspace index: ready
Root: /tmp/corpus
Index path: /tmp/corpus/.zvec-grep/generations/current/storage
Embedding: local/potion-code-16m-v2
Files: scanned=381 indexed=381 pending=0 failed=0
Entities: 4269
Indexed source size: 3060923 bytes
`,
    groupedReadyStatus.replace("  /tmp/corpus", "  ~/corpus"),
  ];
  const snapshots = [];
  for (const [index, stdout] of outputs.entries()) {
    const directory = join(output, String(index));
    snapshots.push(
      await snapshotIndex({
        cli: "zg",
        root: "/tmp/corpus",
        output: directory,
        env: { HOME: "/tmp" },
        runStatus: async (_cli, args, options) => {
          assert.deepEqual(args, [
            "--status",
            "/tmp/corpus",
            "--mode",
            "direct",
            "--check-ready",
            "--debug",
          ]);
          assert.equal(options.env.HOME, "/tmp");
          return {
            stdout,
            stderr: "",
            code: 0,
            signal: null,
            timed_out: false,
          };
        },
      }),
    );
    assert.equal(await readFile(join(directory, "status.txt"), "utf8"), stdout);
  }
  assert.equal(
    snapshots[0].logical_content_sha256,
    snapshots[1].logical_content_sha256,
  );
  assert.equal(snapshots[1].files, 381);
  assert.equal(snapshots[1].fragments, 4269);
});

test("Rust-generated optional and referenced JSON Schema variants are resolved", () => {
  const root = {
    $defs: {
      QueryList: {
        anyOf: [
          { type: "string" },
          { type: "array", items: { type: "string", maxLength: 4000 } },
        ],
      },
    },
    properties: {
      fts: {
        anyOf: [{ $ref: "#/$defs/QueryList" }, { type: "null" }],
      },
    },
  };
  const array = findSchemaVariant(
    root.properties.fts,
    root,
    (entry) => entry.type === "array",
  );
  assert.equal(array.items.maxLength, 4000);
});

test("Rust JSON Schema nullable type arrays retain their concrete type", () => {
  assert.equal(schemaAllowsType({ type: ["string", "null"] }, "string"), true);
  assert.equal(
    schemaAllowsType({ type: ["integer", "null"] }, "integer"),
    true,
  );
  assert.equal(schemaAllowsType({ type: ["string", "null"] }, "array"), false);
});

test("public index-readiness failures are product errors", () => {
  assert.equal(isProductPreparationFailure("index", new Error("failed")), true);
  assert.equal(
    isProductPreparationFailure(
      "snapshot",
      Object.assign(new Error("not ready"), { result: { code: 1 } }),
    ),
    true,
  );
  assert.equal(
    isProductPreparationFailure("snapshot", new Error("bad harness path")),
    false,
  );
  assert.equal(
    isProductPreparationFailure("mcp_contract", new Error("schema mismatch")),
    false,
  );
});
