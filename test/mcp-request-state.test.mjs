import assert from "node:assert/strict";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  createRemoteEmbeddingRequestStateCodec,
  InMemoryRemoteEmbeddingRequestStateReplayGuard,
  loadOrCreateMcpRequestStateKey,
  mcpRequestStateKeyPath,
  matchesRemoteEmbeddingRequestState,
  PersistentRemoteEmbeddingRequestStateReplayGuard,
  remoteEmbeddingRequestState,
} from "../dist/mcp/request-state.js";

const root = "/private/tmp/zvec-grep-request-state";

function context(clientId = "client-a") {
  return {
    mcpReq: { method: "tools/call" },
    http: {
      authInfo: { token: "", clientId, scopes: ["zvec-grep"] },
    },
  };
}

test("MCP request-state key is persistent and private", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-request-state-"));
  t.after(async () => rm(home, { recursive: true, force: true }));

  const first = await loadOrCreateMcpRequestStateKey(home);
  const second = await loadOrCreateMcpRequestStateKey(home);
  assert.deepEqual(first, second);
  assert.equal(first.byteLength, 32);
  if (process.platform !== "win32") {
    assert.equal(
      (await stat(mcpRequestStateKeyPath(home))).mode & 0o777,
      0o600,
    );
  }
  assert.deepEqual(await readFile(mcpRequestStateKeyPath(home)), first);
});

test("MCP request-state rejects tampering and principal changes", async () => {
  const codec = createRemoteEmbeddingRequestStateCodec(
    new Uint8Array(32).fill(7),
  );
  const state = await codec.mint(
    {
      version: 1,
      nonce: "nonce",
      method: "tools/call",
      tool: "zvec_grep_search",
      argumentsFingerprint: "arguments",
      targetFingerprint: "target",
      disclosureFingerprint: "disclosure",
    },
    context(),
  );
  assert.equal((await codec.verify(state, context())).tool, "zvec_grep_search");
  const signatureStart = state.lastIndexOf(".") + 1;
  const replacement = state[signatureStart] === "a" ? "b" : "a";
  const tampered =
    state.slice(0, signatureStart) +
    replacement +
    state.slice(signatureStart + 1);
  await assert.rejects(codec.verify(tampered, context()));
  await assert.rejects(codec.verify(state, context("client-b")));
});

test("MCP request-state key loader repairs insecure existing permissions", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-request-state-mode-"));
  t.after(async () => rm(home, { recursive: true, force: true }));
  await mkdir(home, { recursive: true });
  const path = mcpRequestStateKeyPath(home);
  await writeFile(path, new Uint8Array(32).fill(4), { mode: 0o600 });
  await chmod(path, 0o644);
  await loadOrCreateMcpRequestStateKey(home);
  if (process.platform !== "win32") {
    assert.equal((await stat(path)).mode & 0o777, 0o600);
  }
});

test("MCP request-state continuation can only be consumed once", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-request-state-replay-"));
  t.after(async () => rm(home, { recursive: true, force: true }));
  const guard = new PersistentRemoteEmbeddingRequestStateReplayGuard(home);
  const state = guard.issue({
    version: 1,
    method: "tools/call",
    tool: "zvec_grep_search",
    argumentsFingerprint: "arguments",
    targetFingerprint: "target",
    disclosureFingerprint: "disclosure",
  });
  assert.equal(await guard.consume(state), true);
  assert.equal(await guard.consume(state), false);
  assert.equal(
    await new PersistentRemoteEmbeddingRequestStateReplayGuard(home).consume(
      state,
    ),
    false,
  );
  const inMemory = new InMemoryRemoteEmbeddingRequestStateReplayGuard();
  const inMemoryState = inMemory.issue({
    version: 1,
    method: "tools/call",
    tool: "zvec_grep_search",
    argumentsFingerprint: "arguments",
    targetFingerprint: "target",
    disclosureFingerprint: "disclosure",
  });
  assert.equal(await inMemory.consume(inMemoryState), true);
  assert.equal(await inMemory.consume(inMemoryState), false);
});

test("MCP request-state expires at its configured TTL", async () => {
  const codec = createRemoteEmbeddingRequestStateCodec(
    new Uint8Array(32).fill(9),
    1,
  );
  const state = await codec.mint(
    {
      version: 1,
      nonce: "nonce",
      method: "tools/call",
      tool: "zvec_grep_search",
      argumentsFingerprint: "arguments",
      targetFingerprint: "target",
      disclosureFingerprint: "disclosure",
    },
    context(),
  );
  await new Promise((resolve) => setTimeout(resolve, 2_100));
  await assert.rejects(codec.verify(state, context()), /expired/);
});

test("MCP request-state is bound to tool arguments, target, and disclosure", () => {
  const plan = {
    operation: "query",
    target: {
      workspaceRoots: [root],
      workspaceFingerprint: "workspace",
      provider: "qwen",
      model: "text-embedding-v4",
      endpoint: "https://qwen.test/embeddings",
      targetFingerprint: "target",
    },
    disclosure: { queryText: true, workspaceContent: "none" },
    reason: "query",
    grantPath: `${root}/.zvec-grep/authorization.json`,
  };
  const expected = remoteEmbeddingRequestState(
    "zvec_grep_search",
    { root, query: "one" },
    plan,
  );
  const actual = {
    ...expected,
    nonce: "nonce",
  };
  assert.equal(matchesRemoteEmbeddingRequestState(actual, expected), true);
  assert.equal(
    matchesRemoteEmbeddingRequestState(
      actual,
      remoteEmbeddingRequestState(
        "zvec_grep_search",
        { root, query: "two" },
        plan,
      ),
    ),
    false,
  );
  assert.equal(
    matchesRemoteEmbeddingRequestState(actual, {
      ...expected,
      targetFingerprint: "other-target",
    }),
    false,
  );
  assert.equal(
    matchesRemoteEmbeddingRequestState(actual, {
      ...expected,
      disclosureFingerprint: "other-disclosure",
    }),
    false,
  );
});
