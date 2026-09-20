export const ZVEC_GREP_WORKSPACE_EVIDENCE_RULES = [
  "Use the current workspace as the evidence source when the user asks about local material, prior context establishes it as relevant, or the question concerns how the current project works—even if the workspace is not mentioned explicitly.",
  "A workspace may contain any mix of code, documents, configuration, and data.",
  "Do not use workspace retrieval for unrelated open-world questions, current external facts, or web content that does not depend on local evidence.",
] as const;

export function formatPromptRules(
  heading: string,
  rules: readonly string[],
): string {
  return [heading, ...rules.map((rule) => `- ${rule}`)].join("\n");
}
