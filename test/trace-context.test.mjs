import assert from "node:assert/strict";
import test from "node:test";
import {
  currentTraceContext,
  runWithTraceContext,
  traceContextFromMcpBody,
  traceHeaders,
} from "../dist/observability/trace-context.js";

const traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

test("standard MCP trace context is received and propagated asynchronously", async () => {
  const context = traceContextFromMcpBody({
    params: {
      _meta: {
        traceparent,
        tracestate: "vendor=value",
        baggage: "tenant=example",
      },
    },
  });
  assert.equal(context.traceId, "4bf92f3577b34da6a3ce929d0e0e4736");
  await runWithTraceContext(context, async () => {
    await Promise.resolve();
    assert.deepEqual(traceHeaders(), {
      traceparent,
      tracestate: "vendor=value",
      baggage: "tenant=example",
    });
    assert.equal(currentTraceContext(), context);
  });
  assert.equal(currentTraceContext(), undefined);
});

test("invalid trace context is ignored", () => {
  assert.equal(
    traceContextFromMcpBody({
      params: {
        _meta: {
          traceparent:
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
        },
      },
    }),
    undefined,
  );
  assert.deepEqual(traceHeaders(), {});
  assert.equal(
    traceContextFromMcpBody({
      params: { _meta: { traceparent: traceparent.toUpperCase() } },
    }),
    undefined,
  );
});

test("future traceparent versions are forwarded without reinterpretation", () => {
  const future = "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-ab";
  assert.equal(
    traceContextFromMcpBody({
      params: { _meta: { traceparent: future } },
    }).traceparent,
    future,
  );
});

test("baggage accepts standard optional whitespace", () => {
  for (const baggage of [
    "tenant= example ; region= cn ; sampled",
    "tenant=\texample\t;\tregion=\tcn",
  ]) {
    assert.equal(
      traceContextFromMcpBody({
        params: { _meta: { traceparent, baggage } },
      }).baggage,
      baggage,
    );
  }
});

test("invalid tracestate and oversized baggage member sets are not propagated", () => {
  const context = traceContextFromMcpBody({
    params: {
      _meta: {
        traceparent,
        tracestate: "InvalidKey=value",
        baggage: Array.from({ length: 65 }, (_, index) => `k${index}=v`).join(
          ",",
        ),
      },
    },
  });
  assert.equal(context.tracestate, undefined);
  assert.equal(context.baggage, undefined);
  for (const baggage of ['key=bad"value', "key=bad\\value"]) {
    assert.equal(
      traceContextFromMcpBody({
        params: { _meta: { traceparent, baggage } },
      }).baggage,
      undefined,
    );
  }
});
