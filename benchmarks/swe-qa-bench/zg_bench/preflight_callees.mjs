import { writeFile } from "node:fs/promises";

const [root, symbol, target, callsite, output] = process.argv.slice(2);
const headers = {
  "Content-Type": "application/json",
  Accept: "application/json, text/event-stream",
};
async function request(method, params, id) {
  const response = await fetch("http://127.0.0.1:7999/mcp", {
    method: "POST",
    headers,
    body: JSON.stringify({ jsonrpc: "2.0", method, params, id }),
    signal: AbortSignal.timeout(60_000),
  });
  if (!response.ok) throw new Error(`MCP ${method}: HTTP ${response.status}`);
  const session = response.headers.get("Mcp-Session-Id");
  if (session) headers["Mcp-Session-Id"] = session;
  const text = await response.text();
  if (id === undefined && !text) return;
  const message = response.headers
    .get("content-type")
    ?.includes("text/event-stream")
    ? text
        .split(/\r?\n/)
        .filter((line) => line.startsWith("data:") && line.slice(5).trim())
        .map((line) => JSON.parse(line.slice(5)))
        .find((message) => message.id === id)
    : JSON.parse(text);
  if (!message || message.error)
    throw new Error(`MCP ${method}: ${JSON.stringify(message)}`);
  return message.result;
}
const initialized = await request(
  "initialize",
  {
    protocolVersion: "2025-03-26",
    capabilities: {},
    clientInfo: { name: "zg-bench-callees-preflight", version: "1" },
  },
  1,
);
headers["MCP-Protocol-Version"] = initialized.protocolVersion;
await request("notifications/initialized", {});
const { tools } = await request("tools/list", {}, 2);
const expected = ["zvec_grep_callees", "zvec_grep_callers", "zvec_grep_search"];
if (
  JSON.stringify(tools.map((t) => t.name).sort()) !== JSON.stringify(expected)
)
  throw new Error("Expected the default search/callers/callees toolset");
const args = { root, symbol };
const result = await request(
  "tools/call",
  { name: "zvec_grep_callees", arguments: args },
  3,
);
await writeFile(
  output,
  JSON.stringify({ initialized, tools, arguments: args, result }, null, 2) +
    "\n",
);
const text = result.content
  .filter((c) => c.type === "text")
  .map((c) => c.text)
  .join("\n");
if (result.isError || !text.includes(target) || !text.includes(callsite))
  throw new Error(`Source-verified outgoing call missing: ${text}`);
