import assert from "node:assert/strict";
import test from "node:test";
import {
  exactMatchPriority,
  searchIntent,
} from "../../dist/engine/pipeline/search/intent.js";

const file = { relativePath: "src/auth.ts" };
const entity = (symbolName, text) => ({
  metadata: { kind: "code", symbolName },
  content: { kind: "text", text },
});

test("intent hints do not depend on unavailable shell quoting", () => {
  assert.equal(searchIntent("AuthService").kind, "identifier");
  assert.equal(searchIntent("connection_pool").kind, "identifier");
  assert.equal(searchIntent("src/auth.ts").kind, "path");
  assert.equal(searchIntent("./src/auth.ts").query, "src/auth.ts");
  assert.equal(searchIntent("AuthService 如何刷新 token").kind, "text");
  assert.equal(searchIntent("No index found").kind, "text");
  assert.equal(searchIntent("src/*.ts").kind, "text");
});

test("exact symbols precede references and substrings are not exact identifiers", () => {
  const intent = searchIntent("AuthService");
  assert.equal(
    exactMatchPriority(
      intent,
      entity("AuthService", "class AuthService {}"),
      file,
    ),
    3,
  );
  assert.equal(
    exactMatchPriority(intent, entity("caller", "new AuthService()"), file),
    2,
  );
  assert.equal(
    exactMatchPriority(
      intent,
      entity("MockAuthService", "class MockAuthService {}"),
      file,
    ),
    0,
  );
  assert.equal(
    exactMatchPriority(
      searchIntent("$auth"),
      entity("caller", "$auth()"),
      file,
    ),
    2,
  );
});

test("paths and phrases get exact evidence without mistaking mixed questions for lookups", () => {
  const candidate = entity("AuthService", 'throw new Error("No index found")');
  assert.equal(exactMatchPriority(searchIntent("auth.ts"), candidate, file), 3);
  assert.equal(
    exactMatchPriority(searchIntent("other/auth.ts"), candidate, file),
    0,
  );
  assert.equal(
    exactMatchPriority(searchIntent("No index found"), candidate, file),
    1,
  );
  assert.equal(
    exactMatchPriority(
      searchIntent("AuthService 如何刷新 token"),
      candidate,
      file,
    ),
    0,
  );
});

test("literal evidence in source fragments survives grouped entity outlines", () => {
  const outline = entity("Handler", "class Handler { retry(); }");
  const source = entity(
    "retry",
    'throw new Error("No index found"); new AuthService();',
  );
  assert.equal(
    exactMatchPriority(searchIntent("AuthService"), outline, file, [source]),
    2,
  );
  assert.equal(
    exactMatchPriority(searchIntent("No index found"), outline, file, [source]),
    1,
  );
  assert.equal(
    exactMatchPriority(searchIntent("index found"), outline, file, [
      entity("a", "index "),
      entity("b", "found"),
    ]),
    0,
  );
  assert.equal(
    exactMatchPriority(searchIntent("AuthService"), outline, file, [
      entity("AuthService", "unrelated"),
    ]),
    0,
  );
});
