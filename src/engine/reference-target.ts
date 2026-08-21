export type ReferenceReceiverTarget = {
  kind: "owner" | "super" | "qualified";
  name: string;
};

export type ReferenceResolutionHints = {
  receiverType?: string;
  candidateTypes?: string[];
  genericBounds?: string[];
  dispatch?: "static" | "virtual" | "interface" | "trait" | "dynamic";
  callArity?: number;
};

export type ReferenceTarget = {
  raw: string;
  member: string;
  receiver?: ReferenceReceiverTarget;
  /** Optional semantic facts supplied by language-specific analysis. */
  hints?: ReferenceResolutionHints;
};

const OWNER_RECEIVERS = new Set(["this", "self", "cls"]);

/** Build a structured target while the source-language syntax is available. */
export function referenceTargetFromSyntax(raw: string): ReferenceTarget {
  const normalized = raw.replace(/->|::/g, ".");
  const separator = normalized.lastIndexOf(".");
  if (separator < 0) return { raw, member: normalized };
  const receiverRaw = normalized.slice(0, separator).replace(/\(\)$/, "");
  const member = normalized.slice(separator + 1);
  const kind =
    receiverRaw === "super"
      ? "super"
      : OWNER_RECEIVERS.has(receiverRaw)
        ? "owner"
        : "qualified";
  return { raw, member, receiver: { kind, name: receiverRaw } };
}

/** Compatibility fallback for persisted references created before target IR. */
export const referenceTargetFromRaw = referenceTargetFromSyntax;

export function memberReferenceTarget(
  raw: string,
  receiver: string,
  member: string,
): ReferenceTarget {
  const parsed = referenceTargetFromSyntax(`${receiver}.${member}`);
  return { ...parsed, raw };
}
