import { createHash } from "node:crypto";
import { spawn } from "node:child_process";
import { createReadStream } from "node:fs";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve, relative, isAbsolute } from "node:path";

export const sha256 = (value) =>
  createHash("sha256").update(value).digest("hex");
export const readJson = async (path) =>
  JSON.parse(await readFile(path, "utf8"));
export async function writeJson(path, value) {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, `${JSON.stringify(value, null, 2)}\n`);
}
export function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.keys(value)
        .sort()
        .map((key) => [key, canonical(value[key])]),
    );
  }
  return value;
}
export const objectHash = (value) => sha256(JSON.stringify(canonical(value)));
export async function fileHash(path) {
  const hash = createHash("sha256");
  for await (const part of createReadStream(path)) hash.update(part);
  return hash.digest("hex");
}
export function inside(root, path) {
  const rel = relative(resolve(root), resolve(path));
  return (
    rel === "" ||
    (!rel.startsWith(`..${process.platform === "win32" ? "\\" : "/"}`) &&
      rel !== ".." &&
      !isAbsolute(rel))
  );
}
export function run(
  command,
  args,
  { cwd, env = process.env, timeout = 600_000, log, onSpawn } = {},
) {
  return new Promise((resolvePromise, reject) => {
    const child = spawn(command, args, {
      cwd,
      env,
      stdio: ["ignore", "pipe", "pipe"],
    });
    onSpawn?.(child);
    let stdout = "",
      stderr = "",
      timedOut = false;
    child.stdout.on("data", (part) => {
      stdout += part;
      log?.(String(part));
    });
    child.stderr.on("data", (part) => {
      stderr += part;
      log?.(String(part));
    });
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill("SIGKILL");
    }, timeout);
    child.on("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.on("close", (code, signal) => {
      clearTimeout(timer);
      const result = { stdout, stderr, code, signal, timed_out: timedOut };
      if (code === 0) resolvePromise(result);
      else
        reject(
          Object.assign(
            new Error(
              `${command} ${args[0] ?? ""} failed (${timedOut ? "timeout" : code}): ${stderr.slice(-3000)}`,
            ),
            { result },
          ),
        );
    });
  });
}
