import { createHash, randomBytes, randomUUID } from "node:crypto";
import {
  chmod,
  mkdir,
  open,
  readFile,
  readdir,
  unlink,
} from "node:fs/promises";
import { dirname, join } from "node:path";
import {
  createRequestStateCodec,
  type AuthInfo,
  type RequestStateCodec,
} from "@modelcontextprotocol/server";
import { defaultHome } from "../engine/utils/path.js";
import type { RemoteEmbeddingAuthorizationPlan } from "../authorization/types.js";

const REQUEST_STATE_KEY_BYTES = 32;
const REQUEST_STATE_TTL_SECONDS = 10 * 60;
const MAX_IN_MEMORY_CONSUMED_STATES = 4_096;

export type RemoteEmbeddingRequestState = {
  version: 1;
  nonce: string;
  method: "tools/call";
  tool: string;
  argumentsFingerprint: string;
  targetFingerprint: string;
  disclosureFingerprint: string;
};

export interface RemoteEmbeddingRequestStateReplayGuard {
  issue(
    state: Omit<RemoteEmbeddingRequestState, "nonce">,
  ): RemoteEmbeddingRequestState;
  consume(state: RemoteEmbeddingRequestState): Promise<boolean>;
}

export class InMemoryRemoteEmbeddingRequestStateReplayGuard implements RemoteEmbeddingRequestStateReplayGuard {
  private readonly consumed = new Map<string, number>();

  issue(
    state: Omit<RemoteEmbeddingRequestState, "nonce">,
  ): RemoteEmbeddingRequestState {
    return issueState(state);
  }

  async consume(state: RemoteEmbeddingRequestState): Promise<boolean> {
    const now = Date.now();
    this.expire(now);
    if (
      this.consumed.has(state.nonce) ||
      this.consumed.size >= MAX_IN_MEMORY_CONSUMED_STATES
    ) {
      return false;
    }
    this.consumed.set(state.nonce, now);
    return true;
  }

  private expire(now: number): void {
    const expiresBefore = now - REQUEST_STATE_TTL_SECONDS * 1_000;
    for (const [nonce, consumedAt] of this.consumed) {
      if (consumedAt >= expiresBefore) continue;
      this.consumed.delete(nonce);
    }
  }
}

export class PersistentRemoteEmbeddingRequestStateReplayGuard implements RemoteEmbeddingRequestStateReplayGuard {
  private readonly directory: string;
  private readonly consumed = new Map<string, number>();
  private loaded = false;
  private saturated = false;
  private operationTail: Promise<void> = Promise.resolve();

  constructor(home?: string) {
    this.directory = join(home ?? defaultHome(), "mcp-request-state-consumed");
  }

  issue(
    state: Omit<RemoteEmbeddingRequestState, "nonce">,
  ): RemoteEmbeddingRequestState {
    return issueState(state);
  }

  async consume(state: RemoteEmbeddingRequestState): Promise<boolean> {
    const operation = this.operationTail.then(() => this.consumeLocked(state));
    this.operationTail = operation.then(
      () => undefined,
      () => undefined,
    );
    return await operation;
  }

  private async consumeLocked(
    state: RemoteEmbeddingRequestState,
  ): Promise<boolean> {
    if (!/^\d{13}-[0-9a-f-]{36}$/.test(state.nonce)) return false;
    await mkdir(this.directory, { recursive: true, mode: 0o700 });
    if (!this.loaded) await this.loadTombstones();
    await this.removeExpiredTombstones();
    if (this.saturated) {
      await this.loadTombstones();
    }
    if (
      this.saturated ||
      this.consumed.has(state.nonce) ||
      this.consumed.size >= MAX_IN_MEMORY_CONSUMED_STATES
    ) {
      return false;
    }

    try {
      const file = await open(join(this.directory, state.nonce), "wx", 0o600);
      await file.close();
    } catch (error) {
      if (isExistingFile(error)) return false;
      // Force reload on next call if filesystem state is uncertain.
      this.loaded = false;
      throw error;
    }
    this.consumed.set(state.nonce, Number(state.nonce.slice(0, 13)));
    return true;
  }

  private async loadTombstones(): Promise<void> {
    this.consumed.clear();
    this.saturated = false;
    const expiresBefore = Date.now() - REQUEST_STATE_TTL_SECONDS * 1_000;
    for (const name of (await readdir(this.directory)).sort()) {
      const consumedAt = Number(name.slice(0, 13));
      if (!Number.isFinite(consumedAt)) continue;
      if (consumedAt < expiresBefore) {
        await unlink(join(this.directory, name)).catch(() => undefined);
        continue;
      }
      if (this.consumed.size >= MAX_IN_MEMORY_CONSUMED_STATES) {
        this.saturated = true;
        break;
      }
      this.consumed.set(name, consumedAt);
    }
    this.loaded = true;
  }

  private async removeExpiredTombstones(): Promise<void> {
    const expiresBefore = Date.now() - REQUEST_STATE_TTL_SECONDS * 1_000;
    for (const [name, consumedAt] of this.consumed) {
      if (consumedAt >= expiresBefore) break;
      this.consumed.delete(name);
      await unlink(join(this.directory, name)).catch(() => undefined);
    }
  }
}

export function mcpRequestStateKeyPath(home?: string): string {
  return join(home ?? defaultHome(), "mcp-request-state.key");
}

export async function loadOrCreateMcpRequestStateKey(
  home?: string,
): Promise<Uint8Array> {
  const path = mcpRequestStateKeyPath(home);
  try {
    const existing = await readFile(path);
    await chmod(path, 0o600);
    return validateKey(existing);
  } catch (error) {
    if (!isMissingFile(error)) throw error;
  }

  await mkdir(dirname(path), { recursive: true, mode: 0o700 });
  const candidate = randomBytes(REQUEST_STATE_KEY_BYTES);
  try {
    const file = await open(path, "wx", 0o600);
    try {
      await file.writeFile(candidate);
    } finally {
      await file.close();
    }
  } catch (error) {
    if (!isExistingFile(error)) throw error;
  }
  await chmod(path, 0o600);
  return validateKey(await readFile(path));
}

export function createRemoteEmbeddingRequestStateCodec(
  key: Uint8Array,
  ttlSeconds = REQUEST_STATE_TTL_SECONDS,
): RequestStateCodec<RemoteEmbeddingRequestState> {
  return createRequestStateCodec<RemoteEmbeddingRequestState>({
    key,
    ttlSeconds,
    bind: (ctx) =>
      `${ctx.mcpReq.method}\0${ctx.http?.authInfo?.clientId ?? "anonymous"}`,
  });
}

export function remoteEmbeddingRequestState(
  tool: string,
  args: unknown,
  plan: RemoteEmbeddingAuthorizationPlan,
): Omit<RemoteEmbeddingRequestState, "nonce"> {
  return {
    version: 1,
    method: "tools/call",
    tool,
    argumentsFingerprint: fingerprint(args),
    targetFingerprint: plan.target.targetFingerprint,
    disclosureFingerprint: fingerprint(plan.disclosure),
  };
}

export function matchesRemoteEmbeddingRequestState(
  actual: RemoteEmbeddingRequestState | undefined,
  expected: Omit<RemoteEmbeddingRequestState, "nonce">,
): boolean {
  return (
    actual?.version === expected.version &&
    actual.method === expected.method &&
    actual.tool === expected.tool &&
    actual.argumentsFingerprint === expected.argumentsFingerprint &&
    actual.targetFingerprint === expected.targetFingerprint &&
    actual.disclosureFingerprint === expected.disclosureFingerprint
  );
}

function issueState(
  state: Omit<RemoteEmbeddingRequestState, "nonce">,
): RemoteEmbeddingRequestState {
  return {
    ...state,
    nonce: `${Date.now()}-${randomUUID()}`,
  };
}

export function requestPrincipal(token: string | undefined): AuthInfo {
  return {
    token: token ?? "",
    clientId: token ? fingerprint(token) : "loopback-anonymous",
    scopes: ["zvec-grep"],
  };
}

function fingerprint(value: unknown): string {
  return createHash("sha256").update(stableJson(value)).digest("hex");
}

function stableJson(value: unknown): string {
  if (Array.isArray(value)) {
    return `[${value.map(stableJson).join(",")}]`;
  }
  if (value && typeof value === "object") {
    return `{${Object.entries(value as Record<string, unknown>)
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([key, item]) => `${JSON.stringify(key)}:${stableJson(item)}`)
      .join(",")}}`;
  }
  return JSON.stringify(value) ?? "undefined";
}

function validateKey(value: Uint8Array): Uint8Array {
  if (value.byteLength !== REQUEST_STATE_KEY_BYTES) {
    throw new Error("MCP request-state key must be exactly 32 bytes.");
  }
  return value;
}

function isMissingFile(error: unknown): boolean {
  return (
    error instanceof Error &&
    "code" in error &&
    (error as NodeJS.ErrnoException).code === "ENOENT"
  );
}

function isExistingFile(error: unknown): boolean {
  return (
    error instanceof Error &&
    "code" in error &&
    (error as NodeJS.ErrnoException).code === "EEXIST"
  );
}
