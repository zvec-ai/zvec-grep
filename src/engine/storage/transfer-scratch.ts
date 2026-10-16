import { randomUUID } from "node:crypto";
import {
  lstatSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { hostname, tmpdir } from "node:os";
import { basename, join, resolve } from "node:path";
import { EngineError } from "../errors.js";

const MARKER = ".zvec-transfer-owner.json";
const PREFIX = "zg-portability-process-";
type Owner = {
  kind: "zvec-grep-transfer-scratch";
  version: 1;
  path: string;
  token: string;
  hostname: string;
  uid: number | null;
  pid: number;
  childPid: number | null;
  device: number;
  inode: number;
};

function unsafe(message: string): never {
  throw new EngineError(message, {
    code: "ZVEC_GREP.ENGINE.TRANSFER_SCRATCH_UNSAFE",
  });
}

/** Only the private process area is owned. Destinations and their locks are not. */
export function createTransferScratch(): {
  path: string;
  setChildPid(pid: number): void;
  cleanup(): void;
} {
  const path = realpathSync(mkdtempSync(join(tmpdir(), PREFIX)));
  const stat = lstatSync(path);
  const owner: Owner = {
    kind: "zvec-grep-transfer-scratch",
    version: 1,
    path,
    token: randomUUID(),
    hostname: hostname(),
    uid: process.getuid?.() ?? null,
    pid: process.pid,
    childPid: null,
    device: stat.dev,
    inode: stat.ino,
  };
  try {
    writeOwner(owner);
  } catch (error) {
    rmSync(path, { recursive: true, force: true });
    throw error;
  }
  return {
    path,
    setChildPid(pid) {
      const actual = readOwner(path);
      assertSameOwner(actual, owner);
      if (!Number.isSafeInteger(pid) || pid <= 0)
        unsafe("Invalid transfer child identity.");
      // Commit the child identity before sending any start message. A parent
      // killed before this point cannot have started a native source copy.
      writeOwner({ ...owner, childPid: pid });
      owner.childPid = pid;
    },
    cleanup() {
      removeOwned(path, owner);
    },
  };
}

/** Explicit operator recovery. Never remove an unrecorded or live directory. */
export function recoverTransferScratch(input: string): void {
  const requested = resolve(input);
  // Parent aliases such as macOS /var -> /private/var are valid spellings.
  // Reject a link at the scratch entry itself before resolving its parents.
  const entry = lstatSync(requested);
  if (!entry.isDirectory() || entry.isSymbolicLink())
    unsafe("Not an owned transfer scratch directory.");
  const path = realpathSync(requested);
  const owner = readOwner(path);
  for (const pid of [owner.pid, owner.childPid]) {
    if (pid !== null && !processIsGone(pid))
      unsafe(
        `Transfer process ${pid} is active or cannot be checked; temporary data was retained.`,
      );
  }
  removeOwned(path, owner);
}

function processIsGone(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return false;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "ESRCH";
  }
}

function writeOwner(owner: Owner): void {
  const pending = join(owner.path, `${MARKER}.${owner.token}.new`);
  writeFileSync(pending, `${JSON.stringify(owner)}\n`, {
    mode: 0o600,
    flag: "wx",
  });
  try {
    renameSync(pending, join(owner.path, MARKER));
  } finally {
    rmSync(pending, { force: true });
  }
}

function readOwner(path: string): Owner {
  const stat = lstatSync(path);
  if (
    !stat.isDirectory() ||
    stat.isSymbolicLink() ||
    !basename(path).startsWith(PREFIX) ||
    realpathSync(path) !== path
  )
    unsafe("Not an owned transfer scratch directory.");
  const uid = process.getuid?.() ?? null;
  if (uid !== null && (stat.uid !== uid || (stat.mode & 0o077) !== 0))
    unsafe("Transfer scratch owner or permissions do not match.");
  const marker = join(path, MARKER),
    markerStat = lstatSync(marker);
  if (
    !markerStat.isFile() ||
    markerStat.isSymbolicLink() ||
    markerStat.nlink !== 1 ||
    (uid !== null && markerStat.uid !== uid)
  )
    unsafe("Unsafe transfer ownership record.");
  const owner = JSON.parse(readFileSync(marker, "utf8")) as Owner;
  if (
    owner.kind !== "zvec-grep-transfer-scratch" ||
    owner.version !== 1 ||
    owner.path !== path ||
    owner.hostname !== hostname() ||
    owner.uid !== uid ||
    owner.device !== stat.dev ||
    owner.inode !== stat.ino ||
    typeof owner.token !== "string" ||
    !/^[0-9a-f-]{36}$/.test(owner.token) ||
    !Number.isSafeInteger(owner.pid) ||
    owner.pid <= 0 ||
    (owner.childPid !== null &&
      (!Number.isSafeInteger(owner.childPid) || owner.childPid <= 0))
  )
    unsafe("Transfer ownership record does not match this directory and host.");
  return owner;
}

function removeOwned(path: string, owner: Owner): void {
  // Recheck immediately before deletion. rm does not follow directory symlinks.
  // The private (0700) directory belongs to this user, and the identity record
  // must match its original device and inode as well as the random token.
  const actual = readOwner(path);
  assertSameOwner(actual, owner);
  rmSync(path, { recursive: true, force: false });
}

function assertSameOwner(actual: Owner, owner: Owner): void {
  if (
    actual.token !== owner.token ||
    actual.device !== owner.device ||
    actual.inode !== owner.inode ||
    actual.pid !== owner.pid ||
    actual.childPid !== owner.childPid
  )
    unsafe("Transfer scratch identity changed; temporary data was retained.");
}
