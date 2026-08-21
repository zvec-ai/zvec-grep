import { createHash } from "node:crypto";

/** Ref.id = owner + "#" + sha1(ref_name, ref_kind, line, occurrence). */
export function makeRefId(
  owner: string,
  refName: string,
  refKind: string,
  line: number,
  occurrence = 0,
): string {
  const digest = createHash("sha1")
    .update(refName)
    .update("\0")
    .update(refKind)
    .update("\0")
    .update(String(line))
    .update("\0")
    .update(String(occurrence))
    .digest("hex")
    .slice(0, 16);
  return `${owner}#${digest}`;
}
