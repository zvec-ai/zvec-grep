import { isAbsolute } from "node:path";
import type { McpServer, ServerContext } from "@modelcontextprotocol/server";
import { z } from "zod";
import { redactErrorText } from "../engine/errors.js";
import { MCP_MAX_PATH_CHARS } from "./schemas.js";
import {
  runPortabilityOperation,
  type PortabilityOperation,
  type PortabilityInput,
} from "./portability-operation.js";

const path = z
  .string()
  .min(1)
  .max(MCP_MAX_PATH_CHARS)
  .refine(
    (value) => isAbsolute(value) && !value.includes("\0"),
    "Path must be absolute and visible to the server.",
  );
const confirm = z
  .literal(true)
  .describe("Set true only after an explicit user request for this operation.");
const includeAllMissingFiles = z
  .boolean()
  .optional()
  .describe(
    "Request the complete missing-file list. The default response contains a bounded sample. Set this only when the full list is needed; it can be large.",
  );
const inputs = {
  migrate: z.strictObject({
    sourceHome: path,
    destinationRoot: path,
    confirm,
    includeAllMissingFiles,
  }),
  export: z.strictObject({ sourceHome: path, artifactPath: path, confirm }),
  import: z.strictObject({
    artifactPath: path,
    destinationRoot: path,
    confirm,
    includeAllMissingFiles,
  }),
};
const verification = z.object({
  countsMatch: z.boolean(),
  identitiesUnique: z.boolean(),
  identitiesDerived: z.boolean(),
  requiredFieldsValid: z.boolean(),
  ownershipValid: z.boolean(),
  inventoriesExact: z.boolean(),
  groupIntegrity: z.boolean(),
  vectorsSampled: z.boolean(),
  vectorsCompared: z.number(),
  vectorsExact: z.boolean(),
  vectorsPreserved: z.boolean(),
});
const results = {
  migrate: z.object({
    destinationHome: z.string(),
    indexId: z.string(),
    filesConverted: z.number(),
    entitiesConverted: z.number(),
    missingFiles: z.array(z.string()),
    missingFilesCount: z.number().int().nonnegative(),
    missingFilesTruncated: z.boolean(),
    droppedPersistedCredential: z.boolean(),
    droppedPersistedDevice: z.boolean(),
    verification,
  }),
  export: z.object({
    artifactPath: z.string(),
    indexId: z.string(),
    filesExported: z.number(),
    entitiesExported: z.number(),
  }),
  import: z.object({
    destinationHome: z.string(),
    indexId: z.string(),
    filesImported: z.number(),
    entitiesImported: z.number(),
    missingFiles: z.array(z.string()),
    missingFilesCount: z.number().int().nonnegative(),
    missingFilesTruncated: z.boolean(),
    verification,
  }),
};
const descriptions = {
  migrate:
    "Convert a legacy index into a new portable workspace index. Preserve workspace identity and stored vectors. The destination must not contain an index.",
  export:
    "Read an index under its engine lock and create a new logical transfer artifact with indexed text and vectors. Credentials and host bindings are excluded. The artifact path must be new.",
  import:
    "Validate a logical transfer artifact and create native storage in a destination workspace. Preserve index identity and stored vectors. The destination must not contain an index.",
};

export function registerPortabilityTools(server: McpServer): void {
  for (const operation of ["migrate", "export", "import"] as const) {
    register(operation);
  }
  function register(operation: PortabilityOperation): void {
    const outputSchema = z.union([
      z.object({
        operation: z.literal(operation),
        state: z.literal("succeeded"),
        result: results[operation],
      }),
      z.object({
        operation: z.literal(operation),
        state: z.literal("failed"),
        error: z.object({
          code: z.string(),
          message: z.string(),
          context: z.string().optional(),
        }),
      }),
    ]);
    server.registerTool(
      `zvec_grep_index_${operation}`,
      {
        title: `${operation[0].toUpperCase()}${operation.slice(1)} a workspace index`,
        description: `${descriptions[operation]} Require an explicit user request before this persistent operation. All paths refer to files visible to the server. Copying files between hosts is a separate operation. No document embeddings are computed. Cancellation before publication removes owned staging; a completed publication is not undone.`,
        inputSchema: inputs[operation],
        outputSchema,
        annotations: {
          readOnlyHint: false,
          destructiveHint: false,
          idempotentHint: false,
          openWorldHint: false,
        },
      },
      async (
        input: PortabilityInput & { includeAllMissingFiles?: boolean },
        ctx: ServerContext,
      ) => {
        let progress = 0;
        try {
          const result = await runPortabilityOperation(operation, input, {
            signal: ctx.mcpReq.signal,
            onProgress: async (stage, detail) => {
              const progressToken = ctx.mcpReq._meta?.progressToken;
              if (progressToken === undefined || ctx.mcpReq.signal.aborted)
                return;
              await ctx.mcpReq
                .notify({
                  method: "notifications/progress",
                  params: {
                    progressToken,
                    progress: ++progress,
                    message: `${stage}: ${detail}`,
                  },
                })
                .catch(() => undefined);
            },
          });
          const structuredContent = {
            operation,
            state: "succeeded" as const,
            result:
              "missingFiles" in result
                ? summarizeMissingFiles(
                    result,
                    input.includeAllMissingFiles === true,
                  )
                : result,
          };
          return {
            content: [
              {
                type: "text" as const,
                text: JSON.stringify(structuredContent),
              },
            ],
            structuredContent,
          };
        } catch (error) {
          const cause =
            error instanceof Error ? error : new Error(String(error));
          const details = {
            code:
              "code" in cause
                ? String(cause.code)
                : "ZVEC_GREP.ENGINE.TRANSFER_FAILED",
            message: redactErrorText(cause.message, 8_192),
            ...("context" in cause
              ? { context: redactErrorText(String(cause.context), 8_192) }
              : {}),
          };
          const structuredContent = {
            operation,
            state: "failed" as const,
            error: details,
          };
          return {
            isError: true,
            content: [
              {
                type: "text" as const,
                text: JSON.stringify(structuredContent),
              },
            ],
            structuredContent,
          };
        }
      },
    );
  }
}

function summarizeMissingFiles<T extends { missingFiles: string[] }>(
  result: T,
  all: boolean,
): T & { missingFilesCount: number; missingFilesTruncated: boolean } {
  const sample: string[] = [];
  let characters = 0;
  for (const file of result.missingFiles) {
    if (!all && (sample.length >= 20 || characters + file.length > 4096)) break;
    sample.push(file);
    characters += file.length;
  }
  return {
    ...result,
    missingFiles: sample,
    missingFilesCount: result.missingFiles.length,
    missingFilesTruncated: sample.length !== result.missingFiles.length,
  };
}
