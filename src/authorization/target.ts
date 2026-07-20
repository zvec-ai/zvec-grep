import { createHash } from "node:crypto";
import { realpath } from "node:fs/promises";
import { resolve } from "node:path";
import { readGlobalConfig } from "../engine/config.js";
import { resolveRemoteEmbeddingEndpoint } from "../engine/models/providers/qwen/embedding.js";
import type { CreateZvecGrepOptions } from "../engine/service/types.js";
import type { RemoteEmbeddingTarget } from "./types.js";

export async function canonicalizeWorkspaceRoots(
  roots: readonly string[],
): Promise<string[]> {
  const canonical = await Promise.all(
    roots.map(async (root) => {
      const absolute = resolve(root);
      return await realpath(absolute).catch(() => absolute);
    }),
  );
  return [...new Set(canonical)].sort();
}

export function workspaceFingerprint(roots: readonly string[]): string {
  return sha256(JSON.stringify([...roots]));
}

export function remoteEmbeddingTargetFingerprint(input: {
  workspaceFingerprint: string;
  provider: string;
  model: string;
  endpoint: string;
}): string {
  return sha256(
    JSON.stringify([
      input.workspaceFingerprint,
      input.provider,
      input.model,
      input.endpoint,
    ]),
  );
}

export async function createRemoteEmbeddingTarget(input: {
  roots: readonly string[];
  provider: string;
  model: string;
  endpoint?: string;
  serviceOptions?: CreateZvecGrepOptions;
}): Promise<RemoteEmbeddingTarget> {
  const workspaceRoots = await canonicalizeWorkspaceRoots(input.roots);
  if (workspaceRoots.length === 0) {
    throw new Error(
      "Remote Embedding authorization requires a workspace root.",
    );
  }
  const config = readGlobalConfig();
  const configuredEndpoint =
    input.endpoint ??
    input.serviceOptions?.endpoint ??
    config.providers?.[input.provider]?.endpoint;
  const endpoint = resolveRemoteEmbeddingEndpoint(
    { provider: input.provider, model: input.model },
    configuredEndpoint,
  );
  const rootFingerprint = workspaceFingerprint(workspaceRoots);
  return {
    workspaceRoots,
    workspaceFingerprint: rootFingerprint,
    provider: input.provider,
    model: input.model,
    endpoint,
    targetFingerprint: remoteEmbeddingTargetFingerprint({
      workspaceFingerprint: rootFingerprint,
      provider: input.provider,
      model: input.model,
      endpoint,
    }),
  };
}

function sha256(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}
