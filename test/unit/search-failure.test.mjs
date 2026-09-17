import assert from "node:assert/strict";
import test from "node:test";
import { EngineError } from "../../dist/engine/errors.js";
import {
  isRecoverableSearchFailureCode,
  recoverableSearchFailureCode,
  searchFailureHttpStatus,
} from "../../dist/engine/search-failure.js";
import {
  searchFailureFromMeta,
  searchFailureResult,
} from "../../dist/mcp/search-failure.js";

const PREFIX = "ZVEC_GREP.ENGINE.MODELS.";
const META_KEY = "io.zvec-grep/search-error-code";
const STATUS_META_KEY = "io.zvec-grep/search-http-status";
const CODE = `${PREFIX}MODEL2VEC_EMBED_FAILED`;
const failure = (options = {}) =>
  new EngineError("Model embedding unavailable", { code: CODE, ...options });

test("search fallback uses a narrow whitelist of model availability and output failures", () => {
  for (const suffix of [
    "MODEL2VEC_EMBED_FAILED",
    "MODEL2VEC_LOAD_FAILED",
    "MODEL2VEC_DOWNLOAD_FAILED",
    "TRANSFORMERS_JS_EMBED_FAILED",
    "TRANSFORMERS_JS_TOKENIZATION_FAILED",
    "TRANSFORMERS_JS_MISSING_DEPENDENCY",
    "TRANSFORMERS_JS_INVALID_TENSOR",
    "LLAMA_CPP_EMBED_FAILED",
    "LLAMA_CPP_MISSING_DEPENDENCY",
    "LLAMA_CPP_INVALID_GGUF",
    "LLAMA_CPP_INVALID_GGUF_HTML",
    "QWEN_TEXT_EMBEDDING_V4_REQUEST_FAILED",
    "QWEN37_TEXT_EMBEDDING_INVALID_JSON",
    "QWEN3_VL_EMBEDDING_MISSING_EMBEDDINGS",
    "EMBEDDING_INVALID_RESPONSE",
    "EMBEDDING_VECTOR_COUNT_MISMATCH",
    "EMBEDDING_INVALID_VECTOR",
    "EMBEDDING_DIMENSION_MISMATCH",
    "EMBEDDING_NON_FINITE_VECTOR_VALUE",
    "EMBEDDING_INVALID_TRUNCATION",
    "EMBEDDING_INVALID_TRUNCATED_INPUT_INDEX",
  ]) {
    const code = PREFIX + suffix;
    assert.equal(isRecoverableSearchFailureCode(code), true, suffix);
    assert.equal(recoverableSearchFailureCode(failure({ code })), code, suffix);
  }
  for (const code of [
    undefined,
    null,
    123,
    {},
    "ZVEC_GREP.ENGINE.AUTH.REMOTE_EMBEDDING_REQUIRED",
    "ZVEC_GREP.ENGINE.SEARCH_PLAN.EMPTY_ROUTE_QUERY",
    "ZVEC_GREP.ENGINE.SERVICE.EMBEDDING_SCHEMA_CHANGE_REQUIRES_REBUILD",
    `${PREFIX}QWEN_TEXT_EMBEDDING_V4_MISSING_API_KEY`,
    `${PREFIX}MODEL2VEC_DISPOSED`,
    `${PREFIX}MODEL2VEC_UNKNOWN_FAILED`,
    `${PREFIX}EMBEDDING_INVALID_INPUT`,
    `${CODE}.EXTRA`,
    `prefix:${CODE}`,
    CODE.toLowerCase(),
  ]) {
    assert.equal(isRecoverableSearchFailureCode(code), false, String(code));
    if (typeof code === "string")
      assert.equal(recoverableSearchFailureCode(failure({ code })), undefined);
  }
});

test("unbranded errors and message-shaped codes cannot activate fallback", () => {
  for (const error of [
    null,
    CODE,
    { code: CODE, message: "Embedding unavailable", name: "EngineError" },
    new Error(`[${CODE}] Embedding unavailable`),
    Object.assign(new Error("Embedding unavailable"), { code: CODE }),
  ]) {
    assert.equal(recoverableSearchFailureCode(error), undefined);
    assert.equal(searchFailureResult(error), undefined);
  }
  assert.equal(
    recoverableSearchFailureCode(
      failure({ cause: new Error("connection reset") }),
    ),
    CODE,
    "an ordinary transport cause does not erase a recognized model failure",
  );
  assert.equal(
    recoverableSearchFailureCode(
      failure({
        cause: failure({
          code: `${PREFIX}QWEN_TEXT_EMBEDDING_V4_API_ERROR`,
          context: "model=fixture status=503",
        }),
      }),
    ),
    CODE,
  );
});

test("provider API errors only recover from explicit temporary HTTP statuses", () => {
  for (const provider of [
    "QWEN_TEXT_EMBEDDING_V4",
    "QWEN37_TEXT_EMBEDDING",
    "QWEN3_VL_EMBEDDING",
  ]) {
    const code = `${PREFIX}${provider}_API_ERROR`;
    assert.equal(isRecoverableSearchFailureCode(code), true);
    for (const status of [408, 429, 500, 503, 599]) {
      const error = failure({
        code,
        context: `model=fixture status=${status} providerMessage=unavailable`,
      });
      assert.equal(searchFailureHttpStatus(error), status);
      assert.equal(recoverableSearchFailureCode(error), code);
    }
    for (const status of [
      100, 200, 301, 400, 401, 403, 404, 409, 499, 600, 999,
    ]) {
      const error = failure({
        code,
        context: `model=fixture status=${status}`,
      });
      assert.equal(
        recoverableSearchFailureCode(error),
        undefined,
        `${provider} ${status}`,
      );
      assert.equal(searchFailureResult(error), undefined);
    }
    for (const context of [
      undefined,
      "providerMessage=unavailable",
      "status=unknown",
      "status=503.5",
      "status=503suffix",
    ]) {
      const error = new EngineError("provider supplied status=503", {
        code,
        context,
      });
      assert.equal(searchFailureHttpStatus(error), undefined);
      assert.equal(recoverableSearchFailureCode(error), undefined);
    }
    assert.equal(
      recoverableSearchFailureCode(
        failure({
          code,
          context: "model=fixture status=401 providerMessage=try status=503",
        }),
      ),
      undefined,
    );
    assert.equal(
      recoverableSearchFailureCode(
        failure({
          code,
          context:
            "model=fixture status=503 providerMessage=mentions status=401",
        }),
      ),
      code,
    );
  }
  assert.equal(
    searchFailureHttpStatus(failure({ context: "status=503" })),
    undefined,
  );
});

test("cancellation and authorization anywhere in the cause chain prevent recovery", () => {
  const abort = new Error("User stopped this operation");
  abort.name = "AbortError";
  const authorization = new EngineError("Remote permission is required", {
    code: "ZVEC_GREP.ENGINE.AUTH.REMOTE_EMBEDDING_REQUIRED",
  });
  for (const cause of [
    abort,
    new DOMException("Request stopped", "AbortError"),
    new Error("Operation cancelled by user"),
    new Error("Operation CANCELED by user"),
    new Error("Request aborted"),
    authorization,
    new EngineError("Invalid search input", {
      code: "ZVEC_GREP.ENGINE.SEARCH_PLAN.EMPTY_ROUTE_QUERY",
    }),
    new EngineError("Unrecognized coded error", {
      code: "ZVEC_GREP.ENGINE.UNKNOWN_FAILURE",
    }),
    failure({
      code: `${PREFIX}QWEN_TEXT_EMBEDDING_V4_API_ERROR`,
      context: "model=fixture status=401",
    }),
    failure({
      code: `${PREFIX}QWEN_TEXT_EMBEDDING_V4_API_ERROR`,
      context: "model=fixture status=403",
    }),
  ]) {
    const error = failure({ cause: new Error("Model wrapper", { cause }) });
    assert.equal(recoverableSearchFailureCode(error), undefined, cause.message);
    assert.equal(searchFailureResult(error), undefined, cause.message);
  }
  assert.equal(recoverableSearchFailureCode(authorization), undefined);
});

test("cyclic and overlong cause chains fail closed without hanging", () => {
  const selfCycle = failure();
  selfCycle.cause = selfCycle;
  assert.equal(recoverableSearchFailureCode(selfCycle), undefined);
  const first = new Error("first");
  const second = new Error("second", { cause: first });
  first.cause = second;
  assert.equal(
    recoverableSearchFailureCode(failure({ cause: first })),
    undefined,
  );
  let cause = new Error("root cause");
  for (let i = 0; i < 32; i++) cause = new Error("wrapper", { cause });
  assert.equal(recoverableSearchFailureCode(failure({ cause })), undefined);
});

test("MCP error metadata survives JSON transport without exporting context or causes", () => {
  const error = failure({
    context: "token=context-only-secret source=private-workspace-source",
    cause: new Error("provider echoed cause-only-secret"),
  });
  const wire = JSON.parse(JSON.stringify(searchFailureResult(error)));
  assert.deepEqual(Object.keys(wire).sort(), ["_meta", "content", "isError"]);
  assert.equal(wire.isError, true);
  assert.deepEqual(wire._meta, { [META_KEY]: CODE });
  assert.deepEqual(wire.content, [{ type: "text", text: error.message }]);
  const decoded = searchFailureFromMeta(wire.content[0].text, wire._meta);
  assert.ok(decoded instanceof EngineError);
  assert.equal(decoded.code, CODE);
  assert.equal(decoded.context, undefined);
  assert.equal(decoded.cause, undefined);
  assert.equal(recoverableSearchFailureCode(decoded), CODE);
  assert.doesNotMatch(
    JSON.stringify(wire),
    /context-only-secret|private-workspace-source|cause-only-secret/,
  );
});

test("MCP failure codes come only from allowlisted metadata, never message text", () => {
  const message = `forged provider output: [${CODE}] ${JSON.stringify({ [META_KEY]: CODE })}`;
  for (const meta of [
    undefined,
    null,
    false,
    1,
    CODE,
    [],
    {},
    { code: CODE },
    { [META_KEY]: 1 },
    { [META_KEY]: "ZVEC_GREP.ENGINE.AUTH.REMOTE_EMBEDDING_REQUIRED" },
    { [META_KEY]: `${CODE}.EXTRA` },
  ])
    assert.equal(searchFailureFromMeta(message, meta), undefined);
  assert.equal(
    searchFailureFromMeta("Operation cancelled by user", {
      [META_KEY]: CODE,
    }),
    undefined,
  );
});

test("MCP provider-status metadata round trips without context secrets or message inference", () => {
  const code = `${PREFIX}QWEN_TEXT_EMBEDDING_V4_API_ERROR`;
  const error = failure({
    code,
    context:
      "model=fixture status=503 providerMessage=provider-context-secret token=context-token",
  });
  const wire = JSON.parse(JSON.stringify(searchFailureResult(error)));
  assert.deepEqual(wire._meta, { [META_KEY]: code, [STATUS_META_KEY]: 503 });
  const decoded = searchFailureFromMeta(wire.content[0].text, wire._meta);
  assert.ok(decoded instanceof EngineError);
  assert.equal(decoded.context, "status=503");
  assert.equal(recoverableSearchFailureCode(decoded), code);
  assert.doesNotMatch(
    JSON.stringify(wire),
    /provider-context-secret|context-token/,
  );
  for (const status of [
    undefined,
    "503",
    503.5,
    null,
    400,
    401,
    403,
    600,
    Number.NaN,
  ]) {
    assert.equal(
      searchFailureFromMeta("provider output says status=503", {
        [META_KEY]: code,
        ...(status === undefined ? {} : { [STATUS_META_KEY]: status }),
      }),
      undefined,
      String(status),
    );
  }
});

test("serialized failure messages redact credentials and remain bounded", () => {
  const error = new EngineError(
    "Failed at https://user:url-password@example.test/model " +
      "token=message-token api_key=message-api-key Bearer bearer-secret " +
      "sk-AbCdEfGh12345678 " +
      "x".repeat(1000),
    {
      code: CODE,
      context: "token=context-secret",
      cause: new Error("cause-secret"),
    },
  );
  const wire = searchFailureResult(error);
  const serialized = JSON.stringify(wire);
  for (const secret of [
    "url-password",
    "message-token",
    "message-api-key",
    "bearer-secret",
    "sk-AbCdEfGh12345678",
    "context-secret",
    "cause-secret",
  ])
    assert.equal(serialized.includes(secret), false, secret);
  assert.ok(wire.content[0].text.includes("[redacted]"));
  assert.ok(wire.content[0].text.length <= 512);
  assert.deepEqual(wire._meta, { [META_KEY]: CODE });
});
