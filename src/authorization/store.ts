import {
  createHmac,
  randomBytes,
  randomUUID,
  timingSafeEqual,
} from "node:crypto";
import { chmod, mkdir, readFile, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { readJsonFile, writeJsonFile } from "../engine/utils/json.js";
import { acquireReadWriteLock } from "../engine/utils/lock.js";
import {
  REMOTE_EMBEDDING_CAPABILITY,
  type RemoteEmbeddingAuthorizationDocument,
  type RemoteEmbeddingAuthorizationStatus,
  type RemoteEmbeddingTarget,
  type RemoteEmbeddingWorkspaceGrant,
} from "./types.js";

const DOCUMENT_VERSION = 1 as const;
const GRANT_FILE = "authorization.json";

export type RemoteEmbeddingAuthorizationStoreOptions = {
  signingKeyPath?: string;
};

export class RemoteEmbeddingAuthorizationStore {
  private readonly signingKeyPath: string;

  constructor(options: RemoteEmbeddingAuthorizationStoreOptions = {}) {
    this.signingKeyPath = resolve(
      options.signingKeyPath ??
        process.env.ZVEC_GREP_AUTHORIZATION_KEY_FILE ??
        join(homedir(), ".zvec-grep", "authorization-signing.key"),
    );
  }

  grantPath(target: Pick<RemoteEmbeddingTarget, "workspaceRoots">): string {
    const root = target.workspaceRoots[0];
    if (!root) {
      throw new Error("Remote Embedding target has no workspace roots.");
    }
    return join(root, ".zvec-grep", GRANT_FILE);
  }

  async hasGrant(target: RemoteEmbeddingTarget): Promise<boolean> {
    const key = await this.readSigningKey();
    if (!key) return false;
    const document = await this.readDocument(this.grantPath(target));
    return document.grants.some(
      (grant) =>
        grant.targetFingerprint === target.targetFingerprint &&
        this.verifyGrant(grant, key),
    );
  }

  async grant(
    target: RemoteEmbeddingTarget,
  ): Promise<RemoteEmbeddingWorkspaceGrant> {
    const key = await this.getOrCreateSigningKey();
    const unsigned = {
      version: DOCUMENT_VERSION,
      id: randomUUID(),
      capability: REMOTE_EMBEDDING_CAPABILITY,
      scope: "workspace" as const,
      workspaceRoots: [...target.workspaceRoots],
      workspaceFingerprint: target.workspaceFingerprint,
      provider: target.provider,
      model: target.model,
      endpoint: target.endpoint,
      targetFingerprint: target.targetFingerprint,
      grantedAt: Date.now(),
    };
    const grant: RemoteEmbeddingWorkspaceGrant = {
      ...unsigned,
      signature: sign(unsigned, key),
    };
    for (const root of target.workspaceRoots) {
      const path = join(root, ".zvec-grep", GRANT_FILE);
      await this.withDocumentWrite(path, async (document) => {
        const next: RemoteEmbeddingAuthorizationDocument = {
          version: DOCUMENT_VERSION,
          grants: [
            ...document.grants.filter(
              (candidate) =>
                candidate.targetFingerprint !== target.targetFingerprint,
            ),
            grant,
          ],
        };
        await this.writeDocument(path, next);
      });
    }
    return grant;
  }

  async revoke(target: RemoteEmbeddingTarget): Promise<boolean> {
    let revoked = false;
    for (const root of target.workspaceRoots) {
      const path = join(root, ".zvec-grep", GRANT_FILE);
      await this.withDocumentWrite(path, async (document) => {
        const grants = document.grants.filter(
          (grant) => grant.targetFingerprint !== target.targetFingerprint,
        );
        if (grants.length === document.grants.length) return;
        revoked = true;
        await this.writeDocument(path, {
          version: DOCUMENT_VERSION,
          grants,
        });
      });
    }
    return revoked;
  }

  async revokeAll(root: string): Promise<number> {
    const path = join(resolve(root), ".zvec-grep", GRANT_FILE);
    let revoked = 0;
    let validGrants: RemoteEmbeddingWorkspaceGrant[] = [];
    const key = await this.readSigningKey();
    await this.withDocumentWrite(path, async (document) => {
      revoked = document.grants.length;
      if (revoked === 0) return;
      validGrants = key
        ? document.grants.filter((grant) => this.verifyGrant(grant, key))
        : [];
      await this.writeDocument(path, {
        version: DOCUMENT_VERSION,
        grants: [],
      });
    });
    const fingerprintsByPath = new Map<string, Set<string>>();
    for (const grant of validGrants) {
      for (const workspaceRoot of grant.workspaceRoots) {
        const grantPath = join(workspaceRoot, ".zvec-grep", GRANT_FILE);
        if (resolve(grantPath) === resolve(path)) continue;
        const fingerprints = fingerprintsByPath.get(grantPath) ?? new Set();
        fingerprints.add(grant.targetFingerprint);
        fingerprintsByPath.set(grantPath, fingerprints);
      }
    }
    for (const [grantPath, fingerprints] of fingerprintsByPath) {
      await this.withDocumentWrite(grantPath, async (document) => {
        const grants = document.grants.filter(
          (grant) => !fingerprints.has(grant.targetFingerprint),
        );
        if (grants.length === document.grants.length) return;
        await this.writeDocument(grantPath, {
          version: DOCUMENT_VERSION,
          grants,
        });
      });
    }
    return revoked;
  }

  async status(root: string): Promise<RemoteEmbeddingAuthorizationStatus> {
    const path = join(resolve(root), ".zvec-grep", GRANT_FILE);
    const document = await this.readDocument(path);
    const key = await this.readSigningKey();
    return {
      path,
      grants: document.grants.map(({ signature, ...grant }) => ({
        ...grant,
        valid: key ? this.verifyGrant({ ...grant, signature }, key) : false,
      })),
    };
  }

  private async readDocument(
    path: string,
  ): Promise<RemoteEmbeddingAuthorizationDocument> {
    const value = await readJsonFile<unknown>(path, {
      version: DOCUMENT_VERSION,
      grants: [],
    });
    if (!isAuthorizationDocument(value)) {
      return { version: DOCUMENT_VERSION, grants: [] };
    }
    return value;
  }

  private async writeDocument(
    path: string,
    document: RemoteEmbeddingAuthorizationDocument,
  ): Promise<void> {
    await writeJsonFile(path, document, {
      directoryMode: 0o700,
      fileMode: 0o600,
    });
  }

  private async withDocumentWrite<T>(
    path: string,
    operation: (document: RemoteEmbeddingAuthorizationDocument) => Promise<T>,
  ): Promise<T> {
    const lock = acquireReadWriteLock(
      join(dirname(path), "authorization-store"),
      "write",
      { operation: "remote-embedding-authorization" },
    );
    try {
      return await operation(await this.readDocument(path));
    } finally {
      lock.release();
    }
  }

  private verifyGrant(
    grant: RemoteEmbeddingWorkspaceGrant,
    key: Buffer,
  ): boolean {
    const { signature, ...unsigned } = grant;
    const expected = sign(unsigned, key);
    const actualBytes = Buffer.from(signature, "hex");
    const expectedBytes = Buffer.from(expected, "hex");
    return (
      actualBytes.length === expectedBytes.length &&
      timingSafeEqual(actualBytes, expectedBytes)
    );
  }

  private async readSigningKey(): Promise<Buffer | undefined> {
    try {
      const value = (await readFile(this.signingKeyPath, "utf8")).trim();
      return value ? Buffer.from(value, "hex") : undefined;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined;
      throw error;
    }
  }

  private async getOrCreateSigningKey(): Promise<Buffer> {
    const existing = await this.readSigningKey();
    if (existing) return existing;
    await mkdir(dirname(this.signingKeyPath), { recursive: true, mode: 0o700 });
    await chmod(dirname(this.signingKeyPath), 0o700);
    const key = randomBytes(32);
    try {
      await writeFile(this.signingKeyPath, key.toString("hex"), {
        encoding: "utf8",
        mode: 0o600,
        flag: "wx",
      });
      return key;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
      const concurrent = await this.readSigningKey();
      if (!concurrent) throw error;
      return concurrent;
    }
  }
}

function sign(value: object, key: Buffer): string {
  return createHmac("sha256", key).update(stableStringify(value)).digest("hex");
}

function stableStringify(value: unknown): string {
  if (Array.isArray(value)) {
    return `[${value.map(stableStringify).join(",")}]`;
  }
  if (value && typeof value === "object") {
    const record = value as Record<string, unknown>;
    return `{${Object.keys(record)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${stableStringify(record[key])}`)
      .join(",")}}`;
  }
  return JSON.stringify(value);
}

function isAuthorizationDocument(
  value: unknown,
): value is RemoteEmbeddingAuthorizationDocument {
  if (!value || typeof value !== "object") return false;
  const document = value as Record<string, unknown>;
  if (
    document.version !== DOCUMENT_VERSION ||
    !Array.isArray(document.grants)
  ) {
    return false;
  }
  return document.grants.every(isWorkspaceGrant);
}

function isWorkspaceGrant(
  value: unknown,
): value is RemoteEmbeddingWorkspaceGrant {
  if (!value || typeof value !== "object") return false;
  const grant = value as Record<string, unknown>;
  return (
    grant.version === DOCUMENT_VERSION &&
    grant.capability === REMOTE_EMBEDDING_CAPABILITY &&
    grant.scope === "workspace" &&
    Array.isArray(grant.workspaceRoots) &&
    grant.workspaceRoots.every((root) => typeof root === "string") &&
    [
      "id",
      "workspaceFingerprint",
      "provider",
      "model",
      "endpoint",
      "targetFingerprint",
      "signature",
    ].every((key) => typeof grant[key] === "string") &&
    typeof grant.grantedAt === "number"
  );
}
