import { createHash, randomUUID } from "node:crypto";
import { chmod, mkdir, readdir, rm } from "node:fs/promises";
import { finished } from "node:stream/promises";
import { createStream, type RotatingFileStream } from "rotating-file-stream";
import { dirname, join } from "node:path";
import type { ZvecGrepLogConfig } from "../engine/config.js";
import { daemonHome } from "./config.js";
import { currentTraceContext } from "../observability/trace-context.js";

export type LogFields = Record<string, string | number | boolean | undefined>;

export type DaemonLogger = {
  event(name: string, fields?: LogFields, level?: "info" | "debug"): void;
  flush(): Promise<void>;
};

export function createDaemonLogger(
  home?: string,
  options: ZvecGrepLogConfig = {},
): DaemonLogger {
  const maxBytes = options.maxBytes ?? 10 * 1024 * 1024;
  const keep = options.keep ?? 5;
  const level = options.level ?? "info";
  const path = join(daemonHome(home), "logs", "server.log");
  let tail = Promise.resolve();
  let stream: RotatingFileStream | undefined;

  async function openStream(): Promise<RotatingFileStream> {
    if (stream && !stream.destroyed) return stream;
    await mkdir(dirname(path), { recursive: true, mode: 0o700 });
    // Classical rotation does not remove backups beyond a reduced retention limit.
    for (const name of await readdir(dirname(path))) {
      const match = /^server\.log\.([1-9]\d*)$/.exec(name);
      if (match && Number(match[1]) > keep) await rm(join(dirname(path), name));
    }
    await chmod(path, 0o600).catch((error) => {
      if (error.code !== "ENOENT") throw error;
    });
    stream = createStream("server.log", {
      path: dirname(path),
      size: `${maxBytes}B`,
      rotate: Math.max(1, keep),
      mode: 0o600,
    });
    // Logging failures must not become unhandled stream errors in the daemon.
    stream.on("error", () => undefined);
    return stream;
  }

  return {
    event(name, fields = {}, eventLevel = "info") {
      if (eventLevel === "debug" && level !== "debug") return;
      const trace = currentTraceContext();
      const record = JSON.stringify({
        timestamp: new Date().toISOString(),
        event: name,
        level: eventLevel,
        ...(trace ? { trace_id: trace.traceId } : {}),
        ...sanitizeFields(fields),
      });
      tail = tail
        .then(async () => {
          const output = await openStream();
          // One write per JSON record keeps oversized and UTF-8 records intact.
          await new Promise<void>((resolve, reject) => {
            output.write(`${record}\n`, (error) =>
              error ? reject(error) : resolve(),
            );
          });
          // The library requires at least one backup; zero retention discards it.
          if (keep === 0) await rm(`${path}.1`, { force: true });
        })
        .catch(() => undefined);
    },
    flush() {
      tail = tail
        .then(async () => {
          const output = stream;
          stream = undefined;
          if (!output) return;
          const completion = finished(output, { cleanup: true });
          output.end();
          await completion;
        })
        .catch(() => undefined);
      return tail;
    },
  };
}

export function rootIdentity(root: string): string {
  return createHash("sha256").update(root).digest("hex").slice(0, 16);
}

export function opaqueIdentity(value: string): string {
  return createHash("sha256").update(value).digest("hex").slice(0, 16);
}

export function requestId(): string {
  return randomUUID();
}

function sanitizeFields(fields: LogFields): LogFields {
  const safe: LogFields = {};
  for (const [key, value] of Object.entries(fields)) {
    if (value === undefined || /token|api.?key|authorization|query/i.test(key))
      continue;
    safe[key] =
      typeof value === "string" && value.length > 512
        ? `${value.slice(0, 512)}…`
        : value;
  }
  return safe;
}
