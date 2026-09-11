import { createHash, randomUUID } from "node:crypto";
import { appendFile, chmod, mkdir, rename, rm, stat } from "node:fs/promises";
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
          await mkdir(dirname(path), { recursive: true, mode: 0o700 });
          const line = `${record}\n`;
          const size = await stat(path)
            .then((entry) => entry.size)
            .catch((error) => {
              if (error.code === "ENOENT") return 0;
              throw error;
            });
          // Keep each JSON record intact, even when one record exceeds the limit.
          if (size > 0 && size + Buffer.byteLength(line, "utf8") > maxBytes) {
            if (keep === 0) {
              await rm(path);
            } else {
              await rm(`${path}.${keep}`, { force: true });
              for (let index = keep - 1; index >= 1; index--) {
                await rename(`${path}.${index}`, `${path}.${index + 1}`).catch(
                  (error) => {
                    if (error.code !== "ENOENT") throw error;
                  },
                );
              }
              await rename(path, `${path}.1`);
            }
          }
          await appendFile(path, line, {
            encoding: "utf8",
            mode: 0o600,
          });
          await chmod(path, 0o600);
        })
        .catch(() => undefined);
    },
    flush: () => tail,
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
