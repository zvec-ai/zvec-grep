import { readFile, realpath, stat } from "node:fs/promises";
import { isAbsolute, join, relative, resolve, sep } from "node:path";
import { z } from "zod";
import type { CollectionEmbeddingSchema } from "../engine/types.js";
import { anonymousHome } from "../engine/service/root.js";
import { readJsonFile, writeJsonFile } from "../engine/utils/json.js";

const DEMO_AUTHORIZATION_FILE = "remote-embedding-authorization.demo.json";
export const REMOTE_EMBEDDING_DEMO_MAX_FILE_BYTES = 16 * 1024;

export const REMOTE_EMBEDDING_DEMO_SCHEMA = {
  provider: "qwen",
  model: "text-embedding-v4",
  dimension: 1024,
  metric: "cosine",
} as const satisfies CollectionEmbeddingSchema;

const remoteEmbeddingDemoGrantSchema = z
  .object({
    version: z.literal(2),
    demo: z.literal(true),
    capability: z.literal("remote_embedding"),
    scope: z.literal("workspace"),
    workspace: z.string(),
    provider: z.literal(REMOTE_EMBEDDING_DEMO_SCHEMA.provider),
    model: z.literal(REMOTE_EMBEDDING_DEMO_SCHEMA.model),
    granted_at: z.string().datetime(),
  })
  .strict();

export type RemoteEmbeddingDemoGrant = z.infer<
  typeof remoteEmbeddingDemoGrantSchema
>;

export type RemoteEmbeddingDemoFile = {
  absolutePath: string;
  relativePath: string;
  bytes: number;
  text: string;
};

export type RemoteEmbeddingDemoFileMetadata = Omit<
  RemoteEmbeddingDemoFile,
  "text"
>;

export function remoteEmbeddingDemoAuthorizationPath(root: string): string {
  return join(anonymousHome(root), DEMO_AUTHORIZATION_FILE);
}

export async function readRemoteEmbeddingDemoGrant(
  canonicalRoot: string,
): Promise<RemoteEmbeddingDemoGrant | null> {
  const value = await readJsonFile<unknown>(
    remoteEmbeddingDemoAuthorizationPath(canonicalRoot),
    null,
  );
  const parsed = remoteEmbeddingDemoGrantSchema.safeParse(value);
  if (!parsed.success || parsed.data.workspace !== canonicalRoot) {
    return null;
  }
  return parsed.data;
}

export async function writeRemoteEmbeddingDemoGrant(
  canonicalRoot: string,
): Promise<RemoteEmbeddingDemoGrant> {
  const grant: RemoteEmbeddingDemoGrant = {
    version: 2,
    demo: true,
    capability: "remote_embedding",
    scope: "workspace",
    workspace: canonicalRoot,
    provider: REMOTE_EMBEDDING_DEMO_SCHEMA.provider,
    model: REMOTE_EMBEDDING_DEMO_SCHEMA.model,
    granted_at: new Date().toISOString(),
  };
  await writeJsonFile(
    remoteEmbeddingDemoAuthorizationPath(canonicalRoot),
    grant,
    { fileMode: 0o600 },
  );
  return grant;
}

export async function readRemoteEmbeddingDemoFile(
  canonicalRoot: string,
  requestedPath: string,
): Promise<RemoteEmbeddingDemoFile> {
  const file = await inspectRemoteEmbeddingDemoFile(
    canonicalRoot,
    requestedPath,
  );
  const data = await readFile(file.absolutePath);
  if (data.byteLength > REMOTE_EMBEDDING_DEMO_MAX_FILE_BYTES) {
    throw new Error(
      `Remote Embedding demo file exceeds ${REMOTE_EMBEDDING_DEMO_MAX_FILE_BYTES} bytes.`,
    );
  }
  if (data.includes(0)) {
    throw new Error("Remote Embedding demo only accepts UTF-8 text files.");
  }
  let text: string;
  try {
    text = new TextDecoder("utf-8", { fatal: true }).decode(data);
  } catch {
    throw new Error("Remote Embedding demo only accepts UTF-8 text files.");
  }
  if (text.trim().length === 0) {
    throw new Error("Remote Embedding demo file must contain non-empty text.");
  }

  return {
    ...file,
    bytes: data.byteLength,
    text,
  };
}

export async function inspectRemoteEmbeddingDemoFile(
  canonicalRoot: string,
  requestedPath: string,
): Promise<RemoteEmbeddingDemoFileMetadata> {
  if (isAbsolute(requestedPath)) {
    throw new Error(
      "Remote Embedding demo filePath must be relative to the Workspace.",
    );
  }

  const candidate = resolve(canonicalRoot, requestedPath);
  const absolutePath = await realpath(candidate);
  const relativePath = relative(canonicalRoot, absolutePath);
  if (!isWorkspaceRelativePath(relativePath)) {
    throw new Error(
      "Remote Embedding demo file must stay inside the Workspace.",
    );
  }
  if (
    relativePath === ".zvec-grep" ||
    relativePath.startsWith(`.zvec-grep${sep}`)
  ) {
    throw new Error(
      "Remote Embedding demo cannot upload files from .zvec-grep.",
    );
  }

  const fileStat = await stat(absolutePath);
  if (!fileStat.isFile()) {
    throw new Error(
      "Remote Embedding demo filePath must identify a regular file.",
    );
  }
  if (fileStat.size > REMOTE_EMBEDDING_DEMO_MAX_FILE_BYTES) {
    throw new Error(
      `Remote Embedding demo file exceeds ${REMOTE_EMBEDDING_DEMO_MAX_FILE_BYTES} bytes.`,
    );
  }

  return {
    absolutePath,
    relativePath,
    bytes: fileStat.size,
  };
}

function isWorkspaceRelativePath(path: string): boolean {
  return (
    path.length > 0 &&
    path !== ".." &&
    !path.startsWith(`..${sep}`) &&
    !isAbsolute(path)
  );
}
