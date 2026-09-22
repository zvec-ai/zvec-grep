// Run after the root npm build to capture the Node.js public toolset metadata.
import { writeFile } from "node:fs/promises";
import { Client, InMemoryTransport } from "@modelcontextprotocol/client";
import { createZvecGrepMcpServer } from "../../dist/mcp/tools.js";

const fixtures = {};
for (const toolset of ["agent", "full"]) {
  const server = createZvecGrepMcpServer({}, "fixture", { toolset });
  const client = new Client({ name: "toolset-fixture", version: "1" });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await Promise.all([
    client.connect(clientTransport),
    server.connect(serverTransport),
  ]);
  const { tools } = await client.listTools();
  fixtures[toolset] = {
    instructions: client.getInstructions(),
    search_description: tools.find((tool) => tool.name === "zvec_grep_search")
      .description,
  };
  await client.close();
  await server.close();
}
await writeFile(
  new URL("../compat/mcp/toolsets.json", import.meta.url),
  `${JSON.stringify(fixtures, null, 2)}\n`,
);
