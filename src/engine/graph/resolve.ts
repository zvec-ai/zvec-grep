import type { NameIndex } from "./name-index.js";
import {
  referenceResolutionPolicy,
  type AnalyzedReference,
  type ReferenceBindingMatch,
} from "./reference-resolution-policy.js";
import type { PendingRef, RefResolveResult } from "./types.js";

export function resolveRef(
  ref: PendingRef,
  names: NameIndex,
  preferredFileIds: readonly string[] = [],
  binding?: ReferenceBindingMatch,
  sourceContainerName?: string,
  sourceContainerId?: string,
  hierarchyContainerIds: readonly string[] = [],
  analyzedReference?: AnalyzedReference,
): RefResolveResult {
  const reference =
    analyzedReference ??
    referenceResolutionPolicy.analyzeReference(
      ref.target ?? ref.ref_name,
      ref.source_language,
    );
  const context = referenceResolutionPolicy.createContext(reference, {
    sourceFileId: ref.src_file,
    ownerContainerId: sourceContainerId,
    preferredFileIds,
    binding,
    ownerContainerName: sourceContainerName,
  });
  const plan = referenceResolutionPolicy.lookupPlan(context);
  const containerNames =
    plan.containerScope.kind === "named" ? [plan.containerScope.name] : [];
  const containerIds =
    plan.containerScope.kind === "owner-hierarchy"
      ? hierarchyContainerIds.length > 0
        ? hierarchyContainerIds
        : ["__unresolved_container__"]
      : [];
  const hit = names.lookupWithEvidence(
    plan.lookupName,
    ref.src_file,
    plan.preferredFileIds,
    plan.allowBareFallback,
    containerNames,
    containerIds,
  );
  if (!hit)
    return referenceResolutionPolicy.isExternal(reference)
      ? { status: "external" }
      : { status: "failed" };

  const edgeKind =
    ref.ref_kind === "extends" ||
    ref.ref_kind === "implements" ||
    ref.ref_kind === "overrides"
      ? "INHERITS"
      : ref.ref_kind === "call" || ref.ref_kind === "new"
        ? "CALLS"
        : "REFS";

  return {
    status: "resolved",
    dst: hit.entry.id,
    edgeKind,
    evidence: hit.evidence,
  };
}
