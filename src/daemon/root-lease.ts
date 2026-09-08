import { randomUUID } from "node:crypto";
import { mkdir, open, readFile, unlink } from "node:fs/promises";
import { hostname } from "node:os";
import { dirname } from "node:path";
import {
  daemonLeasePath,
  acquireDaemonLeaseGuard,
  daemonLeaseFileIsAbandoned,
  processIsAlive,
  type DaemonLeaseRecord,
} from "../engine/utils/daemon-lease.js";
import { replaceFileAtomically } from "../engine/utils/atomic-file.js";
import { DaemonError } from "./errors.js";

export type RootLease = {
  readonly root: string;
  release(): Promise<void>;
};

type ManagedLease = {
  refs: number;
  record: DaemonLeaseRecord;
  heartbeat?: ReturnType<typeof setInterval>;
  heartbeatInFlight?: Promise<void>;
  stopped: boolean;
};

export class RootLeaseManager {
  readonly instanceToken = randomUUID();
  private readonly leases = new Map<string, ManagedLease>();

  async acquire(root: string): Promise<RootLease> {
    let managed = this.leases.get(root);
    if (!managed) {
      const record = await this.createLease(root);
      managed = {
        refs: 0,
        record,
        stopped: false,
      };
      managed.heartbeat = this.startHeartbeat(root, managed);
      this.leases.set(root, managed);
    }
    managed.refs += 1;
    let released = false;
    return {
      root,
      release: async () => {
        if (released) {
          return;
        }
        released = true;
        await this.release(root, managed!);
      },
    };
  }

  async close(): Promise<void> {
    const leases = [...this.leases.entries()];
    this.leases.clear();
    await Promise.all(
      leases.map(async ([root, managed]) => {
        managed.stopped = true;
        if (managed.heartbeat) clearInterval(managed.heartbeat);
        await managed.heartbeatInFlight;
        await this.removeOwnedLease(root, managed.record);
      }),
    );
  }

  private async createLease(root: string): Promise<DaemonLeaseRecord> {
    const path = daemonLeasePath(root);
    await mkdir(dirname(path), { recursive: true });
    const now = Date.now();
    const record: DaemonLeaseRecord = {
      pid: process.pid,
      hostname: hostname(),
      instanceToken: this.instanceToken,
      createdAt: now,
      updatedAt: now,
    };

    const guard = acquireDaemonLeaseGuard(root, this.instanceToken);
    if (!guard) {
      throw new DaemonError(
        "INDEX_BUSY",
        "Another daemon is changing the root lease.",
        true,
      );
    }
    try {
      try {
        const handle = await open(path, "wx", 0o600);
        try {
          await handle.writeFile(`${JSON.stringify(record)}\n`);
        } finally {
          await handle.close();
        }
        return record;
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "EEXIST") {
          throw error;
        }
        const existing = await readLeaseFile(path);
        if (
          existing?.pid === process.pid &&
          existing.instanceToken === this.instanceToken
        ) {
          return existing;
        }
        if (
          existing &&
          existing.hostname === hostname() &&
          !processIsAlive(existing.pid)
        ) {
          await unlink(path).catch(() => undefined);
          const handle = await open(path, "wx", 0o600);
          try {
            await handle.writeFile(`${JSON.stringify(record)}\n`);
          } finally {
            await handle.close();
          }
          return record;
        }
        if (!existing && daemonLeaseFileIsAbandoned(root)) {
          await unlink(path).catch(() => undefined);
          const handle = await open(path, "wx", 0o600);
          try {
            await handle.writeFile(`${JSON.stringify(record)}\n`);
          } finally {
            await handle.close();
          }
          return record;
        }
        throw new DaemonError(
          "INDEX_BUSY",
          "Another daemon owns index writes for this root.",
          true,
        );
      }
    } finally {
      guard.release();
    }
  }

  private async release(root: string, managed: ManagedLease): Promise<void> {
    managed.refs -= 1;
    if (managed.refs > 0 || this.leases.get(root) !== managed) {
      return;
    }
    this.leases.delete(root);
    managed.stopped = true;
    if (managed.heartbeat) clearInterval(managed.heartbeat);
    await managed.heartbeatInFlight;
    await this.removeOwnedLease(root, managed.record);
  }

  private startHeartbeat(
    root: string,
    managed: ManagedLease,
  ): ReturnType<typeof setInterval> {
    const timer = setInterval(() => {
      if (managed.stopped || managed.heartbeatInFlight) return;
      const heartbeat = (async () => {
        const guard = acquireDaemonLeaseGuard(root, this.instanceToken);
        if (!guard) return;
        try {
          const path = daemonLeasePath(root);
          const current = await readLeaseFile(path);
          if (
            managed.stopped ||
            current?.pid !== managed.record.pid ||
            current.instanceToken !== managed.record.instanceToken
          )
            return;
          const updatedAt = Date.now();
          await replaceFileAtomically(
            path,
            `${JSON.stringify({ ...managed.record, updatedAt })}\n`,
            { mode: 0o600 },
          );
          managed.record.updatedAt = updatedAt;
        } finally {
          guard.release();
        }
      })().catch(() => undefined);
      managed.heartbeatInFlight = heartbeat;
      void heartbeat.finally(() => {
        if (managed.heartbeatInFlight === heartbeat)
          managed.heartbeatInFlight = undefined;
      });
    }, 5_000);
    timer.unref?.();
    return timer;
  }

  private async removeOwnedLease(
    root: string,
    expected: DaemonLeaseRecord,
  ): Promise<void> {
    let guard;
    for (let attempt = 0; attempt < 10 && !guard; attempt++) {
      guard = acquireDaemonLeaseGuard(root, this.instanceToken);
      if (!guard) {
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
    }
    if (!guard) {
      throw new DaemonError(
        "INDEX_BUSY",
        "Could not remove the owned daemon lease.",
        true,
      );
    }
    const path = daemonLeasePath(root);
    try {
      const current = await readLeaseFile(path);
      if (
        current?.pid === expected.pid &&
        current.instanceToken === expected.instanceToken
      ) {
        await unlink(path).catch(() => undefined);
      }
    } finally {
      guard.release();
    }
  }
}

async function readLeaseFile(
  path: string,
): Promise<DaemonLeaseRecord | undefined> {
  try {
    const parsed = JSON.parse(
      await readFile(path, "utf8"),
    ) as Partial<DaemonLeaseRecord>;
    if (
      typeof parsed.pid !== "number" ||
      typeof parsed.hostname !== "string" ||
      typeof parsed.instanceToken !== "string" ||
      typeof parsed.createdAt !== "number" ||
      typeof parsed.updatedAt !== "number"
    ) {
      return undefined;
    }
    return parsed as DaemonLeaseRecord;
  } catch {
    return undefined;
  }
}
