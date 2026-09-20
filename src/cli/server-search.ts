import { zvecGrepSearchOutputSchema } from "../mcp/schemas.js";
import type { ZvecGrepSearchResult } from "../mcp/tools.js";

export const INCOMPATIBLE_SERVER_SEARCH_MESSAGE =
  "The running zvec-grep server is incompatible with grouped CLI query output. Restart the currently configured daemon after upgrading zvec-grep, then retry.";

export function parseServerSearchResponse(
  value: unknown,
): ZvecGrepSearchResult {
  const parsed = zvecGrepSearchOutputSchema.safeParse(value);
  if (
    !parsed.success ||
    parsed.data.result.groupResults === undefined ||
    parsed.data.result.groupResults.length === 0
  ) {
    throw new Error(INCOMPATIBLE_SERVER_SEARCH_MESSAGE);
  }
  return parsed.data as unknown as ZvecGrepSearchResult;
}
