export const MCP_TOOLSET_ENV = "ZVEC_GREP_MCP_TOOLSET";

export const DEFAULT_MCP_TOOLSET = "agent";

export type McpToolset = "agent" | "full";

export function parseMcpToolset(value: string): McpToolset {
  if (value === "agent" || value === "full") return value;
  throw new Error(
    `Unsupported MCP toolset "${value}". Expected "agent" or "full".`,
  );
}

export function resolveMcpToolset(
  explicitValue?: string,
  environmentValue?: string,
): McpToolset {
  const value = explicitValue ?? environmentValue;
  return value === undefined ? DEFAULT_MCP_TOOLSET : parseMcpToolset(value);
}
