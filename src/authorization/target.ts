import { createHash } from "node:crypto";
import { realpath } from "node:fs/promises";
import { resolve } from "node:path";
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
  endpoint: string;
}): Promise<RemoteEmbeddingTarget> {
  const workspaceRoots = await canonicalizeWorkspaceRoots(input.roots);
  if (workspaceRoots.length === 0) {
    throw new Error(
      "Remote Embedding authorization requires a workspace root.",
    );
  }
  const endpoint = input.endpoint.trim();
  if (endpoint.length === 0) {
    throw new Error("Remote Embedding authorization requires an endpoint.");
  }
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
