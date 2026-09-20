import assert from "node:assert/strict";
import test from "node:test";
import { toolSatisfiesContract } from "../../dist/client/daemon-client.js";

test("daemon tool contract checks required schema properties", () => {
  const currentSearch = {
    inputSchema: { properties: { root: {}, routes: {} } },
    outputSchema: { properties: { root: {}, result: {} } },
  };
  assert.equal(
    toolSatisfiesContract(currentSearch, {
      inputProperties: ["routes"],
      outputProperties: ["result"],
    }),
    true,
  );
  assert.equal(
    toolSatisfiesContract(
      { inputSchema: { properties: { root: {} } } },
      { inputProperties: ["routes"] },
    ),
    false,
  );
  assert.equal(
    toolSatisfiesContract(currentSearch, {
      outputProperties: ["missing"],
    }),
    false,
  );
  assert.equal(toolSatisfiesContract(undefined, {}), false);
});
